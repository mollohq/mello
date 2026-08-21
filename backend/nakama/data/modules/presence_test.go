package main

import "testing"

func TestIsValidStatus(t *testing.T) {
	valid := []string{StatusOnline, StatusIdle, StatusDoNotDisturb, StatusOffline}
	for _, s := range valid {
		if !IsValidStatus(s) {
			t.Errorf("expected %q to be valid", s)
		}
	}

	invalid := []string{"", "busy", "away", "ONLINE", "invisible"}
	for _, s := range invalid {
		if IsValidStatus(s) {
			t.Errorf("expected %q to be invalid", s)
		}
	}
}

func TestIsValidActivityType(t *testing.T) {
	valid := []string{ActivityNone, ActivityInVoice, ActivityStreaming, ActivityWatching}
	for _, a := range valid {
		if !IsValidActivityType(a) {
			t.Errorf("expected %q to be valid", a)
		}
	}

	invalid := []string{"", "gaming", "afk", "STREAMING"}
	for _, a := range invalid {
		if IsValidActivityType(a) {
			t.Errorf("expected %q to be invalid", a)
		}
	}
}

func TestSessionCountTracksOverlappingSessions(t *testing.T) {
	resetSessionCountsForTests()
	t.Cleanup(resetSessionCountsForTests)

	if got := registerSessionStart("user_1"); got != 1 {
		t.Fatalf("first session start should return 1, got %d", got)
	}
	if got := registerSessionStart("user_1"); got != 2 {
		t.Fatalf("second session start should return 2, got %d", got)
	}
	if got := registerSessionEnd("user_1"); got != 1 {
		t.Fatalf("first session end should leave 1 active, got %d", got)
	}
	if got := registerSessionEnd("user_1"); got != 0 {
		t.Fatalf("second session end should leave 0 active, got %d", got)
	}
}

func TestSessionCountDoesNotGoNegative(t *testing.T) {
	resetSessionCountsForTests()
	t.Cleanup(resetSessionCountsForTests)

	if got := registerSessionEnd("user_2"); got != 0 {
		t.Fatalf("ending non-existent session should return 0, got %d", got)
	}
	if got := registerSessionStart("user_2"); got != 1 {
		t.Fatalf("session start should return 1, got %d", got)
	}
	if got := registerSessionEnd("user_2"); got != 0 {
		t.Fatalf("session end should return 0, got %d", got)
	}
	if got := registerSessionEnd("user_2"); got != 0 {
		t.Fatalf("repeated session end should stay at 0, got %d", got)
	}
}

// ---------------------------------------------------------------------------
// Partial presence updates (spec 17 §5.2 — game and voice coexist).
//
// The game sensor publishes only {game}. Before mergePresenceUpdate existed,
// an omitted activity reset it to "none", so starting a game silently kicked
// the user out of their voice activity.
// ---------------------------------------------------------------------------

func inVoice() *Activity {
	return &Activity{Type: ActivityInVoice, CrewID: "crew_x", ChannelID: "ch_1", ChannelName: "General"}
}

func playingCS() *GamePresence {
	return &GamePresence{GameID: "counter-strike-2", GameName: "Counter-Strike 2", StartedAt: "2026-08-15T20:00:00Z"}
}

func TestGameOnlyUpdatePreservesVoiceActivity(t *testing.T) {
	existing := &UserPresence{UserID: "u1", Status: StatusOnline, Activity: inVoice()}
	req := &PresenceUpdateRequest{Game: playingCS()}

	p := mergePresenceUpdate(existing, req, "u1", "now")

	if p.Activity == nil || p.Activity.Type != ActivityInVoice {
		t.Fatalf("game-only update wiped voice activity: %+v", p.Activity)
	}
	if p.Activity.ChannelName != "General" {
		t.Errorf("activity detail lost: %+v", p.Activity)
	}
	if p.Game == nil || p.Game.GameID != "counter-strike-2" {
		t.Errorf("game not set: %+v", p.Game)
	}
	if p.Status != StatusOnline {
		t.Errorf("status not preserved, got %q", p.Status)
	}
}

func TestClearGameKeepsActivity(t *testing.T) {
	existing := &UserPresence{UserID: "u1", Status: StatusOnline, Activity: inVoice(), Game: playingCS()}
	req := &PresenceUpdateRequest{ClearGame: true}

	p := mergePresenceUpdate(existing, req, "u1", "now")

	if p.Game != nil {
		t.Errorf("expected game cleared, got %+v", p.Game)
	}
	if p.Activity == nil || p.Activity.Type != ActivityInVoice {
		t.Errorf("clearing the game must not touch activity: %+v", p.Activity)
	}
}

func TestClearActivityStillResets(t *testing.T) {
	// Connect and logout rely on this: stale activity from a previous session
	// must not survive.
	existing := &UserPresence{UserID: "u1", Status: StatusOnline, Activity: inVoice()}
	req := &PresenceUpdateRequest{Status: StatusOnline, ClearActivity: true}

	p := mergePresenceUpdate(existing, req, "u1", "now")

	if p.Activity == nil || p.Activity.Type != ActivityNone {
		t.Errorf("expected activity reset to none, got %+v", p.Activity)
	}
}

func TestGameSurvivesAnActivityUpdate(t *testing.T) {
	// The mirror case: joining voice while a game is running must not drop
	// the game, or the crew sidebar loses it mid-session.
	existing := &UserPresence{UserID: "u1", Status: StatusOnline, Game: playingCS()}
	req := &PresenceUpdateRequest{Activity: inVoice()}

	p := mergePresenceUpdate(existing, req, "u1", "now")

	if p.Game == nil || p.Game.GameID != "counter-strike-2" {
		t.Errorf("activity update dropped the game: %+v", p.Game)
	}
	if p.Activity.Type != ActivityInVoice {
		t.Errorf("activity not applied: %+v", p.Activity)
	}
}

func TestFirstEverUpdateDefaults(t *testing.T) {
	p := mergePresenceUpdate(nil, &PresenceUpdateRequest{}, "u1", "now")

	if p.Status != StatusOnline {
		t.Errorf("expected default online, got %q", p.Status)
	}
	if p.Activity == nil || p.Activity.Type != ActivityNone {
		t.Errorf("expected default activity none, got %+v", p.Activity)
	}
	if p.Game != nil {
		t.Errorf("expected no game, got %+v", p.Game)
	}
}
