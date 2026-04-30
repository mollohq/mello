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
