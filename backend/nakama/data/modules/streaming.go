package main

import (
	"context"
	"database/sql"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// ---------------------------------------------------------------------------
// Types & constants
// ---------------------------------------------------------------------------

type StartStreamRequest struct {
	CrewID      string `json:"crew_id"`
	Title       string `json:"title,omitempty"`
	SupportsAV1 bool   `json:"supports_av1,omitempty"`
	Width       uint32 `json:"width,omitempty"`
	Height      uint32 `json:"height,omitempty"`
	BitrateKbps uint32 `json:"bitrate_kbps,omitempty"`
	// Exe is the game executable name (e.g. "Heaven.exe"), for logging and
	// policy-mismatch diagnostics only.
	Exe string `json:"exe,omitempty"`
	// IgdbID is the catalogue identity of the captured process, for logging
	// only. The client supplies it and the server cannot verify it. The
	// client matches it against the capture block lists.
	IgdbID uint32 `json:"igdb_id,omitempty"`
	// PreferredRegion is optional: an SFU region from sfu_routing.go for the
	// host's session. Viewers join the host's region (watch_stream). An empty
	// or unknown value falls back (selectSFURegion).
	PreferredRegion string `json:"preferred_region,omitempty"`
	// ResumeSessionID re-advertises a stream that the host's SFU session
	// still carries, after a Nakama realtime reconnect removed its records
	// (OnSessionEnd). The server restores only what it recorded for this
	// host and this session; it does not start a new session.
	ResumeSessionID string `json:"resume_session_id,omitempty"`
	// Fps is the stream's frame rate, for the stream_started push.
	Fps uint32 `json:"fps,omitempty"`
	// Advertise "on_live" defers the advertisement: start_stream signs the
	// host token and records the session, and stream_live advertises it
	// when the host's track is open (streaming-quality stage 6). Empty
	// advertises at once, as before (older clients).
	Advertise string `json:"advertise,omitempty"`
}

// advertiseOnLive is the StartStreamRequest.Advertise value that defers the
// advertisement to stream_live.
const advertiseOnLive = "on_live"

// CapturePolicy is the backend `capture` block in the start_stream response
// (streaming-reliability plan §8). Three gates must all hold before the client
// hooks a game: this block allows it, the catalogue policy is `hook`, and the
// runtime checks pass. With no block, or with hook_enabled=false, the client
// never hooks.
//
// The lists carry catalogue identities (IGDB ids), which the client resolves
// from the process it captures. Deny wins over allow; an id on neither list is
// never hooked. hook_review games appear in neither list. Counter-Strike 2
// (242408) is in hook_deny_ids permanently.
//
// Source data: mello-backlog/plans/game-capture-hook-games/games.csv
// (policy==hook -> hook_allow_ids, policy==no_hook -> hook_deny_ids). Stored
// as a versioned blob in Nakama storage so exposure is controlled from the
// backend, not by who installed what. See loadCapturePolicy. The admin tool
// edits it through admin_capture_policy_set.
type CapturePolicy struct {
	HookEnabled   bool     `json:"hook_enabled"`
	PolicyVersion string   `json:"policy_version"`
	HookAllowIDs  []uint32 `json:"hook_allow_ids"`
	HookDenyIDs   []uint32 `json:"hook_deny_ids"`
}

const (
	CapturePolicyCollection = "capture_config"
	CapturePolicyKey        = "hook_policy"
	// CS2IgdbID is Counter-Strike 2. It is denied permanently: Trusted Mode
	// blocks third-party DLLs and Valve keeps no allowlist.
	CS2IgdbID uint32 = 242408
)

// defaultCapturePolicy is the safe default: no hook until a policy is stored.
func defaultCapturePolicy() CapturePolicy {
	return CapturePolicy{
		HookEnabled:   false,
		PolicyVersion: "none",
		HookAllowIDs:  []uint32{},
		HookDenyIDs:   []uint32{},
	}
}

// parseCapturePolicy parses a stored policy blob. Unknown fields are ignored
// and missing lists become empty, so an old blob never enables a hook by
// accident and never fails the stream start.
func parseCapturePolicy(raw string) CapturePolicy {
	policy := defaultCapturePolicy()
	if raw == "" {
		return policy
	}
	var stored CapturePolicy
	if err := json.Unmarshal([]byte(raw), &stored); err != nil {
		return policy
	}
	policy.HookEnabled = stored.HookEnabled
	policy.PolicyVersion = stored.PolicyVersion
	if stored.HookAllowIDs != nil {
		policy.HookAllowIDs = stored.HookAllowIDs
	}
	if stored.HookDenyIDs != nil {
		policy.HookDenyIDs = stored.HookDenyIDs
	}
	return policy
}

// loadCapturePolicy reads the versioned hook policy from Nakama storage.
// Any failure (missing object, corrupt JSON, storage error) returns the safe
// default, so a stream never fails because the policy is unreadable — it just
// runs without the hook. The client treats a missing block the same way.
func loadCapturePolicy(ctx context.Context, nk runtime.NakamaModule) CapturePolicy {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: CapturePolicyCollection, Key: CapturePolicyKey, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return defaultCapturePolicy()
	}
	return parseCapturePolicy(objects[0].GetValue())
}

const (
	MaxSFUViewers    = 100
	StreamSessionCol = "stream_sessions"
	// StreamResumeCollection holds, per host user, the records that the
	// session-end cleanup removed, so the host can re-advertise the same
	// stream after a Nakama reconnect. Server-only.
	StreamResumeCollection = "stream_resume"
	// streamResumeWindow is how long after the cleanup a host can
	// re-advertise its stream.
	streamResumeWindow = 10 * time.Minute
)

// streamResumeRecord is what cleanupStreamIfHost removed for one stream.
type streamResumeRecord struct {
	Meta          StreamMeta        `json:"meta"`
	Session       StreamSessionMeta `json:"session"`
	LedgerEventID string            `json:"ledger_event_id"`
	EndedAt       int64             `json:"ended_at"`
}

type StopStreamRequest struct {
	CrewID string `json:"crew_id"`
	// SessionID is optional. With it, a stream that was never advertised
	// (advertise "on_live", no stream_live yet) is removed too.
	SessionID string `json:"session_id,omitempty"`
}

type ActiveStream struct {
	HostID      string `json:"host_id"`
	HostName    string `json:"host_name"`
	Title       string `json:"title"`
	StartedAt   int64  `json:"started_at"`
	Width       uint32 `json:"width,omitempty"`
	Height      uint32 `json:"height,omitempty"`
	BitrateKbps uint32 `json:"bitrate_kbps,omitempty"`
	Fps         uint32 `json:"fps,omitempty"`
}

