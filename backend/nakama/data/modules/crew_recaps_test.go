package main

import (
	"context"
	"encoding/json"
	"strings"
	"testing"
	"time"
)

func TestBuildGamesPlayed_Sorted(t *testing.T) {
	got := buildGamesPlayed(map[string]int{"CS2": 30, "Rocket League": 80, "Valorant": 10})
	if len(got) != 3 {
		t.Fatalf("len = %d, want 3", len(got))
	}
	if got[0].Game != "Rocket League" || got[0].Matches != 80 {
		t.Fatalf("top = %+v, want Rocket League/80", got[0])
	}
	if got[2].Game != "Valorant" {
		t.Fatalf("last = %+v, want Valorant", got[2])
	}
}

func TestTopKeyCount(t *testing.T) {
	id, n := topKeyCount(map[string]int{"a": 3, "b": 9, "c": 5})
	if id != "b" || n != 9 {
		t.Fatalf("top = %s/%d, want b/9", id, n)
	}
	if id, n := topKeyCount(map[string]int{}); id != "" || n != 0 {
		t.Fatalf("empty = %s/%d, want empty/0", id, n)
	}
}

// ---- belts ----

func beltNames(id string) string { return "name-" + id }

func beltDoc() *Head2headDoc {
	return &Head2headDoc{Titles: []Head2headTitle{
		{TitleID: titleMouth, HolderUserID: "u1", Value: 120, Unit: "min", SinceMs: 100},
		{TitleID: titleClipper, HolderUserID: "u2", Value: 3, Unit: "clips", SinceMs: 5000, PreviousHolderUserID: "u1", MovedAtMs: 5000},
		{TitleID: titleWinner, HolderUserID: "u2", Value: 2, Unit: "wins", SinceMs: 1, PreviousHolderUserID: "u1", MovedAtMs: 10}, // before the week
		{TitleID: titleRageQuit, HolderUserID: "u1", Value: 4, Unit: "sessions", SinceMs: 2},
	}}
}

func TestBuildRecapBeltsMovedAreTheTitlesMovedInTheWeek(t *testing.T) {
	moved, _ := buildRecapBelts(beltDoc(), 1000, 9000, beltNames)
	if len(moved) != 1 {
		t.Fatalf("moved: %+v", moved)
	}
	m := moved[0]
	if m.TitleID != titleClipper || m.HolderUserID != "u2" || m.HolderName != "name-u2" ||
		m.PreviousUserID != "u1" || m.PreviousName != "name-u1" || m.Value != 3 || m.MovedAtMs != 5000 {
		t.Fatalf("moved: %+v", m)
	}
}

func TestBuildRecapBeltsHeldCountsEveryContestedTitle(t *testing.T) {
	_, held := buildRecapBelts(beltDoc(), 1000, 9000, beltNames)
	if len(held) != 2 {
		t.Fatalf("held: %+v", held)
	}
	// Two each: the tie goes to the lowest id.
	if held[0].UserID != "u1" || held[0].Count != 2 || held[1].UserID != "u2" || held[1].Count != 2 {
		t.Fatalf("held: %+v", held)
	}
	if got := held[0].TitleIDs; len(got) != 2 || got[0] != titleMouth || got[1] != titleRageQuit {
		t.Fatalf("title ids: %v", got)
	}
}

func TestBuildRecapBeltsNoDocIsEmpty(t *testing.T) {
	moved, held := buildRecapBelts(nil, 0, 1, beltNames)
	if moved != nil || held != nil {
		t.Fatal("no doc, no belts")
	}
}

func TestBeltAwardsUseTheTitleLabelAndPreviousHolder(t *testing.T) {
	moved, _ := buildRecapBelts(beltDoc(), 1000, 9000, beltNames)
	awards := beltAwards(moved)
	if len(awards) != 1 || awards[0] != (RecapAward{Kind: "belt", Title: "Clipper", Name: "name-u2", Detail: "took it from name-u1"}) {
		t.Fatalf("awards: %+v", awards)
	}
	for id, label := range head2headTitleLabels {
		if label == "" || strings.Contains(label, "—") {
			t.Errorf("label of %s: %q", id, label)
		}
	}
}

func recapFixture() *fakeGuestNk {
	nk := newFakeGuestNk()
	nk.addMember("crew", "u1", "Ann")
	nk.addMember("crew", "u2", "Bo")
	return nk
}

