package main

import (
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/json"
	"fmt"
	"math/big"
	"sort"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const (
	ClipMaxDurationSec   = 60
	ClipMaxPerUserPerDay = 50
	TimelinePageSize     = 20

	CrewClipsCollection = "crew_clips"
	// CrewClipsMaxRetained caps the per-crew clips document. Clips live in a
	// single Nakama storage object which is hard-limited to 256KB. A StoredClip
	// with participant arrays serializes to roughly 400-600 bytes, so 250
	// entries keeps a safe margin under the limit. Older clips are trimmed;
	// unbounded history is future m3llo+ work.
	CrewClipsMaxRetained = 250
)

// ---------------------------------------------------------------------------
// Clip data model
// ---------------------------------------------------------------------------

type ClipData struct {
	ClipID           string   `json:"clip_id"`
	ClipType         string   `json:"clip_type"`
	ClipperName      string   `json:"clipper_name"`
	DurationSeconds  float64  `json:"duration_seconds"`
	Participants     []string `json:"participants,omitempty"`
	ParticipantNames []string `json:"participant_names,omitempty"`
	Game             string   `json:"game,omitempty"`
	LocalPath        string   `json:"local_path,omitempty"`
	MediaURL         string   `json:"media_url,omitempty"`
}

// ---------------------------------------------------------------------------
// Durable clips store — separate per-crew document, outside the ledger trim
// ---------------------------------------------------------------------------

type StoredClip struct {
	EventID          string   `json:"event_id"` // time-sortable, from generateEventID()
	ClipID           string   `json:"clip_id"`
	ActorID          string   `json:"actor_id"`
	Ts               int64    `json:"ts"`
	Score            int      `json:"score"`
	ClipType         string   `json:"clip_type"`
	ClipperName      string   `json:"clipper_name"`
	DurationSeconds  float64  `json:"duration_seconds"`
	Participants     []string `json:"participants,omitempty"`
	ParticipantNames []string `json:"participant_names,omitempty"`
	Game             string   `json:"game,omitempty"`
	LocalPath        string   `json:"local_path,omitempty"`
	MediaURL         string   `json:"media_url,omitempty"`
}

type CrewClipsDoc struct {
	CrewID    string       `json:"crew_id"`
	Clips     []StoredClip `json:"clips"` // newest appended last
	UpdatedAt int64        `json:"updated_at"`
}

func readClipsDoc(ctx context.Context, nk runtime.NakamaModule, crewID string) (*CrewClipsDoc, string) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: CrewClipsCollection, Key: crewID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return &CrewClipsDoc{CrewID: crewID, Clips: []StoredClip{}}, ""
	}

	var doc CrewClipsDoc
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &doc); err != nil {
		return &CrewClipsDoc{CrewID: crewID, Clips: []StoredClip{}}, ""
	}
	return &doc, objects[0].GetVersion()
}

func writeClipsDoc(ctx context.Context, nk runtime.NakamaModule, crewID string, doc *CrewClipsDoc, version string) error {
	data, err := json.Marshal(doc)
	if err != nil {
		return err
	}
	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      CrewClipsCollection,
			Key:             crewID,
			UserID:          SystemUserID,
			Value:           string(data),
			Version:         version,
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	})
	return err
}

// capClips trims a clip slice to the most-recent CrewClipsMaxRetained by
// timestamp. Pure (no I/O) so the cap behavior is unit-testable.
func capClips(clips []StoredClip) []StoredClip {
	if len(clips) <= CrewClipsMaxRetained {
		return clips
	}
	sort.Slice(clips, func(i, j int) bool { return clips[i].Ts < clips[j].Ts })
	return clips[len(clips)-CrewClipsMaxRetained:]
}

