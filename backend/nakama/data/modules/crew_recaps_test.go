package main

import (
	"context"
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
		"hangout":  {TotalHangoutMin: 1},
		"game":     {TotalGameMin: 1},
		"sessions": {SessionCount: 1},
		"clips":    {ClipCount: 1},
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