func TestWeeklyRecapListsBeltsMovedAndHeld(t *testing.T) {
	ctx := context.Background()
	nk := recapFixture()
	today := time.Now().UTC().Format("2006-01-02")
	nk.put(UserActivityStatsCollection, userActivityStatsKey, "u1", UserActivityStats{Clips: 1, Days: []ActivityDay{{Date: today, VoiceMin: 90}}})
	nk.put(UserActivityStatsCollection, userActivityStatsKey, "u2", UserActivityStats{Clips: 4, Days: []ActivityDay{{Date: today, VoiceMin: 30}}})
	// Last week u1 held the clipper title. u2 takes it at generation time.
	if err := writeHead2headDoc(ctx, nk, &Head2headDoc{CrewID: "crew", Titles: []Head2headTitle{
		{TitleID: titleClipper, HolderUserID: "u1", Value: 1, Unit: "clips", SinceMs: 1},
	}, UpdatedAt: 1}, ""); err != nil {
		t.Fatal(err)
	}
	// One voice session in the week gives the recap content.
	if err := AppendCrewEvent(ctx, nk, "crew", CrewEvent{
		ID: "e1", CrewID: "crew", Type: "voice_session", Timestamp: time.Now().UnixMilli(),
		Data: VoiceSessionData{ChannelName: "General", ParticipantNames: []string{"Ann", "Bo"}, DurationMin: 45},
	}); err != nil {
		t.Fatal(err)
	}

	generateWeeklyRecap(ctx, nk, testLogger(), "crew")

	doc, _ := readRecapsDoc(ctx, nk, "crew")
	if len(doc.Recaps) != 1 {
		t.Fatalf("recaps: %d", len(doc.Recaps))
	}
	r := doc.Recaps[0]
	if len(r.BeltsMoved) != 1 || r.BeltsMoved[0].TitleID != titleClipper ||
		r.BeltsMoved[0].HolderUserID != "u2" || r.BeltsMoved[0].PreviousUserID != "u1" || r.BeltsMoved[0].HolderName != "Bo" {
		t.Fatalf("belts_moved: %+v", r.BeltsMoved)
	}
	// u1 holds mouth, u2 holds clipper.
	if len(r.BeltsHeld) != 2 || r.BeltsHeld[0].Count != 1 || r.BeltsHeld[1].Count != 1 {
		t.Fatalf("belts_held: %+v", r.BeltsHeld)
	}
	if len(r.Leaderboard) != len(r.BeltsHeld) {
		t.Fatalf("leaderboard: %+v", r.Leaderboard)
	}
	var belt *RecapAward
	for i := range r.Awards {
		if r.Awards[i].Kind == "belt" {
			belt = &r.Awards[i]
		}
	}
	if belt == nil || belt.Title != "Clipper" || belt.Name != "Bo" {
		t.Fatalf("awards: %+v", r.Awards)
	}
	// The JSON the client reads carries the new fields.
	raw, _ := json.Marshal(r)
	for _, k := range []string{`"belts_moved"`, `"belts_held"`, `"leaderboard"`} {
		if !strings.Contains(string(raw), k) {
			t.Errorf("recap JSON lacks %s", k)
		}
	}
}

func putLedger(nk *fakeGuestNk, crewID string, events ...CrewEvent) {
	nk.put(CrewEventsCollection, crewID, SystemUserID, CrewEventLedger{CrewID: crewID, Events: events})
}

func gameEvent(d GameSessionData) CrewEvent {
	return CrewEvent{Type: "game_session", ActorID: "u1", Timestamp: time.Now().UnixMilli(), Data: d}
}

func TestGenerateWeeklyRecap_GameOnlyCrewHasNonZeroTotal(t *testing.T) {
	nk := newFakeGuestNk()
	nk.addMember("c1", "u1", "ann")
	putLedger(nk, "c1",
		gameEvent(GameSessionData{GameName: "Hades", DurationMin: 90}),
		// Left open overnight: active time wins, same as the Crew play card.
		gameEvent(GameSessionData{GameName: "Hades", DurationMin: 600, ActiveMin: 30}),
	)

	generateWeeklyRecap(context.Background(), nk, testLogger(), "c1")

	doc, _ := readRecapsDoc(context.Background(), nk, "c1")
	if len(doc.Recaps) != 1 {
		t.Fatalf("recaps = %d, want 1", len(doc.Recaps))
	}
	r := doc.Recaps[0]
	if r.TotalGameMin != 120 {
		t.Errorf("TotalGameMin = %d, want 120", r.TotalGameMin)
	}
	if r.TotalHangoutMin != 0 {
		t.Errorf("TotalHangoutMin = %d, want 0 (voice + stream only)", r.TotalHangoutMin)
	}
	if r.TotalMin != 120 {
		t.Errorf("TotalMin = %d, want 120", r.TotalMin)
	}
	if r.SessionCount != 2 {
		t.Errorf("SessionCount = %d, want 2", r.SessionCount)
	}
}

