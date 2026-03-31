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

// Notification codes for real-time pushes
const (
	NotifyCrewState      = 110
	NotifyCrewEvent      = 111
	NotifySidebarUpdate  = 112
	NotifyPresenceChange = 113
	NotifyVoiceUpdate    = 114
	NotifyMessagePreview = 115
)

// Priority events are always pushed immediately to all subscribers.
var PriorityEvents = map[string]bool{
	"stream_started":  true,
	"stream_ended":    true,
	"voice_joined":    true,
	"voice_left":      true,
	"mention":         true,
	"dm_received":     true,
	"channel_created": true,
	"channel_renamed": true,
	"channel_deleted": true,
}

// ---------------------------------------------------------------------------
// Subscription state (per-user, in-memory)
// ---------------------------------------------------------------------------

type UserSubscription struct {
	UserID       string
	ActiveCrew   string
	SidebarCrews map[string]bool // set of crew IDs
}

var (
	subscriptions   = make(map[string]*UserSubscription) // userID -> subscription
	subscriptionsMu sync.RWMutex

	// Reverse index: crewID -> set of userIDs subscribed (active or sidebar)
	crewSubscribers   = make(map[string]map[string]bool)
	crewSubscribersMu sync.RWMutex
)

func getOrCreateSubscription(userID string) *UserSubscription {
	subscriptionsMu.Lock()
	defer subscriptionsMu.Unlock()

	sub, ok := subscriptions[userID]
	if !ok {
		sub = &UserSubscription{
			UserID:       userID,
			SidebarCrews: make(map[string]bool),
		}
		subscriptions[userID] = sub
	}
	return sub
}

func addCrewSubscriber(crewID, userID string) {
	crewSubscribersMu.Lock()
	defer crewSubscribersMu.Unlock()
	if crewSubscribers[crewID] == nil {
		crewSubscribers[crewID] = make(map[string]bool)
	}
	crewSubscribers[crewID][userID] = true
}

func removeCrewSubscriber(crewID, userID string) {
	crewSubscribersMu.Lock()
	defer crewSubscribersMu.Unlock()
	if subs, ok := crewSubscribers[crewID]; ok {
		delete(subs, userID)
		if len(subs) == 0 {
			delete(crewSubscribers, crewID)
		}
	}
}

// CleanupUser removes all subscription state for a user (called on disconnect).
func CleanupUser(userID string) {
	subscriptionsMu.Lock()
	sub, ok := subscriptions[userID]
	if ok {
		delete(subscriptions, userID)
	}
	subscriptionsMu.Unlock()

	if !ok {
		return
	}

	if sub.ActiveCrew != "" {
		removeCrewSubscriber(sub.ActiveCrew, userID)
	}
	for crewID := range sub.SidebarCrews {
		removeCrewSubscriber(crewID, userID)
	}
}

// ---------------------------------------------------------------------------
// Subscription RPCs
// ---------------------------------------------------------------------------

func SetActiveCrewRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
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

	sub := getOrCreateSubscription(userID)

	subscriptionsMu.Lock()
	if sub.ActiveCrew != "" && sub.ActiveCrew != req.CrewID {
		removeCrewSubscriber(sub.ActiveCrew, userID)
	}
	sub.ActiveCrew = req.CrewID
	subscriptionsMu.Unlock()

	addCrewSubscriber(req.CrewID, userID)

	// Update last-seen for event ledger catch-up
	updateLastSeen(ctx, nk, userID, req.CrewID)

	crewSubscribersMu.RLock()
	subCount := len(crewSubscribers[req.CrewID])
	crewSubscribersMu.RUnlock()
	logger.Debug("SetActiveCrew: user=%s crew=%s subscribers=%d", userID, req.CrewID, subCount)

	// Return full state immediately
	state, err := ComputeCrewState(ctx, logger, nk, req.CrewID, true, userID)
	if err != nil {
		return "", err
	}

	resp, _ := json.Marshal(map[string]interface{}{
		"success": true,
		"state":   state,
	})
	return string(resp), nil
}

func SubscribeSidebarRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewIDs []string `json:"crew_ids"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	sub := getOrCreateSubscription(userID)

	subscriptionsMu.Lock()
	for crewID := range sub.SidebarCrews {
		removeCrewSubscriber(crewID, userID)
	}
	sub.SidebarCrews = make(map[string]bool, len(req.CrewIDs))
	for _, cid := range req.CrewIDs {
		sub.SidebarCrews[cid] = true
		addCrewSubscriber(cid, userID)
	}
	subscriptionsMu.Unlock()

	// Return initial sidebar state
	crews := make([]*CrewSidebarState, 0, len(req.CrewIDs))
	for _, cid := range req.CrewIDs {
		state, err := ComputeCrewState(ctx, logger, nk, cid, false, "")
		if err != nil {
			logger.Warn("failed to compute sidebar state for %s: %v", cid, err)
			continue
		}
		crews = append(crews, state.ToSidebar())
	}

	resp, _ := json.Marshal(map[string]interface{}{
		"success": true,
		"crews":   crews,
	})
	return string(resp), nil
}

// ---------------------------------------------------------------------------
// Push helpers
// ---------------------------------------------------------------------------

// getSubscribersForCrew returns all users subscribed to a crew (active or sidebar).
func getSubscribersForCrew(crewID string) []*UserSubscription {
	crewSubscribersMu.RLock()
	userIDs := crewSubscribers[crewID]
	crewSubscribersMu.RUnlock()

	subscriptionsMu.RLock()
	defer subscriptionsMu.RUnlock()

	subs := make([]*UserSubscription, 0, len(userIDs))
	for uid := range userIDs {
		if sub, ok := subscriptions[uid]; ok {
			subs = append(subs, sub)
		}
	}
	return subs
}

// getActiveSubscribersForCrew returns users where this crew is the active crew.
func getActiveSubscribersForCrew(crewID string) []*UserSubscription {
	all := getSubscribersForCrew(crewID)
	active := make([]*UserSubscription, 0)
	for _, sub := range all {
		if sub.ActiveCrew == crewID {
			active = append(active, sub)
		}
	}
	return active
}

// pushNotification sends a notification to a user.
func pushNotification(ctx context.Context, nk runtime.NakamaModule, userID string, code int, content map[string]interface{}) {
	if err := nk.NotificationSend(ctx, userID, "update", content, code, "", false); err != nil {
		fmt.Printf("pushNotification FAILED: user=%s code=%d err=%v\n", userID, code, err)
	}
}

// PushCrewEvent sends a priority event to all crew subscribers immediately.
func PushCrewEvent(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID, event string, data map[string]interface{}) {
	content := map[string]interface{}{
		"type":    "crew_event",
		"crew_id": crewID,
		"event":   event,
		"data":    data,
	}

	subs := getSubscribersForCrew(crewID)
	seen := make(map[string]bool, len(subs))
	for _, sub := range subs {
		if seen[sub.UserID] {
			continue
		}
		seen[sub.UserID] = true
		pushNotification(ctx, nk, sub.UserID, NotifyCrewEvent, content)
	}
}

// PushPresenceChange sends a presence change to active crew subscribers only.
func PushPresenceChange(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID, userID string, p *UserPresence) {
	content := map[string]interface{}{
		"type":    "presence_change",
		"crew_id": crewID,
		"user_id": userID,
		"presence": map[string]interface{}{
			"status":   p.Status,
			"activity": p.Activity,
		},
	}

	subs := getActiveSubscribersForCrew(crewID)
	seen := make(map[string]bool, len(subs))
	for _, sub := range subs {
		if seen[sub.UserID] || sub.UserID == userID {
			continue
		}
		seen[sub.UserID] = true
		pushNotification(ctx, nk, sub.UserID, NotifyPresenceChange, content)
	}
}

// PushVoiceUpdate sends voice state to active crew subscribers (includes speaking).
// Sends per-channel voice data so clients can render multi-channel state.
func PushVoiceUpdate(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID string) {
	// Build per-channel voice data
	channelDefs, _ := GetVoiceChannels(ctx, nk, crewID)
	var channels []map[string]interface{}
	if channelDefs != nil && len(channelDefs.Channels) > 0 {
		channels = make([]map[string]interface{}, 0, len(channelDefs.Channels))
		for _, ch := range channelDefs.Channels {
			snap := GetVoiceChannelSnapshot(ch.ID)
			members := make([]map[string]interface{}, 0, len(snap.Members))
			for _, m := range snap.Members {
				members = append(members, map[string]interface{}{
					"user_id":  m.UserID,
					"username": m.Username,
					"speaking": m.Speaking,
				})
			}
			channels = append(channels, map[string]interface{}{
				"id":         ch.ID,
				"name":       ch.Name,
				"is_default": ch.IsDefault,
				"members":    members,
			})
		}
	}

	// Legacy flat members list (first active channel) for backward compat
	legacySnap := GetVoiceSnapshot(crewID)
	legacyMembers := make([]map[string]interface{}, 0, len(legacySnap.Members))
	for _, m := range legacySnap.Members {
		legacyMembers = append(legacyMembers, map[string]interface{}{
			"user_id":  m.UserID,
			"username": m.Username,
			"speaking": m.Speaking,
		})
	}

	content := map[string]interface{}{
		"type":           "voice_update",
		"crew_id":        crewID,
		"members":        legacyMembers,
		"voice_channels": channels,
	}

	subs := getActiveSubscribersForCrew(crewID)
	seen := make(map[string]bool, len(subs))
	for _, sub := range subs {
		if seen[sub.UserID] {
			continue
		}
		seen[sub.UserID] = true
		pushNotification(ctx, nk, sub.UserID, NotifyVoiceUpdate, content)
	}
}

// ---------------------------------------------------------------------------
// Message throttling
// ---------------------------------------------------------------------------

type throttleState struct {
	lastPush map[string]time.Time      // crewID -> last push time
	pending  map[string]*MessagePreview // crewID -> latest pending message
	mu       sync.Mutex
}

var msgThrottle = &throttleState{
	lastPush: make(map[string]time.Time),
	pending:  make(map[string]*MessagePreview),
}

// QueueMessagePreview queues a message preview for sidebar push (10s throttle).
func QueueMessagePreview(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID string, msg *MessagePreview) {
	msgThrottle.mu.Lock()
	defer msgThrottle.mu.Unlock()

	msgThrottle.pending[crewID] = msg

	lastPush := msgThrottle.lastPush[crewID]
	if time.Since(lastPush) >= 10*time.Second {
		pushMessagePreviewNow(ctx, logger, nk, crewID, msg)
		msgThrottle.lastPush[crewID] = time.Now()
		delete(msgThrottle.pending, crewID)
	}
}

func pushMessagePreviewNow(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID string, msg *MessagePreview) {
	// Get the last 2 messages from the buffer
	crewRecentMsgsMu.RLock()
	msgs := crewRecentMsgs[crewID]
	crewRecentMsgsMu.RUnlock()

	content := map[string]interface{}{
		"type":     "message_preview",
		"crew_id":  crewID,
		"messages": msgs,
	}

	// Push to sidebar subscribers only (not active crew — they get full messages)
	subs := getSubscribersForCrew(crewID)
	seen := make(map[string]bool, len(subs))
	for _, sub := range subs {
		if sub.ActiveCrew == crewID {
			continue // active crew gets full messages, skip preview
		}
		if seen[sub.UserID] {
			continue
		}
		seen[sub.UserID] = true
		pushNotification(ctx, nk, sub.UserID, NotifyMessagePreview, content)
	}
}

// ---------------------------------------------------------------------------
// Sidebar batcher
// ---------------------------------------------------------------------------

type sidebarBatcher struct {
	mu sync.Mutex
}

var sidebar = &sidebarBatcher{}

// FlushSidebarUpdates sends batched sidebar updates to all subscribers.
func FlushSidebarUpdates(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule) {
	subscriptionsMu.RLock()
	// Collect all subscriptions snapshot
	allSubs := make([]*UserSubscription, 0, len(subscriptions))
	for _, sub := range subscriptions {
		allSubs = append(allSubs, sub)
	}
	subscriptionsMu.RUnlock()

	// Group by user (deduplicate across sessions)
	type userSidebar struct {
		userID  string
		crewIDs []string
	}
	userMap := make(map[string]*userSidebar)
	for _, sub := range allSubs {
		if len(sub.SidebarCrews) == 0 {
			continue
		}
		us, ok := userMap[sub.UserID]
		if !ok {
			us = &userSidebar{userID: sub.UserID}
			userMap[sub.UserID] = us
		}
		for cid := range sub.SidebarCrews {
			us.crewIDs = append(us.crewIDs, cid)
		}
	}

	for _, us := range userMap {
		// Deduplicate crew IDs
		seen := make(map[string]bool)
		crews := make([]interface{}, 0)
		for _, cid := range us.crewIDs {
			if seen[cid] {
				continue
			}
			seen[cid] = true

			state, err := ComputeCrewState(ctx, logger, nk, cid, false, "")
			if err != nil {
				continue
			}
			crews = append(crews, state.ToSidebar())
		}

		if len(crews) == 0 {
			continue
		}

		content := map[string]interface{}{
			"type":  "sidebar_update",
			"crews": crews,
		}
		pushNotification(ctx, nk, us.userID, NotifySidebarUpdate, content)
	}
}

// FlushThrottledMessages pushes any pending throttled message previews.
func FlushThrottledMessages(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule) {
	msgThrottle.mu.Lock()
	now := time.Now()
	var toFlush []string
	for crewID, msg := range msgThrottle.pending {
		if msg != nil && now.Sub(msgThrottle.lastPush[crewID]) >= 10*time.Second {
			toFlush = append(toFlush, crewID)
		}
	}
	for _, crewID := range toFlush {
		pushMessagePreviewNow(ctx, logger, nk, crewID, msgThrottle.pending[crewID])
		msgThrottle.lastPush[crewID] = now
		delete(msgThrottle.pending, crewID)
	}
	msgThrottle.mu.Unlock()
}

// ---------------------------------------------------------------------------
// Background loops (started from InitModule)
// ---------------------------------------------------------------------------

func StartSidebarBatchLoop(nk runtime.NakamaModule, logger runtime.Logger, interval time.Duration) {
	ticker := time.NewTicker(interval)
	ctx := context.Background()
	for range ticker.C {
		FlushSidebarUpdates(ctx, logger, nk)
	}
}

func StartMessageThrottleLoop(nk runtime.NakamaModule, logger runtime.Logger, interval time.Duration) {
	ticker := time.NewTicker(interval)
	ctx := context.Background()
	for range ticker.C {
		FlushThrottledMessages(ctx, logger, nk)
	}
}
