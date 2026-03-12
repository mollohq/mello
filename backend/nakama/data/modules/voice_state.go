package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"sync"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type VoiceMemberState struct {
	UserID   string `json:"user_id"`
	Username string `json:"username"`
	Speaking bool   `json:"speaking"`
	Muted    bool   `json:"muted"`
	Deafened bool   `json:"deafened"`
}

type VoiceRoom struct {
	CrewID  string                       `json:"crew_id"`
	Members map[string]*VoiceMemberState `json:"members"` // keyed by user_id
}

// VoiceSnapshot is a read-only view returned to callers.
type VoiceSnapshot struct {
	Active    bool                `json:"active"`
	MemberIDs []string           `json:"member_ids"`
	Members   []*VoiceMemberState `json:"members"`
}

// ---------------------------------------------------------------------------
// In-memory voice state (package-level)
// ---------------------------------------------------------------------------

var (
	voiceRooms   = make(map[string]*VoiceRoom) // crewID -> room
	voiceRoomsMu sync.RWMutex
	// Reverse map: userID -> crewID so we can find them on disconnect
	voiceUserCrew   = make(map[string]string)
	voiceUserCrewMu sync.RWMutex
)

// GetVoiceSnapshot returns a read-only snapshot for a crew's voice state.
func GetVoiceSnapshot(crewID string) *VoiceSnapshot {
	voiceRoomsMu.RLock()
	defer voiceRoomsMu.RUnlock()

	room, ok := voiceRooms[crewID]
	if !ok || len(room.Members) == 0 {
		return &VoiceSnapshot{Active: false, Members: []*VoiceMemberState{}}
	}

	snap := &VoiceSnapshot{
		Active:    true,
		MemberIDs: make([]string, 0, len(room.Members)),
		Members:   make([]*VoiceMemberState, 0, len(room.Members)),
	}
	for uid, m := range room.Members {
		snap.MemberIDs = append(snap.MemberIDs, uid)
		copy := *m
		snap.Members = append(snap.Members, &copy)
	}
	return snap
}

// ---------------------------------------------------------------------------
// RPCs
// ---------------------------------------------------------------------------

func VoiceJoinRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID string `json:"crew_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}

	// Verify membership
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	// Resolve username
	username := resolveUsername(ctx, nk, userID)

	// Leave any existing voice room first
	voiceLeaveInternal(ctx, logger, nk, userID)

	// Join the new room
	voiceRoomsMu.Lock()
	room, exists := voiceRooms[req.CrewID]
	if !exists {
		room = &VoiceRoom{
			CrewID:  req.CrewID,
			Members: make(map[string]*VoiceMemberState),
		}
		voiceRooms[req.CrewID] = room
	}
	room.Members[userID] = &VoiceMemberState{
		UserID:   userID,
		Username: username,
	}
	voiceRoomsMu.Unlock()

	voiceUserCrewMu.Lock()
	voiceUserCrew[userID] = req.CrewID
	voiceUserCrewMu.Unlock()

	// Update user presence activity
	now := time.Now().UTC().Format(time.RFC3339)
	_ = WritePresence(ctx, nk, &UserPresence{
		UserID:    userID,
		Status:    StatusOnline,
		LastSeen:  now,
		Activity:  &Activity{Type: ActivityInVoice, CrewID: req.CrewID},
		UpdatedAt: now,
	})

	InvalidateCrewState(req.CrewID)

	// Push priority event: voice_joined to all crew subscribers
	PushCrewEvent(ctx, logger, nk, req.CrewID, "voice_joined", map[string]interface{}{
		"user_id":  userID,
		"username": username,
	})
	// Push voice_update to active crew subscribers
	PushVoiceUpdate(ctx, logger, nk, req.CrewID)

	snap := GetVoiceSnapshot(req.CrewID)
	resp, _ := json.Marshal(map[string]interface{}{
		"success":     true,
		"voice_state": snap,
	})
	return string(resp), nil
}

func VoiceLeaveRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID string `json:"crew_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	voiceLeaveInternal(ctx, logger, nk, userID)

	return `{"success":true}`, nil
}

func VoiceSpeakingRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID   string `json:"crew_id"`
		Speaking bool   `json:"speaking"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	voiceRoomsMu.Lock()
	room, ok := voiceRooms[req.CrewID]
	if ok {
		if m, exists := room.Members[userID]; exists {
			m.Speaking = req.Speaking
		}
	}
	voiceRoomsMu.Unlock()

	// Push speaking update only to active crew subscribers (not sidebar)
	PushVoiceUpdate(ctx, logger, nk, req.CrewID)

	return `{"success":true}`, nil
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

func voiceLeaveInternal(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID string) {
	voiceUserCrewMu.Lock()
	crewID, wasInVoice := voiceUserCrew[userID]
	delete(voiceUserCrew, userID)
	voiceUserCrewMu.Unlock()

	if !wasInVoice {
		return
	}

	username := ""
	voiceRoomsMu.Lock()
	if room, ok := voiceRooms[crewID]; ok {
		if m, exists := room.Members[userID]; exists {
			username = m.Username
		}
		delete(room.Members, userID)
		if len(room.Members) == 0 {
			delete(voiceRooms, crewID)
		}
	}
	voiceRoomsMu.Unlock()

	// Reset presence activity
	now := time.Now().UTC().Format(time.RFC3339)
	_ = WritePresence(ctx, nk, &UserPresence{
		UserID:    userID,
		Status:    StatusOnline,
		LastSeen:  now,
		Activity:  &Activity{Type: ActivityNone},
		UpdatedAt: now,
	})

	InvalidateCrewState(crewID)

	PushCrewEvent(ctx, logger, nk, crewID, "voice_left", map[string]interface{}{
		"user_id":  userID,
		"username": username,
	})
	PushVoiceUpdate(ctx, logger, nk, crewID)
}

// VoiceCleanupUser removes a user from any voice room (called on disconnect).
func VoiceCleanupUser(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID string) {
	voiceLeaveInternal(ctx, logger, nk, userID)
}

// ---------------------------------------------------------------------------
// Utility helpers shared across modules
// ---------------------------------------------------------------------------

func isCrewMember(ctx context.Context, nk runtime.NakamaModule, crewID, userID string) bool {
	members, _, err := nk.GroupUsersList(ctx, crewID, 100, nil, "")
	if err != nil {
		return false
	}
	for _, m := range members {
		if m.GetUser().GetId() == userID {
			return true
		}
	}
	return false
}

func resolveUsername(ctx context.Context, nk runtime.NakamaModule, userID string) string {
	users, err := nk.UsersGetId(ctx, []string{userID}, nil)
	if err != nil || len(users) == 0 {
		return ""
	}
	return users[0].GetDisplayName()
}