func TestGenerateWeeklyRecap_EmptyWeekWritesNothing(t *testing.T) {
	nk := newFakeGuestNk()
	putLedger(nk, "c1")

	generateWeeklyRecap(context.Background(), nk, testLogger(), "c1")

	doc, _ := readRecapsDoc(context.Background(), nk, "c1")
	if len(doc.Recaps) != 0 {
		t.Fatalf("recaps = %d, want 0 for an empty week", len(doc.Recaps))
	}
}

func TestIsEmptyRecap(t *testing.T) {
	cases := map[string]WeeklyRecapData{
		"hangout": {TotalHangoutMin: 1},
		"game":    {TotalGameMin: 1},
		"clips":   {ClipCount: 1}, // clips alone show a clip count
	}
	for name, r := range cases {
		if isEmptyRecap(r) {
			t.Errorf("%s: recap with activity reported empty", name)
		}
	}
	if !isEmptyRecap(WeeklyRecapData{CrewID: "c1", WeekStart: 5}) {
		t.Error("recap with no activity reported non-empty")
	}
}

func TestIsEmptyRecap_ZeroMinuteSessionIsEmpty(t *testing.T) {
	if !isEmptyRecap(WeeklyRecapData{SessionCount: 1}) {
		t.Error("a session with zero minutes must not keep a recap alive")
	}
}

func TestGenerateWeeklyRecap_NoMinutesWritesNothing(t *testing.T) {
	cases := map[string][]CrewEvent{
		"member join only": {{Type: "member_joined", ActorID: "u1", Timestamp: time.Now().UnixMilli()}},
		"chat only":        {{Type: "chat_activity", ActorID: "u1", Timestamp: time.Now().UnixMilli()}},
		"zero-minute stream": {{Type: "stream_session", ActorID: "u1", Timestamp: time.Now().UnixMilli(),
			Data: StreamSessionData{}}},
	}
	for name, evs := range cases {
		nk := newFakeGuestNk()
		nk.addMember("c1", "u1", "ann")
		putLedger(nk, "c1", evs...)

		generateWeeklyRecap(context.Background(), nk, testLogger(), "c1")

		doc, _ := readRecapsDoc(context.Background(), nk, "c1")
		if len(doc.Recaps) != 0 {
			t.Errorf("%s: recaps = %d, want 0", name, len(doc.Recaps))
		}
	}
}

func TestWeeklyRecapWithNoActivityStaysEmptyAndHasNoBelts(t *testing.T) {
	ctx := context.Background()
	nk := recapFixture()
	nk.put(UserActivityStatsCollection, userActivityStatsKey, "u1", UserActivityStats{Clips: 1})
	nk.put(UserActivityStatsCollection, userActivityStatsKey, "u2", UserActivityStats{Clips: 4})
	generateWeeklyRecap(ctx, nk, testLogger(), "crew")

	doc, _ := readRecapsDoc(ctx, nk, "crew")
	if len(doc.Recaps) != 0 {
		t.Fatalf("an empty week must write no recap: %d", len(doc.Recaps))
	}
	if h, _ := readHead2headDoc(ctx, nk, "crew"); h != nil {
		t.Fatal("an empty recap must not recompute the holders doc")
	}
}

func TestBuildThisWeek_SkipsEmptyRecap(t *testing.T) {
	empty := typedCard("recap_1", "recap", "weekly_recap")
	empty.data = WeeklyRecapData{WeekStart: 1}
	full := typedCard("recap_2", "recap", "weekly_recap")
	full.data = WeeklyRecapData{WeekStart: 2, TotalGameMin: 30, SessionCount: 1}

	if e := findEntry(buildThisWeek([]feedCard{empty}, 0), "recap_1"); e != nil {
		t.Error("empty recap offered in this_week")
	}
	if e := findEntry(buildThisWeek([]feedCard{full}, 0), "recap_2"); e == nil {
		t.Error("recap with activity missing from this_week")
	}
}

func TestBuildMemorySection_FiltersEmptyRecaps(t *testing.T) {
	nk := newFakeGuestNk()
	nk.put(CrewRecapsCollection, "c1", SystemUserID, CrewRecapsDoc{CrewID: "c1", Recaps: []WeeklyRecapData{
		{WeekStart: 1, GeneratedAt: 1},
		{WeekStart: 2, GeneratedAt: 2, TotalGameMin: 45, SessionCount: 1},
		{WeekStart: 3, GeneratedAt: 3},
	}})

	entries := buildMemorySection(context.Background(), nk, "c1", -1, nil, nil)

	if findEntry(entries, "recap_1") != nil || findEntry(entries, "recap_3") != nil {
		t.Error("empty recap present in memory")
	}
	if findEntry(entries, "recap_2") == nil {
		t.Error("recap with activity missing from memory")
	}
}

