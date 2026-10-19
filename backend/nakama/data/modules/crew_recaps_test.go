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

func TestWeeklyRecapWithNoActivityStaysEmptyAndHasNoBelts(t *testing.T) {
	ctx := context.Background()
	nk := recapFixture()
	nk.put(UserActivityStatsCollection, userActivityStatsKey, "u1", UserActivityStats{Clips: 1})
	nk.put(UserActivityStatsCollection, userActivityStatsKey, "u2", UserActivityStats{Clips: 4})
	generateWeeklyRecap(ctx, nk, testLogger(), "crew")

	doc, _ := readRecapsDoc(ctx, nk, "crew")
	if len(doc.Recaps) != 1 {
		t.Fatalf("recaps: %d", len(doc.Recaps))
	}
	r := doc.Recaps[0]
	if recapHasContent(r) {
		t.Fatalf("belts must not give an empty week content: %+v", r)
	}
	if len(r.BeltsMoved) != 0 || len(r.BeltsHeld) != 0 || len(r.Leaderboard) != 0 {
		t.Fatalf("empty recap carries belts: %+v", r)
	}
	if h, _ := readHead2headDoc(ctx, nk, "crew"); h != nil {
		t.Fatal("an empty recap must not recompute the holders doc")
	}
}
