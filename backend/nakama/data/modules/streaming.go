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
}

const MaxP2PViewers = 5

type StopStreamRequest struct {
	CrewID string `json:"crew_id"`
}

type ActiveStream struct {
	HostID    string `json:"host_id"`
	HostName  string `json:"host_name"`
	Title     string `json:"title"`
	StartedAt int64  `json:"started_at"`
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

	logger.Info("User %s started stream %s in crew %s", userID, streamID, req.CrewID)
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
