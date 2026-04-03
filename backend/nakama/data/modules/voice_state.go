package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
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
	JoinedAt int64  `json:"joined_at"` // Unix millis
}

type VoiceRoom struct {
	ChannelID string                       `json:"channel_id"`
	CrewID    string                       `json:"crew_id"`
	Members   map[string]*VoiceMemberState `json:"members"` // keyed by user_id
}

// VoiceSnapshot is a read-only view returned to callers.
type VoiceSnapshot struct {
	ChannelID string              `json:"channel_id"`
	Active    bool                `json:"active"`
	MemberIDs []string            `json:"member_ids"`
	Members   []*VoiceMemberState `json:"members"`
}

// ---------------------------------------------------------------------------
// In-memory voice state (package-level)
// ---------------------------------------------------------------------------

var (
	voiceRooms   = make(map[string]*VoiceRoom) // channelID -> room
	voiceRoomsMu sync.RWMutex

	// Reverse maps
	voiceUserChannel   = make(map[string]string) // userID -> channelID
	voiceUserChannelMu sync.RWMutex

	voiceChannelCrew   = make(map[string]string) // channelID -> crewID
	voiceChannelCrewMu sync.RWMutex
)

// GetVoiceChannelSnapshot returns a read-only snapshot for a single voice channel.
func GetVoiceChannelSnapshot(channelID string) *VoiceSnapshot {
	voiceRoomsMu.RLock()
	defer voiceRoomsMu.RUnlock()

	room, ok := voiceRooms[channelID]
	if !ok || len(room.Members) == 0 {
		return &VoiceSnapshot{ChannelID: channelID, Active: false, Members: []*VoiceMemberState{}}
	}

	snap := &VoiceSnapshot{
		ChannelID: channelID,
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

// GetCrewVoiceSnapshots returns snapshots for all voice channels belonging to a crew.
func GetCrewVoiceSnapshots(ctx context.Context, nk runtime.NakamaModule, crewID string) []*VoiceSnapshot {
	list, err := GetVoiceChannels(ctx, nk, crewID)
	if err != nil || len(list.Channels) == 0 {
		return nil
	}

	snapshots := make([]*VoiceSnapshot, 0, len(list.Channels))
	for _, ch := range list.Channels {
		snapshots = append(snapshots, GetVoiceChannelSnapshot(ch.ID))
	}
	return snapshots
}

// GetVoiceSnapshot returns the legacy single-crew snapshot (picks the first active channel).
// Kept for backward compatibility during migration.
func GetVoiceSnapshot(crewID string) *VoiceSnapshot {
	voiceRoomsMu.RLock()
	defer voiceRoomsMu.RUnlock()

	// Find the first room belonging to this crew
	for _, room := range voiceRooms {
		if room.CrewID == crewID && len(room.Members) > 0 {
			snap := &VoiceSnapshot{
				ChannelID: room.ChannelID,
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
	}
	return &VoiceSnapshot{Active: false, Members: []*VoiceMemberState{}}
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
		CrewID    string `json:"crew_id"`
		ChannelID string `json:"channel_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}

	// Load channel list — needed to resolve default and channel name
	channelList, err := GetVoiceChannels(ctx, nk, req.CrewID)
	if err != nil || len(channelList.Channels) == 0 {
		return "", runtime.NewError("no voice channels for crew", 5)
	}

	// If no channel_id provided, use the default channel for this crew
	if req.ChannelID == "" {
		for _, ch := range channelList.Channels {
			if ch.IsDefault {
				req.ChannelID = ch.ID
				break
			}
		}
		if req.ChannelID == "" {
			req.ChannelID = channelList.Channels[0].ID
		}
	}

	// Resolve channel name
	channelName := ""
	for _, ch := range channelList.Channels {
		if ch.ID == req.ChannelID {
			channelName = ch.Name
			break
		}
	}

	// Verify membership
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	// Determine voice mode based on crew entitlement
	sfuMode := sfuAuthEnabled() && hasPremiumCrew(ctx, nk, req.CrewID)

	// Check capacity (SFU: 50, P2P: 6)
	maxMembers := MaxVoiceChannelMembers
	if sfuMode {
		maxMembers = MaxSFUVoiceChannelMembers
	}
	voiceRoomsMu.RLock()
	room, exists := voiceRooms[req.ChannelID]
	if exists && len(room.Members) >= maxMembers {
		voiceRoomsMu.RUnlock()
		return "", runtime.NewError(fmt.Sprintf("channel full (%d members max)", maxMembers), 9)
	}
	voiceRoomsMu.RUnlock()

	// Resolve username
	username := resolveUsername(ctx, nk, userID)

	// Leave any existing voice room first
	voiceLeaveInternal(ctx, logger, nk, userID)

	// Join the new room
	voiceRoomsMu.Lock()
	room, exists = voiceRooms[req.ChannelID]
	if !exists {
		room = &VoiceRoom{
			ChannelID: req.ChannelID,
			CrewID:    req.CrewID,
			Members:   make(map[string]*VoiceMemberState),
		}
		voiceRooms[req.ChannelID] = room
	}
	room.Members[userID] = &VoiceMemberState{
		UserID:   userID,
		Username: username,
		JoinedAt: time.Now().UnixMilli(),
	}
	voiceRoomsMu.Unlock()

	voiceUserChannelMu.Lock()
	voiceUserChannel[userID] = req.ChannelID
	voiceUserChannelMu.Unlock()

	voiceChannelCrewMu.Lock()
	voiceChannelCrew[req.ChannelID] = req.CrewID
	voiceChannelCrewMu.Unlock()

	// Track voice session for event ledger
	voiceSessionOnJoin(req.ChannelID, req.CrewID, channelName, userID, username)

	// Update last-seen for event ledger catch-up
	updateLastSeen(ctx, nk, userID, req.CrewID)

	// Update user presence activity
	now := time.Now().UTC().Format(time.RFC3339)
	_ = WritePresence(ctx, nk, &UserPresence{
		UserID:   userID,
		Status:   StatusOnline,
		LastSeen: now,
		Activity: &Activity{
			Type:        ActivityInVoice,
			CrewID:      req.CrewID,
			ChannelID:   req.ChannelID,
			ChannelName: channelName,
		},
		UpdatedAt: now,
	})

	InvalidateCrewState(req.CrewID)

	// Push priority event: voice_joined to all crew subscribers
	PushCrewEvent(ctx, logger, nk, req.CrewID, "voice_joined", map[string]interface{}{
		"user_id":      userID,
		"username":     username,
		"channel_id":   req.ChannelID,
		"channel_name": channelName,
	})
	// Push voice_update to active crew subscribers
	PushVoiceUpdate(ctx, logger, nk, req.CrewID)

	snap := GetVoiceChannelSnapshot(req.ChannelID)

	if sfuMode {
		region := selectSFURegion("")
		endpoint := sfuEndpointForRegion(region)
		voiceSessionKey := fmt.Sprintf("voice:%s:%s", req.CrewID, req.ChannelID)

		token, err := signSFUToken(SFUTokenClaims{
			UserID:    userID,
			SessionID: voiceSessionKey,
			Type:      "voice",
			Role:      "member",
			CrewID:    req.CrewID,
			ChannelID: req.ChannelID,
			Region:    region,
		})
		if err != nil {
			logger.Error("Failed to sign SFU token for voice: %v", err)
			// Fall through to P2P response
		} else {
			logger.Info("Voice join (SFU): user=%s crew=%s channel=%s region=%s", userID, req.CrewID, req.ChannelID, region)
			resp, _ := json.Marshal(map[string]interface{}{
				"success":      true,
				"channel_id":   req.ChannelID,
				"voice_state":  snap,
				"mode":         "sfu",
				"sfu_endpoint": endpoint,
				"sfu_token":    token,
			})
			return string(resp), nil
		}
	}

	logger.Info("Voice join (P2P): user=%s crew=%s channel=%s", userID, req.CrewID, req.ChannelID)
	resp, _ := json.Marshal(map[string]interface{}{
		"success":     true,
		"channel_id":  req.ChannelID,
		"voice_state": snap,
		"mode":        "p2p",
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

	// Resolve channel from user
	voiceUserChannelMu.RLock()
	channelID := voiceUserChannel[userID]
	voiceUserChannelMu.RUnlock()

	if channelID == "" {
		logger.Warn("voice_speaking: user %s not in any voice channel (voiceUserChannel empty)", userID)
		return `{"success":true}`, nil
	}

	logger.Debug("voice_speaking: user=%s channel=%s speaking=%v", userID, channelID, req.Speaking)

	voiceRoomsMu.Lock()
	room, ok := voiceRooms[channelID]
	if ok {
		if m, exists := room.Members[userID]; exists {
			m.Speaking = req.Speaking
		}
	}
	voiceRoomsMu.Unlock()

	// Resolve crew from channel for push
	crewID := req.CrewID
	if crewID == "" {
		voiceChannelCrewMu.RLock()
		crewID = voiceChannelCrew[channelID]
		voiceChannelCrewMu.RUnlock()
	}

	if crewID != "" {
		PushVoiceUpdate(ctx, logger, nk, crewID)
	}

	return `{"success":true}`, nil
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

// voiceLeaveInternalOpts controls optional behaviour of voiceLeaveInternal.
type voiceLeaveInternalOpts struct {
	// When true, skip the presence write back to StatusOnline. Used when the
	// caller (OnSessionEnd) has already written StatusOffline and we must not
	// overwrite it.
	skipPresenceWrite bool
}

func voiceLeaveInternal(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID string, opts ...voiceLeaveInternalOpts) {
	var opt voiceLeaveInternalOpts
	if len(opts) > 0 {
		opt = opts[0]
	}

	voiceUserChannelMu.Lock()
	channelID, wasInVoice := voiceUserChannel[userID]
	delete(voiceUserChannel, userID)
	voiceUserChannelMu.Unlock()

	if !wasInVoice {
		return
	}

	// Resolve crew
	voiceChannelCrewMu.RLock()
	crewID := voiceChannelCrew[channelID]
	voiceChannelCrewMu.RUnlock()

	username := ""
	lastMemberLeft := false
	voiceRoomsMu.Lock()
	if room, ok := voiceRooms[channelID]; ok {
		if m, exists := room.Members[userID]; exists {
			username = m.Username
		}
		delete(room.Members, userID)
		if len(room.Members) == 0 {
			lastMemberLeft = true
			delete(voiceRooms, channelID)
			voiceChannelCrewMu.Lock()
			delete(voiceChannelCrew, channelID)
			voiceChannelCrewMu.Unlock()
		}
	}
	voiceRoomsMu.Unlock()

	// Write voice_session event if this was the last member and session had 2+ participants
	if lastMemberLeft {
		if sess := voiceSessionOnLastLeave(channelID); sess != nil {
			participantIDs := make([]string, 0, len(sess.participants))
			participantNames := make([]string, 0, len(sess.participants))
			for uid, uname := range sess.participants {
				participantIDs = append(participantIDs, uid)
				participantNames = append(participantNames, uname)
			}
			durationMin := int(time.Since(sess.startTime).Minutes())
			if durationMin < 1 {
				durationMin = 1
			}
			event := CrewEvent{
				ID:        generateEventID(),
				CrewID:    sess.crewID,
				Type:      "voice_session",
				ActorID:   "",
				Timestamp: time.Now().UnixMilli(),
				Score:     20,
				Data: VoiceSessionData{
					ChannelID:        channelID,
					ChannelName:      sess.channelName,
					ParticipantIDs:   participantIDs,
					ParticipantNames: participantNames,
					DurationMin:      durationMin,
					PeakCount:        sess.peakCount,
				},
			}
			if err := AppendCrewEvent(ctx, nk, sess.crewID, event); err != nil {
				logger.Warn("Failed to write voice_session event for crew %s: %v", sess.crewID, err)
			}
		}
	}

	if !opt.skipPresenceWrite {
		now := time.Now().UTC().Format(time.RFC3339)
		_ = WritePresence(ctx, nk, &UserPresence{
			UserID:    userID,
			Status:    StatusOnline,
			LastSeen:  now,
			Activity:  &Activity{Type: ActivityNone},
			UpdatedAt: now,
		})
	}

	if crewID != "" {
		InvalidateCrewState(crewID)

		channelName := resolveChannelName(ctx, nk, crewID, channelID)
		PushCrewEvent(ctx, logger, nk, crewID, "voice_left", map[string]interface{}{
			"user_id":      userID,
			"username":     username,
			"channel_id":   channelID,
			"channel_name": channelName,
		})
		PushVoiceUpdate(ctx, logger, nk, crewID)
	}
}

// VoiceEvictChannel removes all users from a specific channel (used when channel is deleted).
func VoiceEvictChannel(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, channelID string) {
	voiceRoomsMu.RLock()
	room, ok := voiceRooms[channelID]
	if !ok {
		voiceRoomsMu.RUnlock()
		return
	}
	userIDs := make([]string, 0, len(room.Members))
	for uid := range room.Members {
		userIDs = append(userIDs, uid)
	}
	voiceRoomsMu.RUnlock()

	for _, uid := range userIDs {
		voiceLeaveInternal(ctx, logger, nk, uid)
	}
}

// VoiceCleanupUser removes a user from any voice room (called on disconnect).
// Skips the presence write since OnSessionEnd already set StatusOffline.
func VoiceCleanupUser(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID string) {
	voiceLeaveInternal(ctx, logger, nk, userID, voiceLeaveInternalOpts{skipPresenceWrite: true})
}

// StartVoiceRoomGC runs a background loop that prunes voice room members whose
// Nakama sessions are no longer active. This catches users that weren't cleaned
// up by OnSessionEnd (crashes, network drops, missed events).
func StartVoiceRoomGC(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, interval time.Duration) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for range ticker.C {
		voiceRoomGC(ctx, logger, nk)
	}
}

const voiceGCStalenessThreshold = 2 * time.Hour

func voiceRoomGC(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule) {
	voiceRoomsMu.RLock()
	var userIDs []string
	for _, room := range voiceRooms {
		for uid := range room.Members {
			userIDs = append(userIDs, uid)
		}
	}
	voiceRoomsMu.RUnlock()

	if len(userIDs) == 0 {
		return
	}

	removed := 0
	for _, uid := range userIDs {
		p, err := ReadPresence(ctx, nk, uid)
		if err != nil {
			continue
		}

		stale := false
		if p.Status == StatusOffline {
			stale = true
		} else if p.UpdatedAt != "" {
			// Catch ghost "online" presences that were never flipped to offline
			// (e.g. OnSessionEnd failed or never fired).
			if updatedAt, parseErr := time.Parse(time.RFC3339, p.UpdatedAt); parseErr == nil {
				if time.Since(updatedAt) > voiceGCStalenessThreshold {
					stale = true
				}
			}
		}

		if stale {
			logger.Info("Voice GC: removing stale member %s (status=%s, updated_at=%s)", uid, p.Status, p.UpdatedAt)
			voiceLeaveInternal(ctx, logger, nk, uid, voiceLeaveInternalOpts{skipPresenceWrite: true})

			// Also fix the stored presence if it's not already offline.
			if p.Status != StatusOffline {
				now := time.Now().UTC().Format(time.RFC3339)
				_ = WritePresence(ctx, nk, &UserPresence{
					UserID:    uid,
					Status:    StatusOffline,
					LastSeen:  now,
					Activity:  &Activity{Type: ActivityNone},
					UpdatedAt: now,
				})
			}
			removed++
		}
	}
	if removed > 0 {
		logger.Info("Voice GC: cleaned up %d stale members", removed)
	}
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
