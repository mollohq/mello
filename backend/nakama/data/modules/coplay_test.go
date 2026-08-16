package main

import "testing"

// ---------------------------------------------------------------------------
// Co-play attribution: who else was in the game at the same time.
// ---------------------------------------------------------------------------

func win(user, game string, startMs, endMs int64) sessionWindow {
	return sessionWindow{UserID: user, GameID: game, StartMs: startMs, EndMs: endMs}
}

const hour = int64(3_600_000)

func TestOverlappingSessionsAreCoPlay(t *testing.T) {
	me := win("me", "counter-strike-2", 0, 2*hour)
	others := []sessionWindow{
		win("kim", "counter-strike-2", hour, 3*hour),  // overlaps the back half
		win("ash", "counter-strike-2", -hour, hour/2), // overlaps the front
	}
	got := coPlayersFromLedger(me, others)
	if len(got) != 2 {
		t.Fatalf("expected both crewmates, got %v", got)
	}
}

func TestDifferentGameIsNotCoPlay(t *testing.T) {
	me := win("me", "counter-strike-2", 0, 2*hour)
	others := []sessionWindow{win("kim", "dota-2", 0, 2*hour)}
	if got := coPlayersFromLedger(me, others); len(got) != 0 {
		t.Errorf("playing a different game at the same time is not together: %v", got)
	}
}

func TestNonOverlappingSessionIsNotCoPlay(t *testing.T) {
	me := win("me", "counter-strike-2", 0, hour)
	others := []sessionWindow{win("kim", "counter-strike-2", 2*hour, 3*hour)}
	if got := coPlayersFromLedger(me, others); len(got) != 0 {
		t.Errorf("same game hours apart is not together: %v", got)
	}
}

func TestTouchingSessionsAreAHandoffNotCompany(t *testing.T) {
	// kim quits exactly as I start. We were never in the game together.
	me := win("me", "counter-strike-2", hour, 2*hour)
	others := []sessionWindow{win("kim", "counter-strike-2", 0, hour)}
	if got := coPlayersFromLedger(me, others); len(got) != 0 {
		t.Errorf("touching endpoints are not an overlap: %v", got)
	}
}

func TestActorIsNeverTheirOwnCoPlayer(t *testing.T) {
	me := win("me", "counter-strike-2", 0, 2*hour)
	// The actor's own earlier session of the same game overlaps trivially.
	others := []sessionWindow{win("me", "counter-strike-2", 0, 2*hour)}
	if got := coPlayersFromLedger(me, others); len(got) != 0 {
		t.Errorf("expected no self-attribution, got %v", got)
	}
}

func TestEachCoPlayerCountedOnce(t *testing.T) {
	// A crewmate with several short sessions across mine is still one person.
	me := win("me", "counter-strike-2", 0, 4*hour)
	others := []sessionWindow{
		win("kim", "counter-strike-2", 0, hour),
		win("kim", "counter-strike-2", 2*hour, 3*hour),
	}
	got := coPlayersFromLedger(me, others)
	if len(got) != 1 || got[0] != "kim" {
		t.Errorf("expected kim once, got %v", got)
	}
}

func TestLedgerWindowsSkipUnusableEvents(t *testing.T) {
	ledger := &CrewEventLedger{Events: []CrewEvent{
		{Type: "game_session", ActorID: "kim", Timestamp: 2 * hour,
			Data: GameSessionData{GameID: "counter-strike-2", DurationMin: 60}},
		// Zero duration: a zero-length window can never overlap, so including
		// it would silently drop the crewmate rather than fail loudly.
		{Type: "game_session", ActorID: "ash", Timestamp: 2 * hour,
			Data: GameSessionData{GameID: "counter-strike-2", DurationMin: 0}},
		// Legacy event with no stable id.
		{Type: "game_session", ActorID: "nav", Timestamp: 2 * hour,
			Data: GameSessionData{GameID: "", DurationMin: 60}},
		// Wrong type entirely.
		{Type: "voice_session", ActorID: "sam", Timestamp: 2 * hour,
			Data: GameSessionData{GameID: "counter-strike-2", DurationMin: 60}},
	}}
	windows := ledgerWindows(ledger, 0)
	if len(windows) != 1 || windows[0].UserID != "kim" {
		t.Fatalf("expected only kim's usable session, got %+v", windows)
	}
	// Start is derived from the end timestamp and the duration.
	if windows[0].StartMs != hour || windows[0].EndMs != 2*hour {
		t.Errorf("window derived wrong: %+v", windows[0])
	}
}

func TestLedgerWindowsRespectTheCutoff(t *testing.T) {
	ledger := &CrewEventLedger{Events: []CrewEvent{
		{Type: "game_session", ActorID: "kim", Timestamp: hour,
			Data: GameSessionData{GameID: "counter-strike-2", DurationMin: 60}},
	}}
	if got := ledgerWindows(ledger, 2*hour); len(got) != 0 {
		t.Errorf("events before the cutoff must be ignored: %+v", got)
	}
}

func TestNilLedgerIsSafe(t *testing.T) {
	if got := ledgerWindows(nil, 0); got != nil {
		t.Errorf("a missing ledger must not panic or invent windows: %+v", got)
	}
}
