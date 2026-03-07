package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

type StartStreamRequest struct {
	CrewID string `json:"crew_id"`
	Title  string `json:"title,omitempty"`
}

type StopStreamRequest struct {
	CrewID string `json:"crew_id"`
}

type ActiveStream struct {
	HostID    string `json:"host_id"`
	HostName  string `json:"host_name"`
	Title     string `json:"title"`
	StartedAt int64  `json:"started_at"`
}

const (
	StreamCollection    = "active_streams"
	NotifyStreamStart   = 100
	NotifyStreamEnd     = 101
)

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

	stream := ActiveStream{
		HostID:    userID,
		HostName:  users[0].GetDisplayName(),
		Title:     req.Title,
		StartedAt: time.Now().Unix(),
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

	content := map[string]interface{}{
		"crew_id":   req.CrewID,
		"host_id":   userID,
		"host_name": users[0].GetDisplayName(),
		"title":     req.Title,
	}
	for _, m := range members {
		mid := m.GetUser().GetId()
		if mid == userID {
			continue
		}
		if err := nk.NotificationSend(ctx, mid, "Stream started", content, NotifyStreamStart, userID, false); err != nil {
			logger.Warn("failed to notify user %s: %v", mid, err)
		}
	}

	logger.Info("User %s started stream in crew %s", userID, req.CrewID)
	return "{}", nil
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

	if err := nk.StorageDelete(ctx, []*runtime.StorageDelete{
		{
			Collection: StreamCollection,
			Key:        req.CrewID,
			UserID:     userID,
		},
	}); err != nil {
		logger.Warn("failed to delete stream state: %v", err)
	}

	members, _, err := nk.GroupUsersList(ctx, req.CrewID, 100, nil, "")
	if err == nil {
		content := map[string]interface{}{
			"crew_id": req.CrewID,
			"host_id": userID,
		}
		for _, m := range members {
			mid := m.GetUser().GetId()
			if mid == userID {
				continue
			}
			nk.NotificationSend(ctx, mid, "Stream ended", content, NotifyStreamEnd, userID, false)
		}
	}

	logger.Info("User %s stopped stream in crew %s", userID, req.CrewID)
	return "{}", nil
}
