package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"time"

	"github.com/heroiclabs/nakama-common/api"
	"github.com/heroiclabs/nakama-common/runtime"
)

// Status constants
const (
	StatusOnline       = "online"
	StatusIdle         = "idle"
	StatusDoNotDisturb = "dnd"
	StatusOffline      = "offline"
)

// Activity types
const (
	ActivityNone      = "none"
	ActivityInVoice   = "in_voice"
	ActivityStreaming  = "streaming"
	ActivityWatching  = "watching"
)

const PresenceCollection = "presence"

// UserPresence stored in Nakama storage: presence/{user_id}
type UserPresence struct {
	UserID    string    `json:"user_id"`
	Status    string    `json:"status"`
	LastSeen  string    `json:"last_seen"`
	Activity  *Activity `json:"activity,omitempty"`
	UpdatedAt string    `json:"updated_at"`
}

// Activity describes what a user is currently doing.
type Activity struct {
	Type        string `json:"type"`
	CrewID      string `json:"crew_id,omitempty"`
	ChannelID   string `json:"channel_id,omitempty"`
	ChannelName string `json:"channel_name,omitempty"`
	StreamID    string `json:"stream_id,omitempty"`
	StreamTitle string `json:"stream_title,omitempty"`
	StreamerID  string `json:"streamer_id,omitempty"`
}

func IsValidStatus(status string) bool {
	switch status {
	case StatusOnline, StatusIdle, StatusDoNotDisturb, StatusOffline:
		return true
	}
	return false
}

func IsValidActivityType(t string) bool {
	switch t {
	case ActivityNone, ActivityInVoice, ActivityStreaming, ActivityWatching:
		return true
	}
	return false
}

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

func WritePresence(ctx context.Context, nk runtime.NakamaModule, p *UserPresence) error {
	data, _ := json.Marshal(p)
	_, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      PresenceCollection,
			Key:             p.UserID,
			UserID:          p.UserID,
			Value:           string(data),
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	})
	return err
}

func ReadPresence(ctx context.Context, nk runtime.NakamaModule, userID string) (*UserPresence, error) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{
			Collection: PresenceCollection,
			Key:        userID,
			UserID:     userID,
		},
	})
	if err != nil {
		return nil, err
	}
	if len(objects) == 0 {
		return &UserPresence{
			UserID:   userID,
			Status:   StatusOffline,
			Activity: &Activity{Type: ActivityNone},
		}, nil
	}
	var p UserPresence
	if err := json.Unmarshal([]byte(objects[0].Value), &p); err != nil {
		return nil, err
	}
	return &p, nil
}

// ---------------------------------------------------------------------------
// RPCs
// ---------------------------------------------------------------------------

func PresenceUpdateRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		Status   string    `json:"status"`
		Activity *Activity `json:"activity,omitempty"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	if req.Status != "" && !IsValidStatus(req.Status) {
		return "", runtime.NewError("invalid status", 3)
	}
	if req.Activity != nil && !IsValidActivityType(req.Activity.Type) {
		return "", runtime.NewError("invalid activity type", 3)
	}

	now := time.Now().UTC().Format(time.RFC3339)

	existing, _ := ReadPresence(ctx, nk, userID)

	p := &UserPresence{
		UserID:    userID,
		Status:    req.Status,
		LastSeen:  now,
		Activity:  req.Activity,
		UpdatedAt: now,
	}
	if p.Status == "" && existing != nil {
		p.Status = existing.Status
	}
	if p.Status == "" {
		p.Status = StatusOnline
	}
	if p.Activity == nil {
		p.Activity = &Activity{Type: ActivityNone}
	}

	if err := WritePresence(ctx, nk, p); err != nil {
		logger.Error("failed to write presence: %v", err)
		return "", runtime.NewError("failed to update presence", 13)
	}

	NotifyPresenceChanged(ctx, logger, nk, userID, p)

	return `{"success":true}`, nil
}

func PresenceGetRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	_, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		UserIDs []string `json:"user_ids"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	presences := make(map[string]*UserPresence, len(req.UserIDs))
	for _, uid := range req.UserIDs {
		p, err := ReadPresence(ctx, nk, uid)
		if err != nil {
			logger.Warn("failed to read presence for %s: %v", uid, err)
			continue
		}
		presences[uid] = p
	}

	resp, _ := json.Marshal(map[string]interface{}{
		"presences": presences,
	})
	return string(resp), nil
}

// ---------------------------------------------------------------------------
// Session lifecycle hooks
// ---------------------------------------------------------------------------

func OnSessionStart(ctx context.Context, logger runtime.Logger, evt *api.Event) {
	props := evt.GetProperties()
	userID := props["user_id"]
	if userID == "" {
		return
	}

	now := time.Now().UTC().Format(time.RFC3339)
	p := &UserPresence{
		UserID:    userID,
		Status:    StatusOnline,
		LastSeen:  now,
		Activity:  &Activity{Type: ActivityNone},
		UpdatedAt: now,
	}

	if err := WritePresence(ctx, globalNk, p); err != nil {
		logger.Error("failed to set presence on session start for %s: %v", userID, err)
		return
	}

	NotifyPresenceChanged(ctx, logger, globalNk, userID, p)
	logger.Info("User %s session started, presence set to online", userID)
}

func OnSessionEnd(ctx context.Context, logger runtime.Logger, evt *api.Event) {
	props := evt.GetProperties()
	userID := props["user_id"]
	if userID == "" {
		return
	}

	now := time.Now().UTC().Format(time.RFC3339)
	p := &UserPresence{
		UserID:    userID,
		Status:    StatusOffline,
		LastSeen:  now,
		Activity:  &Activity{Type: ActivityNone},
		UpdatedAt: now,
	}

	if err := WritePresence(ctx, globalNk, p); err != nil {
		logger.Error("failed to set presence on session end for %s: %v", userID, err)
		return
	}

	// Clean up any voice room the user was in
	VoiceCleanupUser(ctx, logger, globalNk, userID)

	NotifyPresenceChanged(ctx, logger, globalNk, userID, p)
	logger.Info("User %s session ended, presence set to offline", userID)
}

// ---------------------------------------------------------------------------
// Cross-module notifications
// ---------------------------------------------------------------------------

func NotifyPresenceChanged(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID string, p *UserPresence) {
	groups, _, err := nk.UserGroupsList(ctx, userID, 100, nil, "")
	if err != nil {
		logger.Warn("failed to list user groups for presence notify: %v", err)
		return
	}

	for _, g := range groups {
		crewID := g.GetGroup().GetId()
		InvalidateCrewState(crewID)
		PushPresenceChange(ctx, logger, nk, crewID, userID, p)
	}
}
