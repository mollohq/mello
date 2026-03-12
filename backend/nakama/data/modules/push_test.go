package main

import (
	"testing"
	"time"
)

func resetPushState() {
	subscriptionsMu.Lock()
	subscriptions = make(map[string]*UserSubscription)
	subscriptionsMu.Unlock()

	crewSubscribersMu.Lock()
	crewSubscribers = make(map[string]map[string]bool)
	crewSubscribersMu.Unlock()

	msgThrottle.mu.Lock()
	msgThrottle.lastPush = make(map[string]time.Time)
	msgThrottle.pending = make(map[string]*MessagePreview)
	msgThrottle.mu.Unlock()
}

func TestGetOrCreateSubscription_New(t *testing.T) {
	resetPushState()

	sub := getOrCreateSubscription("user_1", "session_a")
	if sub.UserID != "user_1" {
		t.Errorf("expected user_1, got %s", sub.UserID)
	}
	if sub.SessionID != "session_a" {
		t.Errorf("expected session_a, got %s", sub.SessionID)
	}
	if sub.ActiveCrew != "" {
		t.Errorf("expected empty active crew, got %s", sub.ActiveCrew)
	}
}

func TestGetOrCreateSubscription_Existing(t *testing.T) {
	resetPushState()

	sub1 := getOrCreateSubscription("user_1", "session_a")
	sub1.ActiveCrew = "crew_x"

	sub2 := getOrCreateSubscription("user_1", "session_a")
	if sub2.ActiveCrew != "crew_x" {
		t.Errorf("expected existing subscription to be returned, got active_crew=%s", sub2.ActiveCrew)
	}
}

func TestAddRemoveCrewSubscriber(t *testing.T) {
	resetPushState()

	addCrewSubscriber("crew_1", "session_a")
	addCrewSubscriber("crew_1", "session_b")

	crewSubscribersMu.RLock()
	subs := crewSubscribers["crew_1"]
	crewSubscribersMu.RUnlock()

	if len(subs) != 2 {
		t.Fatalf("expected 2 subscribers, got %d", len(subs))
	}

	removeCrewSubscriber("crew_1", "session_a")
	crewSubscribersMu.RLock()
	subs = crewSubscribers["crew_1"]
	crewSubscribersMu.RUnlock()

	if len(subs) != 1 {
		t.Fatalf("expected 1 subscriber after remove, got %d", len(subs))
	}

	removeCrewSubscriber("crew_1", "session_b")
	crewSubscribersMu.RLock()
	_, exists := crewSubscribers["crew_1"]
	crewSubscribersMu.RUnlock()

	if exists {
		t.Error("expected crew entry to be deleted when last subscriber removed")
	}
}

func TestCleanupSession(t *testing.T) {
	resetPushState()

	sub := getOrCreateSubscription("user_1", "session_a")
	sub.ActiveCrew = "crew_active"
	sub.SidebarCrews["crew_sidebar1"] = true
	sub.SidebarCrews["crew_sidebar2"] = true

	addCrewSubscriber("crew_active", "session_a")
	addCrewSubscriber("crew_sidebar1", "session_a")
	addCrewSubscriber("crew_sidebar2", "session_a")

	CleanupSession("session_a")

	// Subscription should be gone
	subscriptionsMu.RLock()
	_, exists := subscriptions["session_a"]
	subscriptionsMu.RUnlock()
	if exists {
		t.Error("expected subscription to be cleaned up")
	}

	// Crew subscribers should be cleaned up
	crewSubscribersMu.RLock()
	for _, crewID := range []string{"crew_active", "crew_sidebar1", "crew_sidebar2"} {
		if subs, ok := crewSubscribers[crewID]; ok && len(subs) > 0 {
			t.Errorf("expected crew %s subscribers to be cleaned up", crewID)
		}
	}
	crewSubscribersMu.RUnlock()
}

func TestPriorityEvents(t *testing.T) {
	expected := []string{
		"stream_started", "stream_ended",
		"voice_joined", "voice_left",
		"mention", "dm_received",
	}
	for _, e := range expected {
		if !PriorityEvents[e] {
			t.Errorf("expected %q to be a priority event", e)
		}
	}

	notPriority := []string{"message_sent", "typing", "presence_changed", ""}
	for _, e := range notPriority {
		if PriorityEvents[e] {
			t.Errorf("expected %q to NOT be a priority event", e)
		}
	}
}

func TestGetSubscribersForCrew(t *testing.T) {
	resetPushState()

	// User 1: active on crew_a, sidebar on crew_b
	sub1 := getOrCreateSubscription("user_1", "session_1")
	sub1.ActiveCrew = "crew_a"
	sub1.SidebarCrews["crew_b"] = true
	addCrewSubscriber("crew_a", "session_1")
	addCrewSubscriber("crew_b", "session_1")

	// User 2: active on crew_b, sidebar on crew_a
	sub2 := getOrCreateSubscription("user_2", "session_2")
	sub2.ActiveCrew = "crew_b"
	sub2.SidebarCrews["crew_a"] = true
	addCrewSubscriber("crew_b", "session_2")
	addCrewSubscriber("crew_a", "session_2")

	// All subscribers for crew_a: session_1 (active) + session_2 (sidebar)
	allA := getSubscribersForCrew("crew_a")
	if len(allA) != 2 {
		t.Fatalf("expected 2 subscribers for crew_a, got %d", len(allA))
	}

	// Active subscribers for crew_a: only session_1
	activeA := getActiveSubscribersForCrew("crew_a")
	if len(activeA) != 1 {
		t.Fatalf("expected 1 active subscriber for crew_a, got %d", len(activeA))
	}
	if activeA[0].UserID != "user_1" {
		t.Errorf("expected active subscriber to be user_1, got %s", activeA[0].UserID)
	}
}

func TestThrottleState(t *testing.T) {
	resetPushState()

	// Simulate message coming in with no recent push
	msg := &MessagePreview{Username: "alice", Preview: "hello", Timestamp: "2026-01-01T00:00:00Z"}

	msgThrottle.mu.Lock()
	msgThrottle.pending["crew_x"] = msg
	lastPush := msgThrottle.lastPush["crew_x"]
	shouldFlush := time.Since(lastPush) >= 10*time.Second
	msgThrottle.mu.Unlock()

	if !shouldFlush {
		t.Error("first message should be eligible for immediate push (no previous push)")
	}

	// Simulate a recent push
	msgThrottle.mu.Lock()
	msgThrottle.lastPush["crew_x"] = time.Now()
	msgThrottle.pending["crew_x"] = msg
	lastPush = msgThrottle.lastPush["crew_x"]
	shouldFlush = time.Since(lastPush) >= 10*time.Second
	msgThrottle.mu.Unlock()

	if shouldFlush {
		t.Error("message should be throttled when pushed recently")
	}
}
