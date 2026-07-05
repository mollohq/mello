package main

import "testing"

func gsCard(id string, wins, losses, draws, streak int) feedCard {
	return feedCard{
		id:          id,
		feedType:    "session",
		backendType: "game_session",
		data: GameSessionData{
			GameName:    "Counter-Strike 2",
			Wins:        wins,
			Losses:      losses,
			Draws:       draws,
			StreakAfter: streak,
		},
	}
}

func TestGameSessionQuality(t *testing.T) {
	// Routine 2-1 night, streak 2 → below the notable floor.
	if q := gameSessionQuality(gsCard("a", 2, 1, 0, 2)); q >= feedGameSessionNotableMin {
		t.Fatalf("routine session scored %d, want < %d", q, feedGameSessionNotableMin)
	}
	// 5-win streak, flawless 5-0 → notable.
	if q := gameSessionQuality(gsCard("b", 5, 0, 0, 5)); q < feedGameSessionNotableMin {
		t.Fatalf("heater session scored %d, want >= %d", q, feedGameSessionNotableMin)
	}
	// No telemetry outcomes → never notable.
	if q := gameSessionQuality(gsCard("c", 0, 0, 0, 0)); q != 0 {
		t.Fatalf("no-telemetry session scored %d, want 0", q)
	}
	// Non-game card → sentinel (never competes as a game session).
	if q := gameSessionQuality(feedCard{backendType: "clip"}); q != feedMinQuality {
		t.Fatalf("non-game scored %d, want sentinel", q)
	}
}

func TestPruneGameSessions_CapsAndDropsRoutine(t *testing.T) {
	cards := []feedCard{
		{id: "clip1", backendType: "clip"},
		gsCard("routine1", 2, 1, 0, 1),
		gsCard("heater", 5, 0, 0, 5),
		gsCard("routine2", 1, 2, 0, -1),
		gsCard("skid", 0, 6, 0, -3),
		gsCard("big", 4, 4, 2, 1),
		{id: "voice1", backendType: "voice_session"},
	}
	out := pruneGameSessions(cards)

	if !feedCardsContain(out, "clip1") || !feedCardsContain(out, "voice1") {
		t.Fatalf("non-game cards were dropped")
	}
	if feedCardsContain(out, "routine1") || feedCardsContain(out, "routine2") {
		t.Fatalf("routine game sessions were kept")
	}

	gameCount := 0
	for _, c := range out {
		if c.backendType == "game_session" {
			gameCount++
		}
	}
	if gameCount > feedGameSessionMaxCards {
		t.Fatalf("kept %d game sessions, want <= %d", gameCount, feedGameSessionMaxCards)
	}
}

func feedCardsContain(cards []feedCard, id string) bool {
	for _, c := range cards {
		if c.id == id {
			return true
		}
	}
	return false
}