// StreamMeta is the extended metadata stored in stream_meta/{crew_id}.
type StreamMeta struct {
	StreamID           string `json:"stream_id"`
	CrewID             string `json:"crew_id"`
	StreamerID         string `json:"streamer_id"`
	StreamerUsername   string `json:"streamer_username"`
	Title              string `json:"title"`
	StartedAt          string `json:"started_at"`
	ThumbnailURL       string `json:"thumbnail_url,omitempty"`
	ThumbnailUpdatedAt string `json:"thumbnail_updated_at,omitempty"`
	// ViewerIDs are the viewers that watch now: watch_stream adds one,
	// stream_viewer_left (from the host, on the SFU member_left) removes it.
	// crew_state viewer_count is its length.
	ViewerIDs []string `json:"viewer_ids,omitempty"`
	// WatchedIDs are all the viewers that watched, for the session ledger.
	WatchedIDs []string `json:"watched_ids,omitempty"`
	// PeakViewers is the largest len(ViewerIDs) so far.
	PeakViewers int    `json:"peak_viewers,omitempty"`
	Width       uint32 `json:"width,omitempty"`
	Height      uint32 `json:"height,omitempty"`
	BitrateKbps uint32 `json:"bitrate_kbps,omitempty"`
	Fps         uint32 `json:"fps,omitempty"`
}

// peakViewers is the largest number of viewers at one time.
func (m *StreamMeta) peakViewers() int {
	if len(m.ViewerIDs) > m.PeakViewers {
		return len(m.ViewerIDs)
	}
	return m.PeakViewers
}

// allViewers is every viewer that watched: WatchedIDs, plus any current
// viewer missing from it (records written before WatchedIDs existed).
func (m *StreamMeta) allViewers() []string {
	out := append([]string(nil), m.WatchedIDs...)
	for _, id := range m.ViewerIDs {
		if !containsString(out, id) {
			out = append(out, id)
		}
	}
	return out
}

// addViewer counts viewerID in. It reports whether anything changed.
func (m *StreamMeta) addViewer(viewerID string) bool {
	changed := false
	if !containsString(m.ViewerIDs, viewerID) {
		m.ViewerIDs = append(m.ViewerIDs, viewerID)
		changed = true
	}
	if !containsString(m.WatchedIDs, viewerID) {
		m.WatchedIDs = append(m.WatchedIDs, viewerID)
		changed = true
	}
	if len(m.ViewerIDs) > m.PeakViewers {
		m.PeakViewers = len(m.ViewerIDs)
		changed = true
	}
	return changed
}

// removeViewer counts viewerID out. It reports whether it was counted.
func (m *StreamMeta) removeViewer(viewerID string) bool {
	for i, id := range m.ViewerIDs {
		if id == viewerID {
			m.ViewerIDs = append(m.ViewerIDs[:i], m.ViewerIDs[i+1:]...)
			return true
		}
	}
	return false
}

func containsString(list []string, s string) bool {
	for _, v := range list {
		if v == s {
			return true
		}
	}
	return false
}

// updateStreamMeta applies change to stream_meta/{crewID} with optimistic
// concurrency: viewers join and leave at the same time. change returns
// false when nothing must be written. A missing record is not an error.
func updateStreamMeta(ctx context.Context, nk runtime.NakamaModule, crewID string, change func(*StreamMeta) bool) error {
	for attempt := 0; attempt < 3; attempt++ {
		objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
			{Collection: StreamMetaCollection, Key: crewID, UserID: SystemUserID},
		})
		if err != nil {
			return err
		}
		if len(objects) == 0 {
			return nil
		}
		var meta StreamMeta
		if err := json.Unmarshal([]byte(objects[0].GetValue()), &meta); err != nil {
			return err
		}
		if !change(&meta) {
			return nil
		}
		data, _ := json.Marshal(meta)
		if _, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{
			{
				Collection:      StreamMetaCollection,
				Key:             crewID,
				UserID:          SystemUserID,
				Value:           string(data),
				Version:         objects[0].GetVersion(),
				PermissionRead:  2,
				PermissionWrite: 0,
			},
		}); err == nil {
			InvalidateCrewState(crewID)
			return nil
		}
		time.Sleep(time.Duration(20*(attempt+1)) * time.Millisecond)
	}
	return fmt.Errorf("stream_meta write for crew %s failed after 3 retries", crewID)
}

const (
	StreamCollection     = "active_streams"
	StreamMetaCollection = "stream_meta"
	SystemUserID         = "00000000-0000-0000-0000-000000000000" // Nakama system user
	ThumbnailCollection  = "thumbnails"
	NotifyStreamStart    = 100
	NotifyStreamEnd      = 101
)

// ---------------------------------------------------------------------------
// Stream RPCs
// ---------------------------------------------------------------------------

func StartStreamRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req StartStreamRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	members, _, err := nk.GroupUsersList(ctx, req.CrewID, 100, nil, "")
	if err != nil {
		return "", runtime.NewError("crew not found", 5)
	}

	isMember := false
	for _, m := range members {
		if m.GetUser().GetId() == userID {
			isMember = true
			break
		}
	}
	if !isMember {
		return "", runtime.NewError("not a crew member", 7)
	}

	users, err := nk.UsersGetId(ctx, []string{userID}, nil)
	if err != nil || len(users) == 0 {
		return "", runtime.NewError("user not found", 13)
	}

	// The SFU is the only stream transport. Sign the host token before any
	// state is written, so a failure leaves no stream behind.
	if !sfuAvailableForCrew(ctx, nk, req.CrewID) {
		logger.Warn("start_stream: SFU unavailable for crew %s (auth=%v)", req.CrewID, sfuAuthEnabled())
		return "", errSFUUnavailable
	}

	if req.ResumeSessionID != "" {
		return resumeStream(ctx, logger, nk, userID, users[0].GetDisplayName(), req)
	}

	now := time.Now()
	streamID := fmt.Sprintf("stream_%s_%d", userID[:8], now.UnixMilli())

	region := selectSFURegion(req.PreferredRegion)
	endpoint := sfuEndpointForRegion(region)
	token, err := signSFUToken(SFUTokenClaims{
		UserID:    userID,
		Username:  users[0].GetDisplayName(),
		SessionID: streamID,
		Type:      "stream",
		Role:      "host",
		CrewID:    req.CrewID,
		Region:    region,
	})
	if err != nil {
		logger.Error("start_stream: failed to sign SFU token: %v", err)
		return "", errSFUUnavailable
	}

	session := StreamSessionMeta{
		CrewID:      req.CrewID,
		HostUserID:  userID,
		Mode:        "sfu",
		SFURegion:   region,
		SFUEndpoint: endpoint,
	}
	advert := streamAdvert{
		SessionID:   streamID,
		CrewID:      req.CrewID,
		HostID:      userID,
		HostName:    users[0].GetDisplayName(),
		Title:       req.Title,
		StartedAt:   now.Unix(),
		Width:       req.Width,
		Height:      req.Height,
		BitrateKbps: req.BitrateKbps,
		Fps:         req.Fps,
	}
	response := map[string]interface{}{
		"stream_id":    streamID,
		"session_id":   streamID,
		"mode":         "sfu",
		"sfu_endpoint": endpoint,
		"sfu_token":    token,
		"capture":      loadCapturePolicy(ctx, nk),
	}

	if req.Advertise == advertiseOnLive {
		// The crew learns of the stream from stream_live, when the host's
		// track is open. Until then only the session record exists: no
		// viewer can find the stream, and the host can join the SFU.
		session.Pending = &advert
		storeStreamSession(ctx, nk, streamID, session)
		response["advertise"] = advertiseOnLive
		logger.Info("User %s started stream %s (SFU) in crew %s region=%s; advertised when live", userID, streamID, req.CrewID, region)
		resp, _ := json.Marshal(response)
		return string(resp), nil
	}

	if err := advertiseStream(ctx, logger, nk, advert); err != nil {
		logger.Error("failed to write stream state: %v", err)
		return "", runtime.NewError("failed to start stream", 13)
	}
	storeStreamSession(ctx, nk, streamID, session)

	logger.Info("User %s started stream %s (SFU) in crew %s region=%s", userID, streamID, req.CrewID, region)
	resp, _ := json.Marshal(response)
	return string(resp), nil
}

