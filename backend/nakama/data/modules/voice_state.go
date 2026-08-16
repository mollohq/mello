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
	// IsGuest marks a browser participant who joined from an invite link and is
	// not a crew member. Clients badge these so members can see that the voice
	// they hear belongs to someone who hasn't installed m3llo.
	IsGuest bool `json:"is_guest,omitempty"`
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

	voiceGCMissesMu sync.Mutex
	voiceGCMisses   = make(map[string]int) // userID -> consecutive stale detections
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

// upsertVoiceMember ensures userID is a member of channelID, creating the room
// if needed and PRESERVING an existing member's JoinedAt. It also keeps the
// channel->crew reverse map consistent. Used by the idempotent same-channel
// rejoin path (e.g. reconnects) so a member's roster entry is never recreated.
// Returns true if the member already existed.
func upsertVoiceMember(channelID, crewID, userID, username string, isGuest bool) bool {
	voiceRoomsMu.Lock()
	room, exists := voiceRooms[channelID]
	if !exists {
		room = &VoiceRoom{
			ChannelID: channelID,
			CrewID:    crewID,
			Members:   make(map[string]*VoiceMemberState),
		}
		voiceRooms[channelID] = room
	}
	m, existed := room.Members[userID]
	if existed {
		m.Username = username
		m.IsGuest = isGuest
	} else {
		room.Members[userID] = &VoiceMemberState{
			UserID:   userID,
			Username: username,
			JoinedAt: time.Now().UnixMilli(),
			IsGuest:  isGuest,
		}
	}
	voiceRoomsMu.Unlock()

	voiceChannelCrewMu.Lock()
	voiceChannelCrew[channelID] = crewID
	voiceChannelCrewMu.Unlock()

	return existed
}

// cleanupVoiceOnCrewExit removes a user from voice when they leave or are
// kicked from a crew, but ONLY if their current voice channel belongs to that
// crew (a user could be in another crew's voice). Prevents voice ghosts that
// otherwise linger until the staleness GC.
func cleanupVoiceOnCrewExit(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID, crewID string) {
	voiceUserChannelMu.RLock()
	ch := voiceUserChannel[userID]
	voiceUserChannelMu.RUnlock()
	if ch == "" {
		return
	}
	voiceChannelCrewMu.RLock()
	chCrew := voiceChannelCrew[ch]
	voiceChannelCrewMu.RUnlock()
	if chCrew != crewID {
		return
	}
	logger.Info("Cleaning up voice for user=%s leaving crew=%s (channel=%s)", userID, crewID, ch)
	voiceLeaveInternal(ctx, logger, nk, userID)
}

// ---------------------------------------------------------------------------
// RPCs
// ---------------------------------------------------------------------------

// resolveVoiceChannel picks the channel a caller should join and returns its ID
// and name. An empty requested ID resolves to the crew's default channel, or the
// first one if no default is flagged.
func resolveVoiceChannel(ctx context.Context, nk runtime.NakamaModule, crewID, requested string) (string, string, error) {
	channelList, err := GetVoiceChannels(ctx, nk, crewID)
	if err != nil || len(channelList.Channels) == 0 {
		return "", "", runtime.NewError("no voice channels for crew", 5)
	}

	if requested == "" {
		for _, ch := range channelList.Channels {
			if ch.IsDefault {
				requested = ch.ID
				break
			}
		}
		if requested == "" {
			requested = channelList.Channels[0].ID
		}
	}

	for _, ch := range channelList.Channels {
		if ch.ID == requested {
			return ch.ID, ch.Name, nil
		}
	}
	return requested, "", nil
}

// voiceJoinParams is everything joinVoiceRoom needs to seat a participant.
type voiceJoinParams struct {
	CrewID      string
	ChannelID   string
	ChannelName string
	UserID      string
	Username    string
	MaxMembers  int
	// IsGuest marks a browser participant joining from an invite link.
	IsGuest bool
}

// recordLedgerSession opens or extends the crew's ledger voice session for a
// participant. Guests are deliberately excluded: the ledger feeds the weekly
// recap, and a visitor who sat in voice for 40 minutes must not turn up as the
// crew's most active member.
func recordLedgerSession(p voiceJoinParams) {
	if p.IsGuest {
		return
	}
	voiceSessionOnJoin(p.ChannelID, p.CrewID, p.ChannelName, p.UserID, p.Username)
}

