package main

import "testing"

func TestApplySessionOutcome_WinStreakBuilds(t *testing.T) {
	s := &UserGameStats{GameID: "counter-strike-2"}

	if got := applySessionOutcome(s, 5, 2); got != "win" {
		t.Fatalf("result = %q, want win", got)
	}
	if s.CurrentStreak != 1 || s.LongestWinStreak != 1 {
		t.Fatalf("after win 1: streak=%d longestWin=%d", s.CurrentStreak, s.LongestWinStreak)
	}
	if s.Wins != 5 || s.Losses != 2 {
		t.Fatalf("totals: wins=%d losses=%d", s.Wins, s.Losses)
	}

	applySessionOutcome(s, 3, 1) // another winning session
	if s.CurrentStreak != 2 || s.LongestWinStreak != 2 {
		t.Fatalf("after win 2: streak=%d longestWin=%d", s.CurrentStreak, s.LongestWinStreak)
	}
}

func TestApplySessionOutcome_LossFlipsAndSkidGrows(t *testing.T) {
	s := &UserGameStats{GameID: "counter-strike-2", CurrentStreak: 3, LongestWinStreak: 3}

	if got := applySessionOutcome(s, 1, 4); got != "loss" {
		t.Fatalf("result = %q, want loss", got)
	}
	// Win streak should reset and flip to a single loss.
	if s.CurrentStreak != -1 {
		t.Fatalf("after loss: streak=%d, want -1", s.CurrentStreak)
	}
	if s.LongestWinStreak != 3 {
		t.Fatalf("longest win streak should be retained, got %d", s.LongestWinStreak)
	}

	applySessionOutcome(s, 0, 2) // another losing session
	if s.CurrentStreak != -2 || s.LongestLossStreak != 2 {
		t.Fatalf("after loss 2: streak=%d longestLoss=%d", s.CurrentStreak, s.LongestLossStreak)
	}
}

func TestApplySessionOutcome_EvenLeavesStreak(t *testing.T) {
	s := &UserGameStats{GameID: "counter-strike-2", CurrentStreak: 2, LongestWinStreak: 2}

	if got := applySessionOutcome(s, 3, 3); got != "even" {
		t.Fatalf("result = %q, want even", got)
	}
	if s.CurrentStreak != 2 {
		t.Fatalf("even session changed streak to %d, want 2", s.CurrentStreak)
	}
	if s.LastResult != "even" {
		t.Fatalf("last_result = %q, want even", s.LastResult)
	}
	// Even sessions still count toward totals.
	if s.Wins != 3 || s.Losses != 3 {
		t.Fatalf("totals: wins=%d losses=%d", s.Wins, s.Losses)
	}
}

func TestApplySessionOutcome_WinAfterSkidFlips(t *testing.T) {
	s := &UserGameStats{GameID: "counter-strike-2", CurrentStreak: -2, LongestLossStreak: 2}

	applySessionOutcome(s, 6, 1)
	if s.CurrentStreak != 1 {
		t.Fatalf("win after skid: streak=%d, want 1", s.CurrentStreak)
	}
	if s.LongestLossStreak != 2 {
		t.Fatalf("longest loss streak should be retained, got %d", s.LongestLossStreak)
	}
}
