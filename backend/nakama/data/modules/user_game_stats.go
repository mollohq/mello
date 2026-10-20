package main

import (
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/json"
	"fmt"
	"math/big"
	"sort"
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
//
// The display fields (Draws, RecentForm, LastPlayed) back the personal "You
// strip" + profile (spec 19).
// ---------------------------------------------------------------------------

const UserGameStatsCollection = "user_game_stats"

// recentFormCap bounds the rolling per-session form list (newest last).
const recentFormCap = 10

// recentDaysCap bounds the rolling daily play-time window (oldest first). 14
// days feeds the head2head sampler title (plans/next-gen-feed.md section 5).
const recentDaysCap = 14

// RecentDayEntry is one UTC calendar day's accumulated session minutes.
type RecentDayEntry struct {
	Date      string `json:"date"` // "YYYY-MM-DD" (UTC)
	WallMin   int    `json:"wall_min"`
	ActiveMin int    `json:"active_min"`
}

type UserGameStats struct {
	GameID            string           `json:"game_id"`
	Wins              int              `json:"wins"`
	Losses            int              `json:"losses"`
	Draws             int              `json:"draws"`
	CurrentStreak     int              `json:"current_streak"` // signed: +wins, -losses (sessions)
	LongestWinStreak  int              `json:"longest_win_streak"`
	LongestLossStreak int              `json:"longest_loss_streak"`
	RecentForm        []string         `json:"recent_form"` // per-session "W"|"L"|"D", newest last, capped
	LastResult        string           `json:"last_result"` // "win" | "loss" | "even" | ""
	LastPlayed        int64            `json:"last_played"`
	UpdatedAt         int64            `json:"updated_at"`
	PlayedMinTotal    int              `json:"played_min_total"` // lifetime wall minutes
	ActiveMinTotal    int              `json:"active_min_total"`
	RecentDays        []RecentDayEntry `json:"recent_days"` // oldest first, capped at recentDaysCap
	// LastTzOffsetMin is the user's UTC offset in minutes (east is positive)
	// as reported with the last session of this game. Nil when the client did
	// not send one. Head2head night_owl needs it to read a local clock time.
	LastTzOffsetMin *int `json:"last_tz_offset_min,omitempty"`
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

// applySessionOutcome folds one session's net wins/losses/draws into the stats
// in place and returns the session-level result. Pure (no I/O) so the streak
// logic is unit-testable. A net-winning session extends/flips a win streak, a
// net-losing session a loss streak; an even session (equal, or draws only)
// leaves the streak unchanged but is still recorded (Draws, recent form).
func applySessionOutcome(s *UserGameStats, wins, losses, draws int) string {
	s.Wins += wins
	s.Losses += losses
	s.Draws += draws

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

	letter := "D"
	switch result {
	case "win":
		letter = "W"
	case "loss":
		letter = "L"
	}
	s.RecentForm = append(s.RecentForm, letter)
	if len(s.RecentForm) > recentFormCap {
		s.RecentForm = s.RecentForm[len(s.RecentForm)-recentFormCap:]
	}

	return result
}

// applySessionPlayTime folds wall/active minutes into lifetime totals and the
// rolling daily window. Pure (no I/O) so the cap/prune logic is unit-testable.
func applySessionPlayTime(s *UserGameStats, durationMin, activeMin int, now time.Time) {
	s.PlayedMinTotal += durationMin
	s.ActiveMinTotal += activeMin

	if durationMin == 0 && activeMin == 0 {
		return
	}

	dateStr := now.UTC().Format("2006-01-02")
	for i := range s.RecentDays {
		if s.RecentDays[i].Date == dateStr {
			s.RecentDays[i].WallMin += durationMin
			s.RecentDays[i].ActiveMin += activeMin
			return
		}
	}
	// A session can land on an older day than the newest entry (a pending
	// queue flushed late), so insert by date and keep the order.
	s.RecentDays = append(s.RecentDays, RecentDayEntry{
		Date:      dateStr,
		WallMin:   durationMin,
		ActiveMin: activeMin,
	})
	sort.SliceStable(s.RecentDays, func(i, j int) bool { return s.RecentDays[i].Date < s.RecentDays[j].Date })
	if len(s.RecentDays) > recentDaysCap {
		s.RecentDays = s.RecentDays[len(s.RecentDays)-recentDaysCap:]
	}
}

// UpdateUserGameStats reads, applies the session outcome, and writes with
// optimistic-concurrency retry. Returns the updated stats and the session
// result ("win"/"loss"/"even"). Outcome-only — used by dev_seed; play-time
// fields are untouched.
func UpdateUserGameStats(ctx context.Context, nk runtime.NakamaModule, userID, gameID string, wins, losses, draws int) (*UserGameStats, string, error) {
	for attempt := 0; attempt < 3; attempt++ {
		s, version := readUserGameStats(ctx, nk, userID, gameID)
		result := applySessionOutcome(s, wins, losses, draws)
		now := time.Now().UnixMilli()
		s.UpdatedAt = now
		s.LastPlayed = now
		if err := writeUserGameStats(ctx, nk, userID, s, version); err == nil {
			return s, result, nil
		}
		jitter, _ := rand.Int(rand.Reader, big.NewInt(50))
		time.Sleep(time.Duration(50*(attempt+1)+int(jitter.Int64())) * time.Millisecond)
	}
	return nil, "", fmt.Errorf("user_game_stats write failed after 3 retries for user %s game %s", userID, gameID)
}

// UpdateUserGameStatsForSession records play time for every session and applies
// telemetry outcomes when present. tzOffsetMin, when not nil, replaces the
// stored last offset. endAt is when the session ended: it picks the day bucket
// and LastPlayed. Returns the session result only when
// wins+losses+draws > 0 (otherwise "").
func UpdateUserGameStatsForSession(ctx context.Context, nk runtime.NakamaModule, userID, gameID string, durationMin, activeMin, wins, losses, draws int, tzOffsetMin *int, endAt time.Time) (*UserGameStats, string, error) {
	for attempt := 0; attempt < 3; attempt++ {
		s, version := readUserGameStats(ctx, nk, userID, gameID)
		applySessionPlayTime(s, durationMin, activeMin, endAt)
		if tzOffsetMin != nil {
			tz := *tzOffsetMin
			s.LastTzOffsetMin = &tz
		}

		result := ""
		if wins > 0 || losses > 0 || draws > 0 {
			result = applySessionOutcome(s, wins, losses, draws)
		}

		s.UpdatedAt = time.Now().UnixMilli()
		if endMs := endAt.UnixMilli(); endMs > s.LastPlayed {
			s.LastPlayed = endMs
		}
		if err := writeUserGameStats(ctx, nk, userID, s, version); err == nil {
			return s, result, nil
		}
		jitter, _ := rand.Int(rand.Reader, big.NewInt(50))
		time.Sleep(time.Duration(50*(attempt+1)+int(jitter.Int64())) * time.Millisecond)
	}
	return nil, "", fmt.Errorf("user_game_stats write failed after 3 retries for user %s game %s", userID, gameID)
}

// ---------------------------------------------------------------------------
// user_game_stats_get RPC — the caller's own per-game stats, newest first.
// Backs the personal "You strip" + profile (spec 19). Owner-only by storage
// permission; this returns only the authenticated caller's documents.
// ---------------------------------------------------------------------------

type UserGameStatsListResponse struct {
	Games []UserGameStats `json:"games"`
}

func UserGameStatsGetRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	games := make([]UserGameStats, 0)
	cursor := ""
	for {
		objects, next, err := nk.StorageList(ctx, userID, userID, UserGameStatsCollection, 100, cursor)
		if err != nil {
			logger.Error("user_game_stats list failed for %s: %v", userID, err)
			return "", runtime.NewError("failed to read stats", 13)
		}
		for _, o := range objects {
			var s UserGameStats
			if err := json.Unmarshal([]byte(o.GetValue()), &s); err == nil {
				games = append(games, s)
			}
		}
		if next == "" {
			break
		}
		cursor = next
	}

	sort.Slice(games, func(i, j int) bool { return games[i].LastPlayed > games[j].LastPlayed })

	resp := UserGameStatsListResponse{Games: games}
	data, _ := json.Marshal(resp)
	return string(data), nil
}