// streamAdvert is what the crew learns about a stream: the records that
// crew_state reads and the stream_started push.
type streamAdvert struct {
	SessionID   string `json:"session_id"`
	CrewID      string `json:"crew_id"`
	HostID      string `json:"host_id"`
	HostName    string `json:"host_name"`
	Title       string `json:"title,omitempty"`
	StartedAt   int64  `json:"started_at"`
	Width       uint32 `json:"width,omitempty"`
	Height      uint32 `json:"height,omitempty"`
	BitrateKbps uint32 `json:"bitrate_kbps,omitempty"`
	Fps         uint32 `json:"fps,omitempty"`
}

// streamStartedPayload is the data of the stream_started push. It carries
// the mode and the encode size and rate, so a viewer can start its decoder
// with no second RPC (streaming-quality stage 6).
func streamStartedPayload(a streamAdvert) map[string]interface{} {
	return map[string]interface{}{
		"stream_id":         a.SessionID,
		"streamer_id":       a.HostID,
		"streamer_username": a.HostName,
		"title":             a.Title,
		"mode":              "sfu",
		"width":             a.Width,
		"height":            a.Height,
		"fps":               a.Fps,
	}
}

// advertiseStream writes the active_streams and stream_meta records and the
// host's presence, and pushes stream_started to the crew.
func advertiseStream(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, a streamAdvert) error {
	streamJSON, _ := json.Marshal(ActiveStream{
		HostID:      a.HostID,
		HostName:    a.HostName,
		Title:       a.Title,
		StartedAt:   a.StartedAt,
		Width:       a.Width,
		Height:      a.Height,
		BitrateKbps: a.BitrateKbps,
		Fps:         a.Fps,
	})
	if _, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      StreamCollection,
			Key:             a.CrewID,
			UserID:          a.HostID,
			Value:           string(streamJSON),
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	}); err != nil {
		return err
	}

	metaJSON, _ := json.Marshal(StreamMeta{
		StreamID:         a.SessionID,
		CrewID:           a.CrewID,
		StreamerID:       a.HostID,
		StreamerUsername: a.HostName,
		Title:            a.Title,
		StartedAt:        time.Unix(a.StartedAt, 0).UTC().Format(time.RFC3339),
		Width:            a.Width,
		Height:           a.Height,
		BitrateKbps:      a.BitrateKbps,
		Fps:              a.Fps,
	})
	nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      StreamMetaCollection,
			Key:             a.CrewID,
			UserID:          SystemUserID,
			Value:           string(metaJSON),
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	})

	now := time.Now().UTC().Format(time.RFC3339)
	_ = WritePresence(ctx, nk, &UserPresence{
		UserID:   a.HostID,
		Status:   StatusOnline,
		LastSeen: now,
		Activity: &Activity{
			Type:        ActivityStreaming,
			CrewID:      a.CrewID,
			StreamID:    a.SessionID,
			StreamTitle: a.Title,
		},
		UpdatedAt: now,
	})

	InvalidateCrewState(a.CrewID)
	PushCrewEvent(ctx, logger, nk, a.CrewID, "stream_started", streamStartedPayload(a))
	return nil
}

// StreamLiveRequest is the stream_live payload: the host's track is open.
type StreamLiveRequest struct {
	CrewID    string `json:"crew_id"`
	SessionID string `json:"session_id"`
	// The encode size and rate the host runs at. Zero keeps the values of
	// start_stream.
	Width       uint32 `json:"width,omitempty"`
	Height      uint32 `json:"height,omitempty"`
	Fps         uint32 `json:"fps,omitempty"`
	BitrateKbps uint32 `json:"bitrate_kbps,omitempty"`
}

// StreamLiveRPC advertises a stream that start_stream recorded with
// advertise "on_live". The host calls it when its SFU track is open, so the
// crew never sees a stream that no SFU session carries (streaming-quality
// stage 6). Only the host of the session can call it. A second call for a
// stream that is already advertised changes nothing.
func StreamLiveRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	var req StreamLiveRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" || req.SessionID == "" {
		return "", runtime.NewError("crew_id and session_id required", 3)
	}
	session := loadStreamSession(ctx, nk, req.SessionID)
	if session == nil || session.CrewID != req.CrewID {
		return "", runtime.NewError("stream not found", 5)
	}
	if session.HostUserID != userID {
		return "", runtime.NewError("not the host of this stream", 7)
	}
	if session.Pending == nil {
		if live := readStreamMeta(ctx, nk, req.CrewID); live != nil && live.StreamID == req.SessionID {
			return `{"advertised":true}`, nil
		}
		return "", runtime.NewError("stream not found", 5)
	}
	advert := *session.Pending
	if req.Width > 0 && req.Height > 0 {
		advert.Width = req.Width
		advert.Height = req.Height
	}
	if req.Fps > 0 {
		advert.Fps = req.Fps
	}
	if req.BitrateKbps > 0 {
		advert.BitrateKbps = req.BitrateKbps
	}
	if err := advertiseStream(ctx, logger, nk, advert); err != nil {
		logger.Error("stream_live: failed to write stream state: %v", err)
		return "", runtime.NewError("failed to advertise stream", 13)
	}
	session.Pending = nil
	storeStreamSession(ctx, nk, req.SessionID, *session)
	logger.Info("stream_live: stream %s of %s advertised in crew %s (%dx%d at %d fps)",
		req.SessionID, userID, req.CrewID, advert.Width, advert.Height, advert.Fps)
	return `{"advertised":true}`, nil
}