// AppendClip appends a clip to the durable clips doc, trims to the most-recent
// cap, and retries on version conflict.
func AppendClip(ctx context.Context, nk runtime.NakamaModule, crewID string, clip StoredClip) error {
	for attempt := 0; attempt < 3; attempt++ {
		doc, version := readClipsDoc(ctx, nk, crewID)
		doc.Clips = capClips(append(doc.Clips, clip))
		doc.UpdatedAt = time.Now().UnixMilli()
		if err := writeClipsDoc(ctx, nk, crewID, doc, version); err == nil {
			return nil
		}
		jitter, _ := rand.Int(rand.Reader, big.NewInt(50))
		time.Sleep(time.Duration(50*(attempt+1)+int(jitter.Int64())) * time.Millisecond)
	}
	return fmt.Errorf("crew_clips write failed after 3 retries for crew %s", crewID)
}

// ---------------------------------------------------------------------------
// PostClip RPC — store clip metadata in the crew event ledger
// ---------------------------------------------------------------------------

type PostClipRequest struct {
	CrewID          string   `json:"crew_id"`
	ClipID          string   `json:"clip_id"`
	ClipType        string   `json:"clip_type"`
	DurationSeconds float64  `json:"duration_seconds"`
	Participants    []string `json:"participants,omitempty"`
	Game            string   `json:"game,omitempty"`
	LocalPath       string   `json:"local_path,omitempty"`
}

func PostClipRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req PostClipRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" || req.ClipID == "" {
		return "", runtime.NewError("crew_id and clip_id required", 3)
	}
	if req.ClipType == "" {
		req.ClipType = "voice"
	}
	if req.DurationSeconds <= 0 || req.DurationSeconds > ClipMaxDurationSec {
		return "", runtime.NewError(fmt.Sprintf("duration must be 0-%d seconds", ClipMaxDurationSec), 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	// Rate limit: max clips per user per day, counted from the durable clips doc
	clipsDoc, _ := readClipsDoc(ctx, nk, req.CrewID)
	dayStart := time.Now().Truncate(24 * time.Hour).UnixMilli()
	clipCount := 0
	for _, c := range clipsDoc.Clips {
		if c.ActorID == userID && c.Ts >= dayStart {
			clipCount++
		}
	}
	if clipCount >= ClipMaxPerUserPerDay {
		return "", runtime.NewError("clip rate limit exceeded", 8)
	}

	username := resolveUsername(ctx, nk, userID)
	participantNames := make([]string, 0, len(req.Participants))
	for _, pid := range req.Participants {
		if n := resolveUsername(ctx, nk, pid); n != "" {
			participantNames = append(participantNames, n)
		}
	}

	eventID := generateEventID()
	clip := StoredClip{
		EventID:          eventID,
		ClipID:           req.ClipID,
		ActorID:          userID,
		Ts:               time.Now().UnixMilli(),
		Score:            50,
		ClipType:         req.ClipType,
		ClipperName:      username,
		DurationSeconds:  req.DurationSeconds,
		Participants:     req.Participants,
		ParticipantNames: participantNames,
		Game:             req.Game,
		LocalPath:        req.LocalPath,
	}

	if err := AppendClip(ctx, nk, req.CrewID, clip); err != nil {
		logger.Error("Failed to append clip: %v", err)
		return "", runtime.NewError("failed to save clip", 13)
	}

	logger.Info("User %s (%s) clipped in crew %s: clip_id=%s type=%s",
		userID, username, req.CrewID, req.ClipID, req.ClipType)

	resp, _ := json.Marshal(map[string]interface{}{
		"success":  true,
		"event_id": eventID,
		"clip_id":  req.ClipID,
	})
	return string(resp), nil
}

// ---------------------------------------------------------------------------
// CrewTimeline RPC — paginated feed of all crew events (newest first)
// ---------------------------------------------------------------------------

type TimelineRequest struct {
	CrewID string `json:"crew_id"`
	Cursor string `json:"cursor,omitempty"`
	Limit  int    `json:"limit,omitempty"`
}

type TimelineEntry struct {
	ID      string      `json:"id"`
	Type    string      `json:"type"`
	ActorID string      `json:"actor_id"`
	Ts      int64       `json:"ts"`
	Score   int         `json:"score"`
	Data    interface{} `json:"data"`
}

type TimelineResponse struct {
	CrewID  string          `json:"crew_id"`
	Entries []TimelineEntry `json:"entries"`
	Cursor  string          `json:"cursor,omitempty"`
	HasMore bool            `json:"has_more"`
}

func CrewTimelineRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req TimelineRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	limit := req.Limit
	if limit <= 0 || limit > TimelinePageSize {
		limit = TimelinePageSize
	}

	ledger, _ := readLedger(ctx, nk, req.CrewID)

	// Merge three bounded sources into the live feed: the 7-day ledger, recent
	// durable clips, and the single latest recap. Durable history beyond this
	// window is reached through crew_clips / crew_recaps.
	all := make([]TimelineEntry, 0, len(ledger.Events))
	for _, e := range ledger.Events {
		all = append(all, TimelineEntry{
			ID:      e.ID,
			Type:    e.Type,
			ActorID: e.ActorID,
			Ts:      e.Timestamp,
			Score:   e.Score,
			Data:    e.Data,
		})
	}

	clipCutoff := time.Now().Add(-time.Duration(EventRollingWindowDays) * 24 * time.Hour).UnixMilli()
	clipsDoc, _ := readClipsDoc(ctx, nk, req.CrewID)
	for _, c := range clipsDoc.Clips {
		if c.Ts < clipCutoff {
			continue
		}
		all = append(all, TimelineEntry{
			ID:      c.EventID,
			Type:    "clip",
			ActorID: c.ActorID,
			Ts:      c.Ts,
			Score:   c.Score,
			Data:    c,
		})
	}

	recapsDoc, _ := readRecapsDoc(ctx, nk, req.CrewID)
	if len(recapsDoc.Recaps) > 0 {
		latest := recapsDoc.Recaps[0]
		for _, r := range recapsDoc.Recaps[1:] {
			if r.WeekStart > latest.WeekStart {
				latest = r
			}
		}
		all = append(all, TimelineEntry{
			ID:      fmt.Sprintf("recap_%d", latest.WeekStart),
			Type:    "weekly_recap",
			ActorID: "",
			Ts:      latest.GeneratedAt,
			Score:   30,
			Data:    latest,
		})
	}

	// Sort by timestamp descending (newest first)
	sort.Slice(all, func(i, j int) bool {
		return all[i].Ts > all[j].Ts
	})

	// Cursor-based pagination: cursor is the entry ID of the last item on the previous page
	startIdx := 0
	if req.Cursor != "" {
		for i, e := range all {
			if e.ID == req.Cursor {
				startIdx = i + 1
				break
			}
		}
	}

	end := startIdx + limit
	if end > len(all) {
		end = len(all)
	}

	entries := all[startIdx:end]

	var cursor string
	hasMore := end < len(all)
	if hasMore && len(entries) > 0 {
		cursor = entries[len(entries)-1].ID
	}

	// Update last_seen for the user
	updateLastSeen(ctx, nk, userID, req.CrewID)

	resp := TimelineResponse{
		CrewID:  req.CrewID,
		Entries: entries,
		Cursor:  cursor,
		HasMore: hasMore,
	}
	data, _ := json.Marshal(resp)
	return string(data), nil
}

// ---------------------------------------------------------------------------
// ClipUploadURL RPC — return a presigned PUT URL for direct R2/S3 upload
// ---------------------------------------------------------------------------

type ClipUploadURLRequest struct {
	ClipID string `json:"clip_id"`
	CrewID string `json:"crew_id"`
}

type ClipUploadURLResponse struct {
	UploadURL string `json:"upload_url"`
	MediaURL  string `json:"media_url"`
}

func ClipUploadURLRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	_, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req ClipUploadURLRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.ClipID == "" || req.CrewID == "" {
		return "", runtime.NewError("clip_id and crew_id required", 3)
	}

	if !S3IsConfigured() {
		logger.Warn("clip_upload_url: S3 not configured, returning empty URLs")
		resp := ClipUploadURLResponse{}
		data, _ := json.Marshal(resp)
		return string(data), nil
	}

	key := fmt.Sprintf("crews/%s/%s.mp4", req.CrewID, req.ClipID)
	uploadURL, err := GeneratePresignedPUT(key, "audio/mp4", 15*time.Minute)
	if err != nil {
		logger.Error("clip_upload_url: presign failed: %v", err)
		return "", runtime.NewError("failed to generate upload URL", 13)
	}

	mediaURL := S3PublicURL(key)

	resp := ClipUploadURLResponse{
		UploadURL: uploadURL,
		MediaURL:  mediaURL,
	}
	data, _ := json.Marshal(resp)
	return string(data), nil
}

