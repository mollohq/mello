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
}

const (
	MaxP2PViewers    = 5
	MaxSFUViewers    = 100
	StreamSessionCol = "stream_sessions"
)

type StopStreamRequest struct {
	CrewID string `json:"crew_id"`
}

type ActiveStream struct {
	HostID    string `json:"host_id"`
	HostName  string `json:"host_name"`
	Title     string `json:"title"`
	StartedAt int64  `json:"started_at"`
	Width     uint32 `json:"width,omitempty"`
	Height    uint32 `json:"height,omitempty"`
}

// StreamMeta is the extended metadata stored in stream_meta/{crew_id}.
type StreamMeta struct {
	StreamID          string   `json:"stream_id"`
	CrewID            string   `json:"crew_id"`
	StreamerID        string   `json:"streamer_id"`
	StreamerUsername  string   `json:"streamer_username"`
	Title             string   `json:"title"`
	StartedAt         string   `json:"started_at"`
	ThumbnailURL      string   `json:"thumbnail_url,omitempty"`
	ThumbnailUpdatedAt string  `json:"thumbnail_updated_at,omitempty"`
	ViewerIDs         []string `json:"viewer_ids,omitempty"`
	Width             uint32   `json:"width,omitempty"`
	Height            uint32   `json:"height,omitempty"`
}