// RefreshStreamTokenRPC signs a new SFU token for a stream session: a host
// token for its host, a viewer token for another crew member. A token lives
// 5 minutes; a client that joins the SFU again after that asks for a new one
// (streaming-quality stage 6). The session must still exist.
func RefreshStreamTokenRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	var req struct {
		SessionID string `json:"session_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.SessionID == "" {
		return "", runtime.NewError("session_id required", 3)
	}
	session := loadStreamSession(ctx, nk, req.SessionID)
	if session == nil {
		return "", runtime.NewError("stream not found", 5)
	}
	role := "viewer"
	if session.HostUserID == userID {
		role = "host"
	} else if !isCrewMember(ctx, nk, session.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}
	if session.SFUEndpoint == "" {
		return "", errSFUUnavailable
	}
	token, err := signSFUToken(SFUTokenClaims{
		UserID:    userID,
		Username:  resolveUsername(ctx, nk, userID),
		SessionID: req.SessionID,
		Type:      "stream",
		Role:      role,
		CrewID:    session.CrewID,
		Region:    session.SFURegion,
	})
	if err != nil {
		logger.Error("refresh_stream_token: failed to sign SFU token: %v", err)
		return "", errSFUUnavailable
	}
	resp, _ := json.Marshal(map[string]interface{}{
		"sfu_endpoint": session.SFUEndpoint,
		"sfu_token":    token,
		"role":         role,
	})
	return string(resp), nil
}

func StopStreamRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req StopStreamRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	// Read stream metadata before deleting (for event ledger)
	var streamMeta *StreamMeta
	metaObjects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: StreamMetaCollection, Key: req.CrewID, UserID: SystemUserID},
	})
	if err == nil && len(metaObjects) > 0 {
		var m StreamMeta
		if json.Unmarshal([]byte(metaObjects[0].GetValue()), &m) == nil {
			streamMeta = &m
		}
	}
	// Only the host stops a stream. Without this check any crew member
	// could end the stream of another member.
	if streamMeta != nil && streamMeta.StreamerID != userID {
		logger.Warn("stop_stream: user %s is not the host of stream %s in crew %s",
			userID, streamMeta.StreamID, req.CrewID)
		return "", runtime.NewError("not the host of this stream", 7)
	}

	// Delete active stream record
	if err := nk.StorageDelete(ctx, []*runtime.StorageDelete{
		{
			Collection: StreamCollection,
			Key:        req.CrewID,
			UserID:     userID,
		},
	}); err != nil {
		logger.Warn("failed to delete stream state: %v", err)
	}

	// Delete stream metadata
	nk.StorageDelete(ctx, []*runtime.StorageDelete{
		{
			Collection: StreamMetaCollection,
			Key:        req.CrewID,
			UserID:     SystemUserID,
		},
	})

	// Delete stream session record
	if streamMeta != nil {
		nk.StorageDelete(ctx, []*runtime.StorageDelete{
			{Collection: StreamSessionCol, Key: streamMeta.StreamID, UserID: SystemUserID},
		})
	}
	// A stream that start_stream recorded and never advertised: only the
	// session record exists.
	if req.SessionID != "" && (streamMeta == nil || streamMeta.StreamID != req.SessionID) {
		if pending := loadStreamSession(ctx, nk, req.SessionID); pending != nil &&
			pending.HostUserID == userID && pending.Pending != nil {
			nk.StorageDelete(ctx, []*runtime.StorageDelete{
				{Collection: StreamSessionCol, Key: req.SessionID, UserID: SystemUserID},
			})
		}
	}

	// Write stream_session event to the crew event ledger
	if streamMeta != nil {
		durationMin := 0
		if startedAt, parseErr := time.Parse(time.RFC3339, streamMeta.StartedAt); parseErr == nil {
			durationMin = int(time.Since(startedAt).Minutes())
			if durationMin < 1 {
				durationMin = 1
			}
		}

		snapshotURLs, err := ListSnapshotURLs(req.CrewID, streamMeta.StreamID)
		if err != nil {
			logger.Warn("StopStreamRPC: ListSnapshotURLs failed, continuing without snapshots: %v", err)
			snapshotURLs = []string{}
		}

		sessionData := StreamSessionData{
			SessionID:    streamMeta.StreamID,
			StreamerID:   streamMeta.StreamerID,
			StreamerName: streamMeta.StreamerUsername,
			Title:        streamMeta.Title,
			DurationMin:  durationMin,
			PeakViewers:  streamMeta.peakViewers(),
			ViewerIDs:    streamMeta.allViewers(),
			SnapshotURLs: snapshotURLs,
		}
		event := CrewEvent{
			ID:        generateEventID(),
			CrewID:    req.CrewID,
			Type:      "stream_session",
			ActorID:   userID,
			Timestamp: time.Now().UnixMilli(),
			Score:     30,
			Data:      sessionData,
		}
		if appendErr := AppendCrewEvent(ctx, nk, req.CrewID, event); appendErr != nil {
			logger.Warn("Failed to write stream_session event for crew %s: %v", req.CrewID, appendErr)
		}
		// Mirror into the durable store so the replay survives the ledger trim.
		if upsertErr := UpsertStreamSession(ctx, nk, req.CrewID,
			storedStreamSessionFrom(event.ID, event.Timestamp, event.Score, sessionData)); upsertErr != nil {
			logger.Warn("Failed to persist durable stream_session for crew %s: %v", req.CrewID, upsertErr)
		}
	}

	// Reset user presence
	now := time.Now().UTC().Format(time.RFC3339)
	_ = WritePresence(ctx, nk, &UserPresence{
		UserID:    userID,
		Status:    StatusOnline,
		LastSeen:  now,
		Activity:  &Activity{Type: ActivityNone},
		UpdatedAt: now,
	})

	InvalidateCrewState(req.CrewID)

	// Push priority event: stream_ended
	PushCrewEvent(ctx, logger, nk, req.CrewID, "stream_ended", map[string]interface{}{
		"crew_id": req.CrewID,
		"host_id": userID,
	})

	logger.Info("User %s stopped stream in crew %s", userID, req.CrewID)
	return "{}", nil
}

// ---------------------------------------------------------------------------
// Thumbnail upload RPC
// ---------------------------------------------------------------------------

func StreamThumbnailUploadRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		StreamID        string `json:"stream_id"`
		CrewID          string `json:"crew_id"`
		ThumbnailBase64 string `json:"thumbnail_base64"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	// Verify the user owns the stream by reading stream metadata
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{
			Collection: StreamMetaCollection,
			Key:        req.CrewID,
			UserID:     SystemUserID,
		},
	})
	if err != nil || len(objects) == 0 {
		return "", runtime.NewError("stream not found", 5)
	}

	var meta StreamMeta
	if err := json.Unmarshal([]byte(objects[0].Value), &meta); err != nil {
		return "", runtime.NewError("invalid stream metadata", 13)
	}
	if meta.StreamerID != userID {
		return "", runtime.NewError("unauthorized: not the streamer", 7)
	}

	// Validate base64
	thumbnailBytes, err := base64.StdEncoding.DecodeString(req.ThumbnailBase64)
	if err != nil {
		return "", runtime.NewError("invalid thumbnail data", 3)
	}
	if len(thumbnailBytes) > 512*1024 { // 512KB max
		return "", runtime.NewError("thumbnail too large", 3)
	}

	// Store thumbnail in Nakama storage
	now := time.Now().UTC()
	thumbnailKey := fmt.Sprintf("%s/latest", req.StreamID)
	thumbValue, _ := json.Marshal(map[string]string{"data": req.ThumbnailBase64})
	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      ThumbnailCollection,
			Key:             thumbnailKey,
			UserID:          SystemUserID,
			Value:           string(thumbValue),
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	})
	if err != nil {
		logger.Error("failed to store thumbnail: %v", err)
		return "", runtime.NewError("failed to store thumbnail", 13)
	}

	// Update stream metadata with thumbnail info
	thumbnailURL := fmt.Sprintf("/v2/storage/%s/%s/%s?t=%d", ThumbnailCollection, SystemUserID, thumbnailKey, now.Unix())
	meta.ThumbnailURL = thumbnailURL
	meta.ThumbnailUpdatedAt = now.Format(time.RFC3339)

	metaJSON, _ := json.Marshal(meta)
	nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      StreamMetaCollection,
			Key:             req.CrewID,
			UserID:          SystemUserID,
			Value:           string(metaJSON),
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	})

	// Invalidate crew state so next sidebar batch picks up new thumbnail
	InvalidateCrewState(req.CrewID)

	resp, _ := json.Marshal(map[string]interface{}{
		"success":       true,
		"thumbnail_url": thumbnailURL,
	})
	return string(resp), nil
}