// joinVoiceRoom seats a participant in a voice room, updates presence and the
// event ledger, and broadcasts the roster change to the crew. Callers are
// responsible for authorization and for choosing MaxMembers; everything after
// that is identical for every kind of participant.
func joinVoiceRoom(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, p voiceJoinParams) (*VoiceSnapshot, error) {
	// Capacity. An existing member re-joining the same channel is already
	// counted, so don't reject them when the room is full.
	voiceRoomsMu.RLock()
	room, exists := voiceRooms[p.ChannelID]
	alreadyMember := false
	if exists {
		_, alreadyMember = room.Members[p.UserID]
	}
	if exists && !alreadyMember && len(room.Members) >= p.MaxMembers {
		voiceRoomsMu.RUnlock()
		return nil, runtime.NewError(fmt.Sprintf("channel full (%d members max)", p.MaxMembers), 9)
	}
	voiceRoomsMu.RUnlock()

	// Is this an idempotent re-join of the channel the user is already in
	// (e.g. a reconnect)? If so we must NOT churn the roster (no leave/join
	// broadcasts) and must preserve the original JoinedAt. This is the fix for
	// the roster flicker every other crew member saw on a peer's reconnect.
	voiceUserChannelMu.RLock()
	sameChannelRejoin := voiceUserChannel[p.UserID] == p.ChannelID
	voiceUserChannelMu.RUnlock()

	if sameChannelRejoin {
		// Ensure the member entry exists, preserve JoinedAt, refresh username.
		upsertVoiceMember(p.ChannelID, p.CrewID, p.UserID, p.Username, p.IsGuest)
		logger.Info("Voice re-join (idempotent): user=%s crew=%s channel=%s", p.UserID, p.CrewID, p.ChannelID)
	} else {
		// First join or channel switch: leave any prior room, then add.
		voiceLeaveInternal(ctx, logger, nk, p.UserID)

		voiceRoomsMu.Lock()
		room, exists = voiceRooms[p.ChannelID]
		if !exists {
			room = &VoiceRoom{
				ChannelID: p.ChannelID,
				CrewID:    p.CrewID,
				Members:   make(map[string]*VoiceMemberState),
			}
			voiceRooms[p.ChannelID] = room
		}
		room.Members[p.UserID] = &VoiceMemberState{
			UserID:   p.UserID,
			Username: p.Username,
			JoinedAt: time.Now().UnixMilli(),
			IsGuest:  p.IsGuest,
		}
		voiceRoomsMu.Unlock()

		voiceUserChannelMu.Lock()
		voiceUserChannel[p.UserID] = p.ChannelID
		voiceUserChannelMu.Unlock()

		voiceChannelCrewMu.Lock()
		voiceChannelCrew[p.ChannelID] = p.CrewID
		voiceChannelCrewMu.Unlock()

		recordLedgerSession(p)
	}

	// Update last-seen for event ledger catch-up. Guests have nothing to catch up
	// on — they cannot read the feed history — so skip the write.
	if !p.IsGuest {
		updateLastSeen(ctx, nk, p.UserID, p.CrewID)
	}

	// Update user presence activity (also corrects any drift after a reconnect)
	now := time.Now().UTC().Format(time.RFC3339)
	_ = WritePresence(ctx, nk, &UserPresence{
		UserID:   p.UserID,
		Status:   StatusOnline,
		LastSeen: now,
		Activity: &Activity{
			Type:        ActivityInVoice,
			CrewID:      p.CrewID,
			ChannelID:   p.ChannelID,
			ChannelName: p.ChannelName,
		},
		UpdatedAt: now,
	})

	InvalidateCrewState(p.CrewID)

	// Only broadcast roster churn on a genuine join/switch, never on an
	// idempotent rejoin (which would flicker every other member's UI).
	if !sameChannelRejoin {
		// Push priority event: voice_joined to all crew subscribers
		PushCrewEvent(ctx, logger, nk, p.CrewID, "voice_joined", map[string]interface{}{
			"user_id":      p.UserID,
			"username":     p.Username,
			"channel_id":   p.ChannelID,
			"channel_name": p.ChannelName,
		})
		// Push voice_update to active crew subscribers
		PushVoiceUpdate(ctx, logger, nk, p.CrewID)
		// Refresh Live Now for the crew's sidebar (non-active) subscribers.
		QueueSidebarVoiceDelta(logger, nk, p.CrewID)
	}

	return GetVoiceChannelSnapshot(p.ChannelID), nil
}

// issueVoiceSFUToken signs a short-lived SFU token for a voice participant.
// Returns ok=false when signing fails, leaving the caller to fall back to P2P.
func issueVoiceSFUToken(logger runtime.Logger, p voiceJoinParams) (endpoint, token string, ok bool) {
	region := selectSFURegion("")
	endpoint = sfuEndpointForRegion(region)
	voiceSessionKey := fmt.Sprintf("voice:%s:%s", p.CrewID, p.ChannelID)

	token, err := signSFUToken(SFUTokenClaims{
		UserID:    p.UserID,
		Username:  p.Username,
		SessionID: voiceSessionKey,
		Type:      "voice",
		Role:      "member",
		CrewID:    p.CrewID,
		ChannelID: p.ChannelID,
		Region:    region,
	})
	if err != nil {
		logger.Error("Failed to sign SFU token for voice: %v", err)
		return "", "", false
	}
	logger.Info("Voice SFU token issued: user=%s crew=%s channel=%s region=%s", p.UserID, p.CrewID, p.ChannelID, region)
	return endpoint, token, true
}

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

	channelID, channelName, err := resolveVoiceChannel(ctx, nk, req.CrewID, req.ChannelID)
	if err != nil {
		return "", err
	}

	// Verify membership
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	// Determine voice mode based on crew entitlement
	sfuMode := sfuAuthEnabled() && hasPremiumCrew(ctx, nk, req.CrewID)

	// Capacity differs by mode (SFU: 50, P2P: 6).
	maxMembers := MaxVoiceChannelMembers
	if sfuMode {
		maxMembers = MaxSFUVoiceChannelMembers
	}

	params := voiceJoinParams{
		CrewID:      req.CrewID,
		ChannelID:   channelID,
		ChannelName: channelName,
		UserID:      userID,
		Username:    resolveUsername(ctx, nk, userID),
		MaxMembers:  maxMembers,
	}

	snap, err := joinVoiceRoom(ctx, logger, nk, params)
	if err != nil {
		return "", err
	}

	if sfuMode {
		if endpoint, token, ok := issueVoiceSFUToken(logger, params); ok {
			resp, _ := json.Marshal(map[string]interface{}{
				"success":      true,
				"channel_id":   channelID,
				"voice_state":  snap,
				"mode":         "sfu",
				"sfu_endpoint": endpoint,
				"sfu_token":    token,
			})
			return string(resp), nil
		}
		// Signing failed — fall through to P2P.
	}

	logger.Info("Voice join (P2P): user=%s crew=%s channel=%s", userID, req.CrewID, channelID)
	resp, _ := json.Marshal(map[string]interface{}{
		"success":     true,
		"channel_id":  channelID,
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
		// Coalesced: speaking transitions can be very frequent.
		MarkVoiceDirty(crewID)
	}

	return `{"success":true}`, nil
}

func VoiceMuteStateRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID   string `json:"crew_id"`
		Muted    bool   `json:"muted"`
		Deafened bool   `json:"deafened"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	voiceUserChannelMu.RLock()
	channelID := voiceUserChannel[userID]
	voiceUserChannelMu.RUnlock()

	if channelID == "" {
		return `{"success":true}`, nil
	}

	voiceRoomsMu.Lock()
	room, ok := voiceRooms[channelID]
	if ok {
		if m, exists := room.Members[userID]; exists {
			m.Muted = req.Muted
			m.Deafened = req.Deafened
		}
	}
	voiceRoomsMu.Unlock()

	crewID := req.CrewID
	if crewID == "" {
		voiceChannelCrewMu.RLock()
		crewID = voiceChannelCrew[channelID]
		voiceChannelCrewMu.RUnlock()
	}

	if crewID != "" {
		// Coalesced: mute/deafen toggles can burst.
		MarkVoiceDirty(crewID)
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
		QueueSidebarVoiceDelta(logger, nk, crewID)
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

// Connected clients heartbeat presence every ~60s (presence_heartbeat), so a
// genuinely-dropped in-voice member goes stale well within this window while an
// idle-but-connected one stays fresh. Backstop for missed OnSessionEnd.
const voiceGCStalenessThreshold = 5 * time.Minute
const voiceGCOfflineGrace = 30 * time.Second
const voiceGCRequiredConsecutiveDetections = 2

func voiceRoomGC(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule) {
	voiceRoomsMu.RLock()
	var userIDs []string
	activeUsers := make(map[string]struct{})
	for _, room := range voiceRooms {
		for uid := range room.Members {
			userIDs = append(userIDs, uid)
			activeUsers[uid] = struct{}{}
		}
	}
	voiceRoomsMu.RUnlock()

	if len(userIDs) == 0 {
		return
	}
	voiceGCMissesMu.Lock()
	for uid := range voiceGCMisses {
		if _, ok := activeUsers[uid]; !ok {
			delete(voiceGCMisses, uid)
		}
	}
	voiceGCMissesMu.Unlock()

	removed := 0
	for _, uid := range userIDs {
		p, err := ReadPresence(ctx, nk, uid)
		if err != nil {
			continue
		}

		stale := false
		if p.Status == StatusOffline {
			offlineLongEnough := true
			if p.UpdatedAt != "" {
				if updatedAt, parseErr := time.Parse(time.RFC3339, p.UpdatedAt); parseErr == nil {
					offlineLongEnough = time.Since(updatedAt) > voiceGCOfflineGrace
				}
			}
			stale = offlineLongEnough
		} else if p.UpdatedAt != "" {
			// Catch ghost "online" presences that were never flipped to offline
			// (e.g. OnSessionEnd failed or never fired).
			if updatedAt, parseErr := time.Parse(time.RFC3339, p.UpdatedAt); parseErr == nil {
				if time.Since(updatedAt) > voiceGCStalenessThreshold {
					stale = true
				}
			}
		}

		if !stale {
			voiceGCMissesMu.Lock()
			delete(voiceGCMisses, uid)
			voiceGCMissesMu.Unlock()
			continue
		}

		voiceGCMissesMu.Lock()
		voiceGCMisses[uid]++
		misses := voiceGCMisses[uid]
		voiceGCMissesMu.Unlock()
		if misses < voiceGCRequiredConsecutiveDetections {
			continue
		}

		logger.Info(
			"Voice GC: removing stale member %s (status=%s, updated_at=%s, misses=%d)",
			uid,
			p.Status,
			p.UpdatedAt,
			misses,
		)
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
		voiceGCMissesMu.Lock()
		delete(voiceGCMisses, uid)
		voiceGCMissesMu.Unlock()
		removed++
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