const (
	StreamCollection    = "active_streams"
	StreamMetaCollection = "stream_meta"
	SystemUserID        = "00000000-0000-0000-0000-000000000000" // Nakama system user
	ThumbnailCollection = "thumbnails"
	NotifyStreamStart   = 100
	NotifyStreamEnd     = 101
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

	now := time.Now()
	streamID := fmt.Sprintf("stream_%s_%d", userID[:8], now.UnixMilli())

	stream := ActiveStream{
		HostID:    userID,
		HostName:  users[0].GetDisplayName(),
		Title:     req.Title,
		StartedAt: now.Unix(),
		Width:     req.Width,
		Height:    req.Height,
	}
	streamJSON, _ := json.Marshal(stream)

	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      StreamCollection,
			Key:             req.CrewID,
			UserID:          userID,
			Value:           string(streamJSON),
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	})
	if err != nil {
		logger.Error("failed to write stream state: %v", err)
		return "", runtime.NewError("failed to start stream", 13)
	}

	// Write stream metadata
	meta := StreamMeta{
		StreamID:         streamID,
		CrewID:           req.CrewID,
		StreamerID:       userID,
		StreamerUsername: users[0].GetDisplayName(),
		Title:            req.Title,
		StartedAt:        now.UTC().Format(time.RFC3339),
		Width:            req.Width,
		Height:           req.Height,
	}
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

	// Update user presence
	_ = WritePresence(ctx, nk, &UserPresence{
		UserID:   userID,
		Status:   StatusOnline,
		LastSeen: now.UTC().Format(time.RFC3339),
		Activity: &Activity{
			Type:        ActivityStreaming,
			CrewID:      req.CrewID,
			StreamID:    streamID,
			StreamTitle: req.Title,
		},
		UpdatedAt: now.UTC().Format(time.RFC3339),
	})

	InvalidateCrewState(req.CrewID)

	// Push priority event: stream_started
	PushCrewEvent(ctx, logger, nk, req.CrewID, "stream_started", map[string]interface{}{
		"stream_id":          streamID,
		"streamer_id":        userID,
		"streamer_username":  users[0].GetDisplayName(),
		"title":              req.Title,
	})

	// SFU path: premium crews get server-relayed streaming
	if sfuAuthEnabled() && hasPremiumCrew(ctx, nk, req.CrewID) {
		region := selectSFURegion("")
		endpoint := sfuEndpointForRegion(region)

		token, err := signSFUToken(SFUTokenClaims{
			UserID:    userID,
			SessionID: streamID,
			Type:      "stream",
			Role:      "host",
			CrewID:    req.CrewID,
			Region:    region,
		})
		if err != nil {
			logger.Error("Failed to sign SFU token: %v", err)
			// Fall through to P2P below
		} else {
			storeStreamSession(ctx, nk, streamID, StreamSessionMeta{
				CrewID:      req.CrewID,
				HostUserID:  userID,
				Mode:        "sfu",
				SFURegion:   region,
				SFUEndpoint: endpoint,
			})

			logger.Info("User %s started stream %s (SFU) in crew %s region=%s", userID, streamID, req.CrewID, region)
			resp, _ := json.Marshal(map[string]interface{}{
				"stream_id":    streamID,
				"session_id":   streamID,
				"mode":         "sfu",
				"sfu_endpoint": endpoint,
				"sfu_token":    token,
			})
			return string(resp), nil
		}
	}

	// P2P path (free crews or SFU auth not configured)
	storeStreamSession(ctx, nk, streamID, StreamSessionMeta{
		CrewID:     req.CrewID,
		HostUserID: userID,
		Mode:       "p2p",
	})

	logger.Info("User %s started stream %s (P2P) in crew %s", userID, streamID, req.CrewID)
	resp, _ := json.Marshal(map[string]interface{}{
		"stream_id":   streamID,
		"session_id":  streamID,
		"mode":        "p2p",
		"max_viewers": MaxP2PViewers,
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

	// Write stream_session event to the crew event ledger
	if streamMeta != nil {
		durationMin := 0
		if startedAt, parseErr := time.Parse(time.RFC3339, streamMeta.StartedAt); parseErr == nil {
			durationMin = int(time.Since(startedAt).Minutes())
			if durationMin < 1 {
				durationMin = 1
			}
		}
		event := CrewEvent{
			ID:        generateEventID(),
			CrewID:    req.CrewID,
			Type:      "stream_session",
			ActorID:   userID,
			Timestamp: time.Now().UnixMilli(),
			Score:     30,
			Data: StreamSessionData{
				StreamerID:   streamMeta.StreamerID,
				StreamerName: streamMeta.StreamerUsername,
				Title:        streamMeta.Title,
				DurationMin:  durationMin,
				PeakViewers:  len(streamMeta.ViewerIDs),
				ViewerIDs:    streamMeta.ViewerIDs,
			},
		}
		if appendErr := AppendCrewEvent(ctx, nk, req.CrewID, event); appendErr != nil {
			logger.Warn("Failed to write stream_session event for crew %s: %v", req.CrewID, appendErr)
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
	var streamWidth, streamHeight uint32
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
		}
	}

	if meta.Mode == "sfu" {
		token, err := signSFUToken(SFUTokenClaims{
			UserID:    userID,
			SessionID: req.SessionID,
			Type:      "stream",
			Role:      "viewer",
			CrewID:    meta.CrewID,
			Region:    meta.SFURegion,
		})
		if err != nil {
			return "", runtime.NewError("token signing failed", 13)
		}

		resp, _ := json.Marshal(map[string]interface{}{
			"mode":         "sfu",
			"sfu_endpoint": meta.SFUEndpoint,
			"sfu_token":    token,
			"width":        streamWidth,
			"height":       streamHeight,
		})
		return string(resp), nil
	}

	// P2P mode: viewer connects directly via signaling
	resp, _ := json.Marshal(map[string]interface{}{
		"mode":   "p2p",
		"width":  streamWidth,
		"height": streamHeight,
	})
	return string(resp), nil
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
// Stream session storage — tracks SFU vs P2P mode for active streams
// ---------------------------------------------------------------------------

type StreamSessionMeta struct {
	CrewID      string `json:"crew_id"`
	HostUserID  string `json:"host_user_id"`
	Mode        string `json:"mode"`
	SFURegion   string `json:"sfu_region,omitempty"`
	SFUEndpoint string `json:"sfu_endpoint,omitempty"`
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