func TestBuildGamesPlayedWithMinutes_EveryGameMostHoursFirst(t *testing.T) {
	got := buildGamesPlayedWithMinutes(
		map[string]int{"CS2": 30, "Rocket League": 80},
		map[string]int{"CS2": 550, "Rocket League": 740, "Fall Guys": 270, "Idle": 0},
	)
	want := []RecapGameTally{
		{Game: "Rocket League", Matches: 80, Minutes: 740},
		{Game: "CS2", Matches: 30, Minutes: 550},
		{Game: "Fall Guys", Matches: 0, Minutes: 270},
	}
	if len(got) != len(want) {
		t.Fatalf("got %+v, want %+v", got, want)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("row %d = %+v, want %+v", i, got[i], want[i])
		}
	}
}

func TestBuildGamesPlayedWithMinutes_MatchesOnlyGameStillListed(t *testing.T) {
	got := buildGamesPlayedWithMinutes(map[string]int{"CS2": 4}, nil)
	if len(got) != 1 || got[0].Game != "CS2" || got[0].Matches != 4 || got[0].Minutes != 0 {
		t.Fatalf("got %+v", got)
	}
}

func TestPickClipOfWeek_LongestInTheWeek(t *testing.T) {
	clips := []StoredClip{
		{ClipID: "old", Ts: 50, DurationSeconds: 59, ClipperName: "Bob"},
		{ClipID: "a", Ts: 120, DurationSeconds: 12, ClipperName: "Max", Game: "Rocket League"},
		{ClipID: "b", Ts: 150, DurationSeconds: 29, ClipperName: "Leo", Game: "Rocket League", ParticipantNames: []string{"Leo", "Max"}},
		{ClipID: "late", Ts: 900, DurationSeconds: 60, ClipperName: "Sara"},
	}
	got := pickClipOfWeek(clips, 100, 200)
	if got == nil {
		t.Fatal("no clip of the week")
	}
	if got.ClipID != "b" || got.DurationSeconds != 29 || got.Game != "Rocket League" {
		t.Fatalf("got %+v", got)
	}
	if got.Title != "Leo clipped that" {
		t.Fatalf("title = %q", got.Title)
	}
}

func TestPickClipOfWeek_TieGoesToTheNewerClipAndNoClipsGivesNil(t *testing.T) {
	clips := []StoredClip{
		{ClipID: "a", Ts: 120, DurationSeconds: 20, ClipperName: "Max"},
		{ClipID: "b", Ts: 130, DurationSeconds: 20, ClipperName: "Leo"},
	}
	if got := pickClipOfWeek(clips, 100, 200); got == nil || got.ClipID != "b" {
		t.Fatalf("tie = %+v, want b", got)
	}
	if got := pickClipOfWeek(clips, 300, 400); got != nil {
		t.Fatalf("empty week = %+v, want nil", got)
	}
}

func TestRecapPayload_ClipFieldsAndOldRecapsStillRead(t *testing.T) {
	r := WeeklyRecapData{
		MostClipped: "Leo", MostClippedUserID: "u5", MostClippedCount: 5,
		ClipOfWeek:  &RecapClipOfWeek{ClipID: "c1", Title: "Leo clipped that", DurationSeconds: 29, Game: "Rocket League"},
		GamesPlayed: []RecapGameTally{{Game: "Rocket League", Matches: 3, Minutes: 740}},
	}
	b, err := json.Marshal(r)
	if err != nil {
		t.Fatal(err)
	}
	var m map[string]interface{}
	_ = json.Unmarshal(b, &m)
	if m["most_clipped"] != "Leo" || m["most_clipped_user_id"] != "u5" || m["most_clipped_count"].(float64) != 5 {
		t.Fatalf("most clipped: %v", m)
	}
	cow := m["clip_of_week"].(map[string]interface{})
	if cow["clip_id"] != "c1" || cow["title"] != "Leo clipped that" || cow["duration_seconds"].(float64) != 29 || cow["game"] != "Rocket League" {
		t.Fatalf("clip_of_week: %v", cow)
	}
	gp := m["games_played"].([]interface{})[0].(map[string]interface{})
	if gp["minutes"].(float64) != 740 {
		t.Fatalf("games_played: %v", gp)
	}
	// A recap stored before these fields existed reads, and has none of them.
	var old WeeklyRecapData
	if err := json.Unmarshal([]byte(`{"crew_id":"c","most_clipped":"Leo","games_played":[{"game":"CS2","matches":3}]}`), &old); err != nil {
		t.Fatal(err)
	}
	if old.ClipOfWeek != nil || old.MostClippedCount != 0 || old.GamesPlayed[0].Minutes != 0 {
		t.Fatalf("old = %+v", old)
	}
}
