package main

import (
	"testing"
	"time"
)

func gsCard(id string, wins, losses, draws, streak int) feedCard {
	return feedCard{
		id:          id,
		feedType:    "session",
		backendType: "game_session",
		data: GameSessionData{
			GameName:    "Counter-Strike 2",
			DurationMin: 20, // under the 30 min T0 bonus, over the empty-card rule
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
	out, pruned := pruneGameSessions(cards)

	if !feedCardsContain(out, "clip1") || !feedCardsContain(out, "voice1") {
		t.Fatalf("non-game cards were dropped")
	}
	if feedCardsContain(out, "routine1") || feedCardsContain(out, "routine2") {
		t.Fatalf("routine game sessions were kept")
	}
	if len(pruned) < 2 {
		t.Fatalf("expected pruned routine sessions, got %d", len(pruned))
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
	out, _ := pruneGameSessions(cards)
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
	out, _ := pruneGameSessions(cards)
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

func gsCardID(id, gameID, game string, durationMin int, ts int64) feedCard {
	return feedCard{
		id: id, feedType: "session", backendType: "game_session", ts: ts,
		data: GameSessionData{GameID: gameID, GameName: game, DurationMin: durationMin},
	}
}

func TestBuildRecentGames_Aggregation(t *testing.T) {
	cards := []feedCard{
		gsCardID("s1", "valorant", "Valorant", 100, 10),
		gsCardID("s2", "counter-strike-2", "Counter-Strike 2", 50, 20),
		gsCardID("s3", "minecraft", "Minecraft", 200, 30),
		gsCardID("s4", "valorant", "Valorant", 30, 40),
		clipCard("not-a-session", 99),
	}
	card, ok := buildRecentGames(cards)
	if !ok {
		t.Fatal("expected a recent games card")
	}
	if card.feedType != "recent-games" || card.backendType != "recent_games" || card.id != "recent_games" {
		t.Fatalf("meta: type=%q backend=%q id=%q", card.feedType, card.backendType, card.id)
	}
	if card.ts != 40 {
		t.Fatalf("ts = %d, want newest session ts 40", card.ts)
	}
	data, ok := card.data.(RecentGamesData)
	if !ok {
		t.Fatalf("data type %T", card.data)
	}
	if data.SessionCount != 4 || data.TotalMin != 380 {
		t.Fatalf("header: sessions=%d min=%d, want 4 / 380", data.SessionCount, data.TotalMin)
	}
	want := []RecentGameTile{
		{GameID: "minecraft", GameName: "Minecraft", Minutes: 200},
		{GameID: "valorant", GameName: "Valorant", Minutes: 130},
		{GameID: "counter-strike-2", GameName: "Counter-Strike 2", Minutes: 50},
	}
	if len(data.Games) != len(want) {
		t.Fatalf("tiles: got %d want %d (%+v)", len(data.Games), len(want), data.Games)
	}
	for i, w := range want {
		if data.Games[i] != w {
			t.Errorf("tile %d: got %+v want %+v", i, data.Games[i], w)
		}
	}
}

// Legacy events carry no game id: the name keys the tile, and the same game
// under a different case is one tile.
func TestBuildRecentGames_LegacyNameKey(t *testing.T) {
	cards := []feedCard{
		gsCardID("a", "", "Valorant", 30, 1),
		gsCardID("b", "", "valorant", 30, 2),
	}
	card, ok := buildRecentGames(cards)
	if !ok {
		t.Fatal("expected a card")
	}
	data := card.data.(RecentGamesData)
	if len(data.Games) != 1 || data.Games[0].Minutes != 60 || data.Games[0].GameID != "valorant" {
		t.Fatalf("tiles: %+v, want one valorant tile of 60 min", data.Games)
	}
}

func TestBuildRecentGames_NothingWithoutMinutes(t *testing.T) {
	if _, ok := buildRecentGames(nil); ok {
		t.Fatal("no sessions must give no card")
	}
	if _, ok := buildRecentGames([]feedCard{gsCardID("z", "valorant", "Valorant", 0, 1)}); ok {
		t.Fatal("a zero-minute session must give no card")
	}
}

func TestBuildRecentGames_CapsTilesAtSixButCountsAllSessions(t *testing.T) {
	cards := make([]feedCard, 0, 9)
	for i := 0; i < 9; i++ {
		id := "game" + string(rune('a'+i))
		cards = append(cards, gsCardID("s"+id, id, id, 10*(i+1), int64(i+1)))
	}
	card, ok := buildRecentGames(cards)
	if !ok {
		t.Fatal("expected a card")
	}
	data := card.data.(RecentGamesData)
	if len(data.Games) != feedRecentGamesMaxTiles {
		t.Fatalf("tiles: got %d want %d", len(data.Games), feedRecentGamesMaxTiles)
	}
	if data.SessionCount != 9 {
		t.Fatalf("header counts %d sessions, want all 9", data.SessionCount)
	}
	if data.Games[0].Minutes != 90 {
		t.Fatalf("top tile = %d min, want 90", data.Games[0].Minutes)
	}
}

func TestReportableMinutes_OvernightGuard(t *testing.T) {
	cases := []struct {
		name           string
		duration, want int
		active         int
	}{
		{"no active figure keeps wall time", 600, 600, 0},
		{"active close to wall keeps wall time", 120, 120, 100},
		{"active at exactly a third keeps wall time", 120, 120, 40},
		{"overnight session reports active time", 600, 40, 40},
	}
	for _, c := range cases {
		if got := reportableMinutes(c.duration, c.active); got != c.want {
			t.Errorf("%s: reportableMinutes(%d, %d) = %d, want %d",
				c.name, c.duration, c.active, got, c.want)
		}
	}
}

// A session left running overnight must not inflate the recent games header.
// The session's own card already shows the active figure, so a total summing
// wall time would contradict the cards directly below it.
func TestBuildRecentGames_OvernightSessionDoesNotInflateTotals(t *testing.T) {
	overnight := gsCardID("s1", "valorant", "Valorant", 600, 10)
	d := overnight.data.(GameSessionData)
	d.ActiveMin = 40
	overnight.data = d

	card, ok := buildRecentGames([]feedCard{
		overnight,
		gsCardID("s2", "valorant", "Valorant", 60, 20),
		gsCardID("s3", "valorant", "Valorant", 30, 30),
	})
	if !ok {
		t.Fatal("expected a card")
	}
	data := card.data.(RecentGamesData)
	if data.TotalMin != 130 {
		t.Fatalf("total = %d, want 130 (40 active + 60 + 30), not the 690 wall-time sum", data.TotalMin)
	}
	if data.Games[0].Minutes != 130 {
		t.Fatalf("tile = %d min, want 130", data.Games[0].Minutes)
	}
}

// The Crew play rollup is gone. A loud week yields one recent games card that
// counts the pruned routine sessions, and no entry of type rollup.
func TestBuildThisWeek_LoudWeekHasRecentGamesNotRollup(t *testing.T) {
	cards := make([]feedCard, 0, 20)
	for i := 0; i < 16; i++ {
		cards = append(cards, gsCard("heater"+string(rune('a'+i)), 5, 0, 0, 5))
	}
	for i := 0; i < 4; i++ {
		cards = append(cards, gsCardT0("routine"+string(rune('a'+i)), 30, []string{"solo"}, 0, 0, 0, 0))
	}
	entries := buildThisWeek(cards, time.Now().UnixMilli())
	var recent *FeedEntry
	for i := range entries {
		if entries[i].Type == "rollup" {
			t.Fatal("the Crew play rollup must not be emitted")
		}
		if entries[i].Type == "recent-games" {
			recent = &entries[i]
		}
	}
	if recent == nil {
		t.Fatal("expected a recent games entry")
	}
	if recent.ID != "recent_games" || recent.Role != "standard" {
		t.Fatalf("meta: id=%q role=%q", recent.ID, recent.Role)
	}
	data := recent.Data.(RecentGamesData)
	if data.SessionCount != 20 {
		t.Fatalf("header counts %d sessions, want all 20", data.SessionCount)
	}
}

func TestFillerPriority_RecentGamesBeatsWeakSession(t *testing.T) {
	recent := feedCard{feedType: "recent-games", backendType: "recent_games"}
	weak := gsCardT0("weak", 10, nil, 0, 0, 0, 0)
	strong := gsCard("strong", 5, 0, 0, 5)
	if fillerPriority(recent) <= fillerPriority(weak) {
		t.Fatalf("recent games priority %d should beat weak session %d", fillerPriority(recent), fillerPriority(weak))
	}
	if fillerPriority(strong) <= fillerPriority(recent) {
		t.Fatalf("strong session priority %d should beat recent games %d", fillerPriority(strong), fillerPriority(recent))
	}
}