// ---------------------------------------------------------------------------
// WatchStream RPC — viewer requests an SFU token for an existing stream
// ---------------------------------------------------------------------------

func WatchStreamRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		SessionID string `json:"session_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.SessionID == "" {
		return "", runtime.NewError("session_id required", 3)
	}

	meta := loadStreamSession(ctx, nk, req.SessionID)
	if meta == nil {
		return "", runtime.NewError("stream not found", 5)
	}

	if !isCrewMember(ctx, nk, meta.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	// Load StreamMeta for encode resolution
	var streamWidth, streamHeight, streamBitrateKbps uint32
	smObjs, smErr := nk.StorageRead(ctx, []*runtime.StorageRead{
		{
			Collection: StreamMetaCollection,
			Key:        meta.CrewID,
			UserID:     SystemUserID,
		},
	})
	if smErr == nil && len(smObjs) > 0 {
		var sm StreamMeta
		if err := json.Unmarshal([]byte(smObjs[0].Value), &sm); err == nil {
			streamWidth = sm.Width
			streamHeight = sm.Height
			streamBitrateKbps = sm.BitrateKbps
		}
	}

	// A session without an SFU endpoint is a record from before the SFU was
	// the only transport. No viewer can join it.
	if meta.SFUEndpoint == "" {
		logger.Warn("watch_stream: session %s has no SFU endpoint", req.SessionID)
		return "", errSFUUnavailable
	}

	token, err := signSFUToken(SFUTokenClaims{
		UserID:    userID,
		Username:  resolveUsername(ctx, nk, userID),
		SessionID: req.SessionID,
		Type:      "stream",
		Role:      "viewer",
		CrewID:    meta.CrewID,
		Region:    meta.SFURegion,
	})
	if err != nil {
		logger.Error("watch_stream: failed to sign SFU token: %v", err)
		return "", errSFUUnavailable
	}

	// Count the viewer in. crew_state viewer_count reads this list; the
	// host takes the viewer out again with stream_viewer_left.
	if userID != meta.HostUserID {
		if err := updateStreamMeta(ctx, nk, meta.CrewID, func(m *StreamMeta) bool {
			return m.StreamID == req.SessionID && m.addViewer(userID)
		}); err != nil {
			logger.Warn("watch_stream: viewer count not updated for %s: %v", req.SessionID, err)
		}
	}

	resp, _ := json.Marshal(map[string]interface{}{
		"mode":         "sfu",
		"sfu_endpoint": meta.SFUEndpoint,
		"sfu_token":    token,
		"width":        streamWidth,
		"height":       streamHeight,
		"bitrate_kbps": streamBitrateKbps,
	})
	return string(resp), nil
}

// StreamViewerLeftRPC takes a viewer out of the crew's viewer count. The
// host calls it on the SFU member_left for a viewer (the SFU sends none for
// a viewer that reconnects). Only the host of the crew's stream can call it.
func StreamViewerLeftRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	var req struct {
		CrewID   string `json:"crew_id"`
		ViewerID string `json:"viewer_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" || req.ViewerID == "" {
		return "", runtime.NewError("crew_id and viewer_id required", 3)
	}
	meta := readStreamMeta(ctx, nk, req.CrewID)
	if meta == nil {
		return "{}", nil
	}
	if meta.StreamerID != userID {
		return "", runtime.NewError("not the host of this stream", 7)
	}
	if err := updateStreamMeta(ctx, nk, req.CrewID, func(m *StreamMeta) bool {
		return m.StreamerID == userID && m.removeViewer(req.ViewerID)
	}); err != nil {
		logger.Warn("stream_viewer_left: %v", err)
		return "", runtime.NewError("failed to update the viewer count", 13)
	}
	return "{}", nil
}

// ---------------------------------------------------------------------------
// UpdateStreamResolution — host updates actual encode resolution after encoder init
// ---------------------------------------------------------------------------

func UpdateStreamResolutionRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID string `json:"crew_id"`
		Width  uint32 `json:"width"`
		Height uint32 `json:"height"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{
			Collection: StreamMetaCollection,
			Key:        req.CrewID,
			UserID:     SystemUserID,
		},
	})
	if err != nil || len(objects) == 0 {
		return "", runtime.NewError("stream not found", 5)
	}

	var meta StreamMeta
	if err := json.Unmarshal([]byte(objects[0].Value), &meta); err != nil {
		return "", runtime.NewError("invalid stream metadata", 13)
	}
	if meta.StreamerID != userID {
		return "", runtime.NewError("unauthorized: not the streamer", 7)
	}

	meta.Width = req.Width
	meta.Height = req.Height

	metaJSON, _ := json.Marshal(meta)
	nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      StreamMetaCollection,
			Key:             req.CrewID,
			UserID:          SystemUserID,
			Value:           string(metaJSON),
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	})

	// Also update the active_streams entry
	streamObjs, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{
			Collection: StreamCollection,
			Key:        req.CrewID,
			UserID:     userID,
		},
	})
	if err == nil && len(streamObjs) > 0 {
		var stream ActiveStream
		if err := json.Unmarshal([]byte(streamObjs[0].Value), &stream); err == nil {
			stream.Width = req.Width
			stream.Height = req.Height
			streamJSON, _ := json.Marshal(stream)
			nk.StorageWrite(ctx, []*runtime.StorageWrite{
				{
					Collection:      StreamCollection,
					Key:             req.CrewID,
					UserID:          userID,
					Value:           string(streamJSON),
					PermissionRead:  2,
					PermissionWrite: 0,
				},
			})
		}
	}

	InvalidateCrewState(req.CrewID)

	resp, _ := json.Marshal(map[string]interface{}{"ok": true})
	return string(resp), nil
}

// ---------------------------------------------------------------------------
// Stream session storage — the SFU region and endpoint of each active stream
// ---------------------------------------------------------------------------

type StreamSessionMeta struct {
	CrewID      string `json:"crew_id"`
	HostUserID  string `json:"host_user_id"`
	Mode        string `json:"mode"`
	SFURegion   string `json:"sfu_region,omitempty"`
	SFUEndpoint string `json:"sfu_endpoint,omitempty"`
	// Pending is the advertisement of a stream that start_stream recorded
	// with advertise "on_live" and stream_live has not advertised yet.
	Pending *streamAdvert `json:"pending,omitempty"`
}

func storeStreamSession(ctx context.Context, nk runtime.NakamaModule, sessionID string, meta StreamSessionMeta) {
	data, _ := json.Marshal(meta)
	nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      StreamSessionCol,
			Key:             sessionID,
			UserID:          SystemUserID,
			Value:           string(data),
			PermissionRead:  1,
			PermissionWrite: 0,
		},
	})
}

func loadStreamSession(ctx context.Context, nk runtime.NakamaModule, sessionID string) *StreamSessionMeta {
	records, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{
			Collection: StreamSessionCol,
			Key:        sessionID,
			UserID:     SystemUserID,
		},
	})
	if err != nil || len(records) == 0 {
		return nil
	}
	var meta StreamSessionMeta
	if err := json.Unmarshal([]byte(records[0].Value), &meta); err != nil {
		return nil
	}
	return &meta
}

// ---------------------------------------------------------------------------
// Session-end and GC cleanup
// ---------------------------------------------------------------------------

// StreamCleanupUser cleans up any active stream hosted by the given user.
// Called from OnSessionEnd when a user disconnects without calling stop_stream.
func StreamCleanupUser(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID string) {
	groups, _, err := nk.UserGroupsList(ctx, userID, 100, nil, "")
	if err != nil {
		logger.Warn("StreamCleanupUser: failed to list groups for %s: %v", userID, err)
		return
	}

	for _, g := range groups {
		crewID := g.GetGroup().GetId()
		cleanupStreamIfHost(ctx, logger, nk, crewID, userID)
	}
}

// cleanupStreamIfHost checks stream_meta for a crew and, if the given user is
// the host, deletes all stream storage and pushes stream_ended.
func cleanupStreamIfHost(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID, userID string) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: StreamMetaCollection, Key: crewID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return
	}

	var meta StreamMeta
	if err := json.Unmarshal([]byte(objects[0].Value), &meta); err != nil {
		return
	}
	if meta.StreamerID != userID {
		return
	}

	logger.Info("StreamCleanupUser: cleaning orphaned stream %s for host %s in crew %s", meta.StreamID, userID, crewID)
	// Read before the delete: the host can re-advertise this session.
	sessionMeta := loadStreamSession(ctx, nk, meta.StreamID)

	// Delete active_streams (owned by host user)
	if err := nk.StorageDelete(ctx, []*runtime.StorageDelete{
		{Collection: StreamCollection, Key: crewID, UserID: userID},
	}); err != nil {
		logger.Warn("StreamCleanupUser: failed to delete active_streams for crew %s: %v", crewID, err)
	}

	// Delete stream_meta (owned by system user)
	nk.StorageDelete(ctx, []*runtime.StorageDelete{
		{Collection: StreamMetaCollection, Key: crewID, UserID: SystemUserID},
	})

	// Delete stream_sessions
	nk.StorageDelete(ctx, []*runtime.StorageDelete{
		{Collection: StreamSessionCol, Key: meta.StreamID, UserID: SystemUserID},
	})

	// Write stream_session event to the crew event ledger
	durationMin := 0
	if startedAt, parseErr := time.Parse(time.RFC3339, meta.StartedAt); parseErr == nil {
		durationMin = int(time.Since(startedAt).Minutes())
		if durationMin < 1 {
			durationMin = 1
		}
	}
	snapshotURLs, err := ListSnapshotURLs(crewID, meta.StreamID)
	if err != nil {
		logger.Warn("cleanupStreamIfHost: ListSnapshotURLs failed, continuing without snapshots: %v", err)
		snapshotURLs = []string{}
	}
	sessionData := StreamSessionData{
		SessionID:    meta.StreamID,
		StreamerID:   meta.StreamerID,
		StreamerName: meta.StreamerUsername,
		Title:        meta.Title,
		DurationMin:  durationMin,
		PeakViewers:  meta.peakViewers(),
		ViewerIDs:    meta.allViewers(),
		SnapshotURLs: snapshotURLs,
	}
	event := CrewEvent{
		ID:        generateEventID(),
		CrewID:    crewID,
		Type:      "stream_session",
		ActorID:   userID,
		Timestamp: time.Now().UnixMilli(),
		Score:     30,
		Data:      sessionData,
	}
	if appendErr := AppendCrewEvent(ctx, nk, crewID, event); appendErr != nil {
		logger.Warn("StreamCleanupUser: failed to write stream_session event for crew %s: %v", crewID, appendErr)
	}
	// A Nakama socket drop also ends here, while the host's SFU session and
	// encoder keep running. Keep what was removed, so the host can
	// re-advertise the same stream (resumeStream).
	if sessionMeta != nil {
		writeStreamResumeRecord(ctx, nk, userID, streamResumeRecord{
			Meta:          meta,
			Session:       *sessionMeta,
			LedgerEventID: event.ID,
			EndedAt:       time.Now().Unix(),
		})
	}
	// Mirror into the durable store so the replay survives the ledger trim.
	if upsertErr := UpsertStreamSession(ctx, nk, crewID,
		storedStreamSessionFrom(event.ID, event.Timestamp, event.Score, sessionData)); upsertErr != nil {
		logger.Warn("StreamCleanupUser: failed to persist durable stream_session for crew %s: %v", crewID, upsertErr)
	}

	InvalidateCrewState(crewID)

	PushCrewEvent(ctx, logger, nk, crewID, "stream_ended", map[string]interface{}{
		"crew_id": crewID,
		"host_id": userID,
	})
}

func writeStreamResumeRecord(ctx context.Context, nk runtime.NakamaModule, hostID string, rec streamResumeRecord) {
	data, _ := json.Marshal(rec)
	nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      StreamResumeCollection,
			Key:             hostID,
			UserID:          SystemUserID,
			Value:           string(data),
			PermissionRead:  0,
			PermissionWrite: 0,
		},
	})
}

func readStreamResumeRecord(ctx context.Context, nk runtime.NakamaModule, hostID string) *streamResumeRecord {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: StreamResumeCollection, Key: hostID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return nil
	}
	var rec streamResumeRecord
	if json.Unmarshal([]byte(objects[0].GetValue()), &rec) != nil {
		return nil
	}
	return &rec
}

func readStreamMeta(ctx context.Context, nk runtime.NakamaModule, crewID string) *StreamMeta {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: StreamMetaCollection, Key: crewID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return nil
	}
	var meta StreamMeta
	if json.Unmarshal([]byte(objects[0].GetValue()), &meta) != nil {
		return nil
	}
	return &meta
}

// resumeStream re-advertises the stream req.ResumeSessionID of userID in
// req.CrewID. The host's SFU session still carries it; only the backend
// records went away with the Nakama socket. Two cases succeed:
//   - The records are still there (the old socket ended while a newer one
//     was open, so no cleanup ran). Nothing is written.
//   - cleanupStreamIfHost removed them less than streamResumeWindow ago.
//     The records are written back from the resume record, and the
//     stream_session ledger event of the cleanup is removed (the real stop
//     writes it).
//
// Anything else returns NOT_FOUND (5). The client then ends the stream.
func resumeStream(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID, displayName string, req StartStreamRequest) (string, error) {
	sessionID := req.ResumeSessionID
	if live := readStreamMeta(ctx, nk, req.CrewID); live != nil &&
		live.StreamID == sessionID && live.StreamerID == userID {
		if session := loadStreamSession(ctx, nk, sessionID); session != nil {
			logger.Info("start_stream resume: stream %s of %s in crew %s is still advertised", sessionID, userID, req.CrewID)
			return resumeStreamResponse(userID, displayName, sessionID, req.CrewID, *session)
		}
	}

	rec := readStreamResumeRecord(ctx, nk, userID)
	if rec == nil || rec.Meta.StreamID != sessionID || rec.Meta.CrewID != req.CrewID ||
		rec.Meta.StreamerID != userID ||
		time.Since(time.Unix(rec.EndedAt, 0)) > streamResumeWindow {
		logger.Warn("start_stream resume: stream %s of %s in crew %s is not resumable", sessionID, userID, req.CrewID)
		return "", runtime.NewError("stream not resumable", 5)
	}

	startedAt := time.Now().Unix()
	if t, err := time.Parse(time.RFC3339, rec.Meta.StartedAt); err == nil {
		startedAt = t.Unix()
	}
	streamJSON, _ := json.Marshal(ActiveStream{
		HostID:      userID,
		HostName:    rec.Meta.StreamerUsername,
		Title:       rec.Meta.Title,
		StartedAt:   startedAt,
		Width:       rec.Meta.Width,
		Height:      rec.Meta.Height,
		BitrateKbps: rec.Meta.BitrateKbps,
	})
	metaJSON, _ := json.Marshal(rec.Meta)
	if _, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      StreamCollection,
			Key:             req.CrewID,
			UserID:          userID,
			Value:           string(streamJSON),
			PermissionRead:  2,
			PermissionWrite: 0,
		},
		{
			Collection:      StreamMetaCollection,
			Key:             req.CrewID,
			UserID:          SystemUserID,
			Value:           string(metaJSON),
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	}); err != nil {
		logger.Error("start_stream resume: failed to write stream state: %v", err)
		return "", runtime.NewError("failed to resume stream", 13)
	}
	storeStreamSession(ctx, nk, sessionID, rec.Session)
	nk.StorageDelete(ctx, []*runtime.StorageDelete{
		{Collection: StreamResumeCollection, Key: userID, UserID: SystemUserID},
	})
	if rec.LedgerEventID != "" {
		if err := removeCrewEvent(ctx, nk, req.CrewID, rec.LedgerEventID); err != nil {
			logger.Warn("start_stream resume: %v", err)
		}
	}

	now := time.Now().UTC().Format(time.RFC3339)
	_ = WritePresence(ctx, nk, &UserPresence{
		UserID:   userID,
		Status:   StatusOnline,
		LastSeen: now,
		Activity: &Activity{
			Type:        ActivityStreaming,
			CrewID:      req.CrewID,
			StreamID:    sessionID,
			StreamTitle: rec.Meta.Title,
		},
		UpdatedAt: now,
	})
	InvalidateCrewState(req.CrewID)
	PushCrewEvent(ctx, logger, nk, req.CrewID, "stream_started", streamStartedPayload(streamAdvert{
		SessionID: sessionID,
		CrewID:    req.CrewID,
		HostID:    userID,
		HostName:  rec.Meta.StreamerUsername,
		Title:     rec.Meta.Title,
		Width:     rec.Meta.Width,
		Height:    rec.Meta.Height,
		Fps:       rec.Meta.Fps,
	}))
	logger.Info("start_stream resume: stream %s of %s advertised again in crew %s", sessionID, userID, req.CrewID)
	return resumeStreamResponse(userID, displayName, sessionID, req.CrewID, rec.Session)
}

func resumeStreamResponse(userID, displayName, sessionID, crewID string, session StreamSessionMeta) (string, error) {
	token, err := signSFUToken(SFUTokenClaims{
		UserID:    userID,
		Username:  displayName,
		SessionID: sessionID,
		Type:      "stream",
		Role:      "host",
		CrewID:    crewID,
		Region:    session.SFURegion,
	})
	if err != nil {
		return "", errSFUUnavailable
	}
	resp, _ := json.Marshal(map[string]interface{}{
		"stream_id":    sessionID,
		"session_id":   sessionID,
		"mode":         "sfu",
		"sfu_endpoint": session.SFUEndpoint,
		"sfu_token":    token,
		"resumed":      true,
	})
	return string(resp), nil
}

// removeCrewEvent deletes one event from the ledger of a crew, with the same
// optimistic-concurrency retry as AppendCrewEvent.
func removeCrewEvent(ctx context.Context, nk runtime.NakamaModule, crewID, eventID string) error {
	for attempt := 0; attempt < 3; attempt++ {
		ledger, version := readLedger(ctx, nk, crewID)
		kept := make([]CrewEvent, 0, len(ledger.Events))
		for _, e := range ledger.Events {
			if e.ID != eventID {
				kept = append(kept, e)
			}
		}
		if len(kept) == len(ledger.Events) {
			return nil
		}
		ledger.Events = kept
		ledger.UpdatedAt = time.Now().UnixMilli()
		if err := writeLedger(ctx, nk, crewID, ledger, version); err == nil {
			return nil
		}
		time.Sleep(time.Duration(50*(attempt+1)) * time.Millisecond)
	}
	return fmt.Errorf("removing ledger event %s of crew %s failed after 3 retries", eventID, crewID)
}

// StartStreamGC runs a background loop that cleans up orphaned stream records
// whose host is offline. Safety net for cases where OnSessionEnd didn't fire
// or failed to clean up.
func StartStreamGC(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, interval time.Duration) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for range ticker.C {
		streamGC(ctx, logger, nk)
	}
}

const streamGCStalenessThreshold = 2 * time.Hour

func streamGC(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule) {
	cursor := ""
	for {
		objects, nextCursor, err := nk.StorageList(ctx, "", SystemUserID, StreamMetaCollection, 100, cursor)
		if err != nil {
			logger.Warn("StreamGC: failed to list stream_meta: %v", err)
			return
		}
		for _, obj := range objects {
			var meta StreamMeta
			if err := json.Unmarshal([]byte(obj.Value), &meta); err != nil {
				continue
			}
			p, err := ReadPresence(ctx, nk, meta.StreamerID)
			if err != nil {
				continue
			}
			stale := false
			if p.Status == StatusOffline {
				stale = true
			} else if p.UpdatedAt != "" {
				if updatedAt, parseErr := time.Parse(time.RFC3339, p.UpdatedAt); parseErr == nil {
					if time.Since(updatedAt) > streamGCStalenessThreshold {
						stale = true
					}
				}
			}
			if stale {
				logger.Info("StreamGC: removing orphaned stream %s (host %s, crew %s)", meta.StreamID, meta.StreamerID, meta.CrewID)
				cleanupStreamIfHost(ctx, logger, nk, meta.CrewID, meta.StreamerID)
			}
		}
		if nextCursor == "" {
			break
		}
		cursor = nextCursor
	}
}

// StartSnapshotBackfillJob runs every 60s and checks for stream_session events
// with empty SnapshotURLs — the SFU may have uploaded frames after StopStreamRPC returned.
func StartSnapshotBackfillJob(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger) {
	ticker := time.NewTicker(60 * time.Second)
	defer ticker.Stop()
	for range ticker.C {
		snapshotBackfill(ctx, nk, logger)
	}
}

const snapshotBackfillWindowHours = 24

func snapshotBackfill(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger) {
	cutoff := time.Now().Add(-time.Duration(snapshotBackfillWindowHours) * time.Hour).UnixMilli()

	// Scan crew event ledgers directly so ended streams are still discoverable.
	cursor := ""
	for {
		objects, nextCursor, err := nk.StorageList(ctx, "", SystemUserID, CrewEventsCollection, 100, cursor)
		if err != nil {
			logger.Warn("SnapshotBackfill: StorageList failed: %v", err)
			return
		}
		for _, obj := range objects {
			if obj.Key == "" {
				continue
			}
			backfillStreamSession(ctx, nk, logger, obj.Key, cutoff)
		}
		if nextCursor == "" {
			break
		}
		cursor = nextCursor
	}
}

func backfillStreamSession(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, crewID string, cutoff int64) {
	ledger, _ := readLedger(ctx, nk, crewID)

	for _, e := range ledger.Events {
		if e.Type != "stream_session" {
			continue
		}
		if e.Timestamp < cutoff {
			continue
		}

		data, err := decodeStreamSessionData(e.Data)
		if err != nil {
			continue
		}

		if data.SessionID == "" {
			continue
		}

		urls, err := ListSnapshotURLs(crewID, data.SessionID)
		if err != nil || len(urls) == 0 {
			continue
		}

		streamDoc, _ := readStreamSessionsDoc(ctx, nk, crewID)
		needsDurableUpsert := streamSessionNeedsDurableUpsert(streamDoc.Sessions, e.ID, data.SessionID, len(urls))

		// Skip only when both ledger snapshots and the durable replay copy are up-to-date.
		if len(data.SnapshotURLs) >= len(urls) && !needsDurableUpsert {
			continue
		}

		if len(data.SnapshotURLs) < len(urls) {
			logger.Info("SnapshotBackfill: backfilling %d snapshot URLs for crew=%s session=%s (had %d)", len(urls), crewID, data.SessionID, len(data.SnapshotURLs))
			if updateErr := UpdateLedgerEventSnapshotURLs(ctx, nk, crewID, e.ID, urls); updateErr != nil {
				logger.Warn("SnapshotBackfill: update failed for crew=%s session=%s: %v", crewID, data.SessionID, updateErr)
			}
		}
		// Keep the durable replay copy in sync with the freshly backfilled URLs.
		data.SnapshotURLs = urls
		if upsertErr := UpsertStreamSession(ctx, nk, crewID,
			storedStreamSessionFrom(e.ID, e.Timestamp, e.Score, data)); upsertErr != nil {
			logger.Warn("SnapshotBackfill: durable upsert failed for crew=%s session=%s: %v", crewID, data.SessionID, upsertErr)
		}
		return
	}
}
