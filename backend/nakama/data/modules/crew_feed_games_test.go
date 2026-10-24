package main

import (
	"context"
	"fmt"
	"strings"
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
	card, ok := buildRecentGames(cards, nil)
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
	// Newest play first: valorant (ts 40), minecraft (30), cs2 (20).
	want := []RecentGameTile{
		{GameID: "valorant", GameName: "Valorant", Minutes: 130, LastPlayed: 40},
		{GameID: "minecraft", GameName: "Minecraft", Minutes: 200, LastPlayed: 30},
		{GameID: "counter-strike-2", GameName: "Counter-Strike 2", Minutes: 50, LastPlayed: 20},
	}
	if len(data.Games) != len(want) {
		t.Fatalf("tiles: got %d want %d (%+v)", len(data.Games), len(want), data.Games)
	}
	for i, w := range want {
		g := data.Games[i]
		if g.GameID != w.GameID || g.GameName != w.GameName || g.Minutes != w.Minutes || g.LastPlayed != w.LastPlayed {
			t.Errorf("tile %d: got %+v want %+v", i, g, w)
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
	card, ok := buildRecentGames(cards, nil)
	if !ok {
		t.Fatal("expected a card")
	}
	data := card.data.(RecentGamesData)
	if len(data.Games) != 1 || data.Games[0].Minutes != 60 || data.Games[0].GameID != "valorant" {
		t.Fatalf("tiles: %+v, want one valorant tile of 60 min", data.Games)
	}
}

func TestBuildRecentGames_NothingWithoutMinutes(t *testing.T) {
	if _, ok := buildRecentGames(nil, nil); ok {
		t.Fatal("no sessions must give no card")
	}
	if _, ok := buildRecentGames([]feedCard{gsCardID("z", "valorant", "Valorant", 0, 1)}, nil); ok {
		t.Fatal("a zero-minute session must give no card")
	}
}

func TestBuildRecentGames_CapsTilesAtSixButCountsAllSessions(t *testing.T) {
	cards := make([]feedCard, 0, 9)
	for i := 0; i < 9; i++ {
		id := "game" + string(rune('a'+i))
		cards = append(cards, gsCardID("s"+id, id, id, 10*(i+1), int64(i+1)))
	}
	card, ok := buildRecentGames(cards, nil)
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
	}, nil)
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

// A busy crew (50 members) has more clips and strong sessions than slots.
// Clips rank 10000 and strong sessions above the recent games card, so before
// the diversity pick the card was crowded out of the week.
func TestBuildThisWeek_BusyCrewStillHasRecentGames(t *testing.T) {
	now := time.Now().UnixMilli()
	cards := make([]feedCard, 0, 40)
	for i := 0; i < 8; i++ {
		cards = append(cards, clipCard(fmt.Sprintf("clip%d", i), now-int64(i)*1000))
	}
	for i := 0; i < 16; i++ {
		cards = append(cards, gsCard(fmt.Sprintf("heater%d", i), 5, 0, 0, 5))
	}
	entries := buildThisWeek(cards, now)
	for _, e := range entries {
		if e.Type == "recent-games" {
			return
		}
	}
	t.Fatalf("a busy crew lost the recent games card: %d entries", len(entries))
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

// ---- library snapshot merge (plans/next-gen-feed.md section 3) ----

func libDoc(updatedAt int64, games ...LibraryGame) *LibraryDoc {
	return &LibraryDoc{Games: games, UpdatedAt: updatedAt}
}

func libGame(id, name string, playtimeMin int, lastPlayedUnix int64) LibraryGame {
	g := LibraryGame{GameID: id, Name: name, Launcher: "steam"}
	if playtimeMin >= 0 {
		g.PlaytimeMin = &playtimeMin
	}
	if lastPlayedUnix > 0 {
		g.LastPlayedUnix = &lastPlayedUnix
	}
	return g
}

func recentData(t *testing.T, card feedCard) RecentGamesData {
	t.Helper()
	d, ok := card.data.(RecentGamesData)
	if !ok {
		t.Fatalf("data type %T", card.data)
	}
	return d
}

func tileByID(t *testing.T, d RecentGamesData, id string) RecentGameTile {
	t.Helper()
	for _, g := range d.Games {
		if g.GameID == id {
			return g
		}
	}
	t.Fatalf("no tile %q in %+v", id, d.Games)
	return RecentGameTile{}
}

// A snapshot with no sessions still gives a card. The header counts nothing.
func TestBuildRecentGames_SnapshotOnly(t *testing.T) {
	libs := map[string]*LibraryDoc{
		"alice": libDoc(5_000,
			libGame("steam-730", "Counter-Strike 2", 249_008, 1_760_000_000),
			libGame("epic-fortnite", "Fortnite", -1, 0)),
		"bob": libDoc(6_000, libGame("steam-730", "Counter-Strike 2", 600, 1_760_000_500)),
	}
	card, ok := buildRecentGames(nil, libs)
	if !ok {
		t.Fatal("a snapshot alone must give a card")
	}
	d := recentData(t, card)
	if d.SessionCount != 0 || d.TotalMin != 0 {
		t.Fatalf("header = %d sessions / %d min, want 0 / 0", d.SessionCount, d.TotalMin)
	}
	cs := tileByID(t, d, "steam-730")
	if cs.Minutes != 249_608 {
		t.Fatalf("minutes = %d, want the members' playtime summed (249608)", cs.Minutes)
	}
	if cs.LastPlayed != 1_760_000_500_000 {
		t.Fatalf("last played = %d, want the newest, in ms", cs.LastPlayed)
	}
	if got := strings.Join(cs.MemberIDs, ","); got != "alice,bob" {
		t.Fatalf("members = %q, want alice,bob", got)
	}
	fn := tileByID(t, d, "epic-fortnite")
	if fn.Minutes != 0 || fn.LastPlayed != 0 || len(fn.MemberIDs) != 1 {
		t.Fatalf("fortnite tile = %+v", fn)
	}
	if d.Games[0].GameID != "steam-730" {
		t.Fatalf("the played game must lead, got %q", d.Games[0].GameID)
	}
	if card.ts != 1_760_000_500_000 {
		t.Fatalf("card ts = %d", card.ts)
	}
	if d.PCCount != 2 {
		t.Fatalf("pc_count = %d, want 2 (alice and bob)", d.PCCount)
	}
	if !cardHasContent(card.feedType, card.backendType, card.data) {
		t.Fatal("a snapshot card has content")
	}
}

// Sessions alone behave as before, and the players become the pips.
func TestBuildRecentGames_SessionsOnlyCarryPlayers(t *testing.T) {
	c := gsCardID("s1", "valorant", "Valorant", 90, 10)
	sd := c.data.(GameSessionData)
	sd.PlayerIDs = []string{"alice", "bob"}
	c.data = sd
	card, ok := buildRecentGames([]feedCard{c}, nil)
	if !ok {
		t.Fatal("expected a card")
	}
	d := recentData(t, card)
	if d.SessionCount != 1 || d.TotalMin != 90 || len(d.Games) != 1 {
		t.Fatalf("data = %+v", d)
	}
	if g := d.Games[0]; g.Minutes != 90 || strings.Join(g.MemberIDs, ",") != "alice,bob" {
		t.Fatalf("tile = %+v", g)
	}
}

// Both sources: union by id, name and hours from the snapshot, last played
// from the newer, pips from both, header from sessions only.
func TestBuildRecentGames_SnapshotAndSessionsMerge(t *testing.T) {
	cs := gsCardID("s1", "steam-730", "CS2 session name", 60, 2_000_000)
	csd := cs.data.(GameSessionData)
	csd.PlayerIDs = []string{"carol"}
	cs.data = csd
	cards := []feedCard{
		cs,
		gsCardID("s2", "valorant", "Valorant", 45, 3_000_000),
	}
	libs := map[string]*LibraryDoc{
		"alice": libDoc(1,
			libGame("steam-730", "Counter-Strike 2", 1_000, 1_000), // 1,000,000 ms: older than the session
			libGame("steam-570", "Dota 2", 300, 9_000)),            // 9,000,000 ms: newest of all
	}
	card, ok := buildRecentGames(cards, libs)
	if !ok {
		t.Fatal("expected a card")
	}
	d := recentData(t, card)
	if d.SessionCount != 2 || d.TotalMin != 105 {
		t.Fatalf("header = %d / %d, want 2 sessions / 105 min from sessions only", d.SessionCount, d.TotalMin)
	}
	if len(d.Games) != 3 {
		t.Fatalf("tiles = %+v, want the union of 3", d.Games)
	}
	csTile := tileByID(t, d, "steam-730")
	if csTile.GameName != "Counter-Strike 2" {
		t.Fatalf("name = %q, want the snapshot name", csTile.GameName)
	}
	if csTile.Minutes != 1_000 {
		t.Fatalf("minutes = %d, want playtime_min over session minutes", csTile.Minutes)
	}
	if csTile.LastPlayed != 2_000_000 {
		t.Fatalf("last played = %d, want the newer (the session)", csTile.LastPlayed)
	}
	if strings.Join(csTile.MemberIDs, ",") != "alice,carol" {
		t.Fatalf("members = %v, want installed (alice) and played (carol)", csTile.MemberIDs)
	}
	if d.PCCount != 2 {
		t.Fatalf("pc_count = %d, want 2 (alice installed, carol played)", d.PCCount)
	}
	val := tileByID(t, d, "valorant")
	if val.Minutes != 45 {
		t.Fatalf("a session-only game keeps its session minutes, got %d", val.Minutes)
	}
	// Order: dota (9,000,000), valorant (3,000,000), cs2 (2,000,000).
	order := []string{d.Games[0].GameID, d.Games[1].GameID, d.Games[2].GameID}
	if strings.Join(order, ",") != "steam-570,valorant,steam-730" {
		t.Fatalf("order = %v, want last played descending", order)
	}
}

// A session id from the catalogue and a launcher id from the library name the
// same game. The name joins them into one tile.
func TestBuildRecentGames_NameJoinsCatalogueAndLauncherIDs(t *testing.T) {
	cards := []feedCard{gsCardID("s1", "counter-strike-2", "Counter-Strike 2", 30, 100)}
	libs := map[string]*LibraryDoc{
		"alice": libDoc(1, libGame("steam-730", "counter-strike 2", 500, 0)),
	}
	libs["alice"].Games[0].Name = "Counter-Strike 2"
	card, ok := buildRecentGames(cards, libs)
	if !ok {
		t.Fatal("expected a card")
	}
	d := recentData(t, card)
	if len(d.Games) != 1 {
		t.Fatalf("tiles = %+v, want one", d.Games)
	}
	g := d.Games[0]
	if g.GameID != "counter-strike-2" || g.Minutes != 500 || len(g.MemberIDs) != 1 {
		t.Fatalf("tile = %+v, want the session id, snapshot hours, one member", g)
	}
}

func TestBuildRecentGames_SnapshotStillCapsAtSix(t *testing.T) {
	games := make([]LibraryGame, 0, 10)
	for i := 0; i < 10; i++ {
		games = append(games, libGame(fmt.Sprintf("steam-%d", i+1), fmt.Sprintf("Game %d", i+1), 10, int64(1000+i)))
	}
	card, ok := buildRecentGames(nil, map[string]*LibraryDoc{"alice": libDoc(1, games...)})
	if !ok {
		t.Fatal("expected a card")
	}
	d := recentData(t, card)
	if len(d.Games) != feedRecentGamesMaxTiles {
		t.Fatalf("tiles = %d, want %d", len(d.Games), feedRecentGamesMaxTiles)
	}
	if d.Games[0].GameID != "steam-10" {
		t.Fatalf("newest first, got %q", d.Games[0].GameID)
	}
}

func TestBuildRecentGames_EmptySnapshotsGiveNoCard(t *testing.T) {
	libs := map[string]*LibraryDoc{"alice": libDoc(1), "bob": nil}
	if _, ok := buildRecentGames(nil, libs); ok {
		t.Fatal("empty documents must give no card")
	}
}

// The feed RPC path reads the member documents as the system.
func TestCrewLibraryDocs_ReadsMemberDocs(t *testing.T) {
	nk := newFakeGuestNk()
	nk.addMember("crew1", "alice", "Alice")
	nk.addMember("crew1", "bob", "Bob")
	nk.put(UserLibraryCollection, UserLibraryKey, "alice", libDoc(1, libGame("steam-1", "A", 5, 0)))
	nk.put(UserLibraryCollection, UserLibraryKey, "stranger", libDoc(1, libGame("steam-2", "B", 5, 0)))
	docs := crewLibraryDocs(context.Background(), nk, "crew1")
	if len(docs) != 1 || docs["alice"] == nil {
		t.Fatalf("docs = %+v, want only the members' own", docs)
	}
}

func TestBuildThisWeek_SnapshotFeedsRecentGamesCard(t *testing.T) {
	libs := map[string]*LibraryDoc{"alice": libDoc(1, libGame("steam-730", "Counter-Strike 2", 120, 1_760_000_000))}
	entries := buildThisWeekWithLibraries(nil, time.Now().UnixMilli(), libs)
	found := false
	for _, e := range entries {
		if e.Type == "recent-games" {
			found = true
		}
	}
	if !found {
		t.Fatalf("entries = %+v, want a recent-games card from the snapshot", entries)
	}
}

// The tile carries the launcher of the library entry, so the client can show
// it when the playtime is unknown.
func TestBuildRecentGames_TileCarriesTheLauncher(t *testing.T) {
	epic := libGame("epic-rocketleague", "Rocket League", -1, 0)
	epic.Launcher = "epic"
	libs := map[string]*LibraryDoc{
		"alice": libDoc(5_000, epic, libGame("steam-730", "Counter-Strike 2", 600, 1_760_000_000)),
	}
	card, ok := buildRecentGames(nil, libs)
	if !ok {
		t.Fatal("a snapshot alone must give a card")
	}
	d := recentData(t, card)
	if got := tileByID(t, d, "epic-rocketleague").Launcher; got != "epic" {
		t.Fatalf("launcher = %q, want epic", got)
	}
	if got := tileByID(t, d, "steam-730").Launcher; got != "steam" {
		t.Fatalf("launcher = %q, want steam", got)
	}
}

// The recent games card is on the first screen for every crew size: right
// after the head2head card, ahead of the clips that fill the other slots. A
// busy crew used to get it as the 7th entry, after the wide-slot swap pushed
// it back.
func TestBuildThisWeek_RecentGamesFollowsHead2headInABusyCrew(t *testing.T) {
	now := time.Now().UnixMilli()
	cards := make([]feedCard, 0, 40)
	for i := 0; i < 13; i++ {
		cards = append(cards, clipCard(fmt.Sprintf("clip%d", i), now-int64(i)*1000))
	}
	for i := 0; i < 16; i++ {
		cards = append(cards, gsCard(fmt.Sprintf("heater%d", i), 5, 0, 0, 5))
	}
	entries := buildThisWeekFull(cards, now, nil, &Head2headFeedData{UpdatedAt: now})
	if len(entries) < 3 || entries[0].Type != "head2head" {
		t.Fatalf("entries: %+v", entries)
	}
	pos := -1
	for i, e := range entries {
		if e.Type == "recent-games" {
			pos = i
		}
		if e.Type == "clip" && e.Size == "lg" {
			t.Fatalf("clip %s is wide", e.ID)
		}
	}
	if pos < 0 || pos > 1 {
		t.Fatalf("recent games at entry %d, want right after head2head", pos)
	}
}

// The PC count is over every game, not the tiles that survive the cap, and a
// member who owns many games counts once. One owner is a list, not a grid.
func TestBuildRecentGames_PCCountCountsMembersNotTiles(t *testing.T) {
	alice := []LibraryGame{}
	for i := 0; i < 8; i++ {
		alice = append(alice, libGame(fmt.Sprintf("steam-%d", i), fmt.Sprintf("Game %d", i), 60, int64(1_000+i)))
	}
	solo := map[string]*LibraryDoc{"alice": libDoc(1, alice...)}
	card, ok := buildRecentGames(nil, solo)
	if !ok {
		t.Fatal("expected a card")
	}
	d := recentData(t, card)
	if len(d.Games) != feedRecentGamesMaxTiles || d.PCCount != 1 {
		t.Fatalf("tiles = %d, pc_count = %d, want %d tiles and 1 PC", len(d.Games), d.PCCount, feedRecentGamesMaxTiles)
	}
	// Bob owns only a game that the cap drops. He still counts.
	two := map[string]*LibraryDoc{
		"alice": libDoc(1, alice...),
		"bob":   libDoc(1, libGame("steam-0", "Game 0", 60, 1_000)),
	}
	card, _ = buildRecentGames(nil, two)
	if d = recentData(t, card); d.PCCount != 2 {
		t.Fatalf("pc_count = %d, want 2", d.PCCount)
	}
}
