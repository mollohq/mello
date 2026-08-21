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

func gsCardT0(id string, durationMin int, playerIDs []string, wins, losses, draws, streak int) feedCard {
	return feedCard{
		id:          id,
		feedType:    "session",
		backendType: "game_session",
		data: GameSessionData{
			GameName:    "Valorant",
			DurationMin: durationMin,
			PlayerIDs:   playerIDs,
			Wins:        wins,
			Losses:      losses,
			Draws:       draws,
			StreakAfter: streak,
		},
	}
}

func TestGameSessionNotableFloor(t *testing.T) {
	cases := []struct {
		count int
		want  int
	}{
		{0, 10},
		{1, 10},
		{5, 10},
		{6, 30},
		{15, 30},
		{16, feedGameSessionNotableMin},
		{100, feedGameSessionNotableMin},
	}
	for _, tc := range cases {
		if got := gameSessionNotableFloor(tc.count); got != tc.want {
			t.Fatalf("floor(%d) = %d, want %d", tc.count, got, tc.want)
		}
	}
}

func TestGameSessionCardCap(t *testing.T) {
	cases := []struct {
		count int
		want  int
	}{
		{0, 4},
		{1, 4},
		{5, 4},
		{6, feedGameSessionMaxCards},
		{100, feedGameSessionMaxCards},
	}
	for _, tc := range cases {
		if got := gameSessionCardCap(tc.count); got != tc.want {
			t.Fatalf("cap(%d) = %d, want %d", tc.count, got, tc.want)
		}
	}
}

func TestGameSessionQuality(t *testing.T) {
	// Routine 2-1 night, streak 2 → below the default notable floor (no T0 data).
	if q := gameSessionQuality(gsCard("a", 2, 1, 0, 2)); q >= feedGameSessionNotableMin {
		t.Fatalf("routine session scored %d, want < %d", q, feedGameSessionNotableMin)
	}
	// 5-win streak, flawless 5-0 → notable.
	if q := gameSessionQuality(gsCard("b", 5, 0, 0, 5)); q < feedGameSessionNotableMin {
		t.Fatalf("heater session scored %d, want >= %d", q, feedGameSessionNotableMin)
	}
	// Short solo session with no telemetry → T0 score 0.
	if q := gameSessionQuality(gsCardT0("c", 10, nil, 0, 0, 0, 0)); q != 0 {
		t.Fatalf("10m solo no-telemetry scored %d, want 0", q)
	}
	// Non-game card → sentinel (never competes as a game session).
	if q := gameSessionQuality(feedCard{backendType: "clip"}); q != feedMinQuality {
		t.Fatalf("non-game scored %d, want sentinel", q)
	}
}

func TestGameSessionQualityT0Examples(t *testing.T) {
	cases := []struct {
		name string
		card feedCard
		want int
	}{
		{"10m solo", gsCardT0("a", 10, nil, 0, 0, 0, 0), 0},
		{"4h solo", gsCardT0("b", 240, nil, 0, 0, 0, 0), 40},
		{"2h with one crewmate", gsCardT0("c", 120, []string{"a", "b"}, 0, 0, 0, 0), 45},
	}
	for _, tc := range cases {
		if got := gameSessionQuality(tc.card); got != tc.want {
			t.Fatalf("%s: scored %d, want %d", tc.name, got, tc.want)
		}
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
	gameSessionCount := 5 // five gsCard entries in input
	cap := gameSessionCardCap(gameSessionCount)
	if gameCount > cap {
		t.Fatalf("kept %d game sessions, want <= %d", gameCount, cap)
	}
}

func TestPruneGameSessions_LongSoloSurvivesQuietWeek(t *testing.T) {
	cards := []feedCard{
		gsCardT0("long-solo", 240, nil, 0, 0, 0, 0),
		{id: "clip1", backendType: "clip"},
	}
	out := pruneGameSessions(cards)
	if !feedCardsContain(out, "long-solo") {
		t.Fatal("4h solo no-telemetry session should survive in a quiet week (floor 10, score 40)")
	}
}

func TestPruneGameSessions_LongSoloPrunedLoudWeek(t *testing.T) {
	cards := make([]feedCard, 0, 17)
	cards = append(cards, gsCardT0("long-solo", 240, nil, 0, 0, 0, 0))
	for i := 0; i < 16; i++ {
		cards = append(cards, gsCard("heater"+string(rune('a'+i)), 5, 0, 0, 5))
	}
	out := pruneGameSessions(cards)
	if feedCardsContain(out, "long-solo") {
		t.Fatal("4h solo session should be pruned when 16+ higher-scoring sessions exist (floor 50, cap 2)")
	}
}

func TestFillerRole_NoTelemetryQuiet(t *testing.T) {
	c := gsCardT0("t0", 240, nil, 0, 0, 0, 0)
	if fillerRole(c) != "quiet" {
		t.Fatalf("no-telemetry game_session role: got %q want quiet", fillerRole(c))
	}
	cTelemetry := gsCard("telemetry", 5, 0, 0, 5)
	if fillerRole(cTelemetry) != "standard" {
		t.Fatalf("telemetry game_session role: got %q want standard", fillerRole(cTelemetry))
	}
}

func TestFillerPriority_GameSessionOrdering(t *testing.T) {
	strong := gsCard("strong", 5, 0, 0, 5)
	weak := gsCardT0("weak", 10, nil, 0, 0, 0, 0)
	voice := typedCard("voice", "session", "voice_session")

	pStrong := fillerPriority(strong)
	pVoice := fillerPriority(voice)
	pWeak := fillerPriority(weak)

	if pStrong <= pVoice {
		t.Fatalf("strong game session priority %d should exceed voice %d", pStrong, pVoice)
	}
	if pWeak < pVoice {
		t.Fatalf("weak game session priority %d should be >= voice %d", pWeak, pVoice)
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
