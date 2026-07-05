package main

import "testing"

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