// ---------------------------------------------------------------------------
// ClipUploadComplete RPC — set media_url on clip event after successful upload
// ---------------------------------------------------------------------------

type ClipUploadCompleteRequest struct {
	ClipID string `json:"clip_id"`
	CrewID string `json:"crew_id"`
}

func ClipUploadCompleteRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req ClipUploadCompleteRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.ClipID == "" || req.CrewID == "" {
		return "", runtime.NewError("clip_id and crew_id required", 3)
	}

	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	key := fmt.Sprintf("crews/%s/%s.mp4", req.CrewID, req.ClipID)
	mediaURL := S3PublicURL(key)

	for attempt := 0; attempt < 3; attempt++ {
		doc, version := readClipsDoc(ctx, nk, req.CrewID)
		found := false
		for i := range doc.Clips {
			if doc.Clips[i].ClipID == req.ClipID {
				doc.Clips[i].MediaURL = mediaURL
				found = true
				break
			}
		}
		if !found {
			return "", runtime.NewError("clip not found", 5)
		}
		doc.UpdatedAt = time.Now().UnixMilli()
		if err := writeClipsDoc(ctx, nk, req.CrewID, doc, version); err == nil {
			break
		}
		if attempt == 2 {
			logger.Error("clip_upload_complete: writeClipsDoc failed after 3 retries")
			return "", runtime.NewError("failed to update clip media url", 13)
		}
		jitter, _ := rand.Int(rand.Reader, big.NewInt(50))
		time.Sleep(time.Duration(50*(attempt+1)+int(jitter.Int64())) * time.Millisecond)
	}

	logger.Info("Clip upload complete: crew=%s clip=%s media_url=%s", req.CrewID, req.ClipID, mediaURL)
	resp, _ := json.Marshal(map[string]interface{}{
		"success":   true,
		"media_url": mediaURL,
	})
	return string(resp), nil
}

// ---------------------------------------------------------------------------
// CrewClips RPC — paginated durable clip history (newest first)
// ---------------------------------------------------------------------------

type ClipsPageRequest struct {
	CrewID string `json:"crew_id"`
	Cursor string `json:"cursor,omitempty"` // last EventID of previous page
	Limit  int    `json:"limit,omitempty"`
}

type ClipsPageResponse struct {
	CrewID  string       `json:"crew_id"`
	Clips   []StoredClip `json:"clips"`
	Cursor  string       `json:"cursor,omitempty"`
	HasMore bool         `json:"has_more"`
}

func CrewClipsRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req ClipsPageRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	limit := req.Limit
	if limit <= 0 || limit > TimelinePageSize {
		limit = TimelinePageSize
	}

	doc, _ := readClipsDoc(ctx, nk, req.CrewID)

	sort.Slice(doc.Clips, func(i, j int) bool {
		return doc.Clips[i].Ts > doc.Clips[j].Ts
	})

	startIdx := 0
	if req.Cursor != "" {
		for i, c := range doc.Clips {
			if c.EventID == req.Cursor {
				startIdx = i + 1
				break
			}
		}
	}

	end := startIdx + limit
	if end > len(doc.Clips) {
		end = len(doc.Clips)
	}

	page := doc.Clips[startIdx:end]

	var cursor string
	hasMore := end < len(doc.Clips)
	if hasMore && len(page) > 0 {
		cursor = page[len(page)-1].EventID
	}

	resp := ClipsPageResponse{
		CrewID:  req.CrewID,
		Clips:   page,
		Cursor:  cursor,
		HasMore: hasMore,
	}
	data, _ := json.Marshal(resp)
	return string(data), nil
}
