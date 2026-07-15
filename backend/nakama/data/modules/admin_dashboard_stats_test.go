package main

import (
	"testing"
	"time"
)

func TestCumulativeDaily(t *testing.T) {
	today := time.Date(2026, 7, 15, 0, 0, 0, 0, time.UTC)

	// Matches the Postgres validation fixture: A 20d ago (before window),
	// B 10d, E 6d, C 5d, D today.
	perDay := map[string]int{
		"2026-06-25": 1, // A — before the 14-day window (baseline)
		"2026-07-05": 1, // B
		"2026-07-09": 1, // E
		"2026-07-10": 1, // C
		"2026-07-15": 1, // D — today
	}

	got := cumulativeDaily(perDay, today)

	if len(got) != dashboardDailyWindow {
		t.Fatalf("len = %d, want %d", len(got), dashboardDailyWindow)
	}
	// Window is 2026-07-02 .. 2026-07-15.
	if got[0].Date != "2026-07-02" || got[len(got)-1].Date != "2026-07-15" {
		t.Fatalf("window = %s..%s, want 2026-07-02..2026-07-15", got[0].Date, got[len(got)-1].Date)
	}
	// First bar carries the baseline (A), not a reset to zero.
	if got[0].Count != 1 {
		t.Fatalf("baseline (first bar) = %d, want 1", got[0].Count)
	}
	// Cumulative is monotonic non-decreasing and ends at the grand total (5).
	prev := 0
	for _, p := range got {
		if p.Count < prev {
			t.Fatalf("cumulative decreased at %s: %d < %d", p.Date, p.Count, prev)
		}
		prev = p.Count
	}
	if got[len(got)-1].Count != 5 {
		t.Fatalf("final cumulative = %d, want 5 (grand total)", got[len(got)-1].Count)
	}

	// Spot-check a specific day: by 2026-07-09 we have A(baseline)+B+E = 3.
	byDate := map[string]int{}
	for _, p := range got {
		byDate[p.Date] = p.Count
	}
	if byDate["2026-07-09"] != 3 {
		t.Fatalf("cumulative on 2026-07-09 = %d, want 3", byDate["2026-07-09"])
	}
}

func TestCumulativeDailyEmpty(t *testing.T) {
	today := time.Date(2026, 7, 15, 0, 0, 0, 0, time.UTC)
	got := cumulativeDaily(map[string]int{}, today)
	if len(got) != dashboardDailyWindow {
		t.Fatalf("len = %d, want %d", len(got), dashboardDailyWindow)
	}
	for _, p := range got {
		if p.Count != 0 {
			t.Fatalf("empty history should be all zeros, got %d on %s", p.Count, p.Date)
		}
	}
}
