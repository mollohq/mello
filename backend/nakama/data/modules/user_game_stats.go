package main

import (
	"context"
	"crypto/rand"
	"encoding/json"
	"fmt"
	"math/big"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// ---------------------------------------------------------------------------
// Per-user, per-game outcome stats (spec 18 — Game Telemetry).
//
// Owner-read, server-write. The 7-day crew ledger can't hold longest-streak
// history, so this private store is the source of truth for streaks/win-rate.
// Only the derived `current_streak` is copied into the public game_session
// event (the "privacy bridge"); raw history never leaves this document.
//
// Streaks are tracked per *session* (a night nets to one win/loss/even
// outcome), matching how "win/loss streaks" was framed. Per-match streaks are
// a future option built on the stored match list.
// ---------------------------------------------------------------------------

const UserGameStatsCollection = "user_game_stats"

type UserGameStats struct {
	GameID            string `json:"game_id"`
	Wins              int    `json:"wins"`
	Losses            int    `json:"losses"`
	CurrentStreak     int    `json:"current_streak"` // signed: +wins, -losses (sessions)
	LongestWinStreak  int    `json:"longest_win_streak"`
	LongestLossStreak int    `json:"longest_loss_streak"`
	LastResult        string `json:"last_result"` // "win" | "loss" | "even" | ""
	UpdatedAt         int64  `json:"updated_at"`
}

func readUserGameStats(ctx context.Context, nk runtime.NakamaModule, userID, gameID string) (*UserGameStats, string) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: UserGameStatsCollection, Key: gameID, UserID: userID},
	})
	if err != nil || len(objects) == 0 {
		return &UserGameStats{GameID: gameID}, ""
	}
	var s UserGameStats
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &s); err != nil {
		return &UserGameStats{GameID: gameID}, ""
	}
	return &s, objects[0].GetVersion()
}

func writeUserGameStats(ctx context.Context, nk runtime.NakamaModule, userID string, s *UserGameStats, version string) error {
	data, err := json.Marshal(s)
	if err != nil {
		return err
	}
	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      UserGameStatsCollection,
			Key:             s.GameID,
			UserID:          userID,
			Value:           string(data),
			Version:         version,
			PermissionRead:  1, // owner only — private history
			PermissionWrite: 0, // server only
		},
	})
	return err
}

// applySessionOutcome folds one session's net wins/losses into the stats in
// place and returns the session-level result. Pure (no I/O) so streak logic is
// unit-testable. A net-winning session extends/flips a win streak, a net-losing
// session a loss streak; an even session leaves the streak unchanged.
func applySessionOutcome(s *UserGameStats, wins, losses int) string {
	s.Wins += wins
	s.Losses += losses

	result := "even"
	switch {
	case wins > losses:
		result = "win"
	case losses > wins:
		result = "loss"
	}

	switch result {
	case "win":
		if s.CurrentStreak < 0 {
			s.CurrentStreak = 0
		}
		s.CurrentStreak++
		if s.CurrentStreak > s.LongestWinStreak {
			s.LongestWinStreak = s.CurrentStreak
		}
	case "loss":
		if s.CurrentStreak > 0 {
			s.CurrentStreak = 0
		}
		s.CurrentStreak--
		if -s.CurrentStreak > s.LongestLossStreak {
			s.LongestLossStreak = -s.CurrentStreak
		}
	}
	s.LastResult = result
	return result
}

// UpdateUserGameStats reads, applies the session outcome, and writes with
// optimistic-concurrency retry. Returns the updated stats and the session
// result ("win"/"loss"/"even").
func UpdateUserGameStats(ctx context.Context, nk runtime.NakamaModule, userID, gameID string, wins, losses int) (*UserGameStats, string, error) {
	for attempt := 0; attempt < 3; attempt++ {
		s, version := readUserGameStats(ctx, nk, userID, gameID)
		result := applySessionOutcome(s, wins, losses)
		s.UpdatedAt = time.Now().UnixMilli()
		if err := writeUserGameStats(ctx, nk, userID, s, version); err == nil {
			return s, result, nil
		}
		jitter, _ := rand.Int(rand.Reader, big.NewInt(50))
		time.Sleep(time.Duration(50*(attempt+1)+int(jitter.Int64())) * time.Millisecond)
	}
	return nil, "", fmt.Errorf("user_game_stats write failed after 3 retries for user %s game %s", userID, gameID)
}
