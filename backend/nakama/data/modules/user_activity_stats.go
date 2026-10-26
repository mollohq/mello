package main

// Per-user activity stats doc (plans/next-gen-feed.md sections 4 and 5, spec
// features/HEAD2HEAD.md). It backs the session titles of head2head: mouth,
// marathon, last_one_up, clipper, rage_quit and winner.
//
// One doc per user, collection user_activity_stats, key "stats". Read 0 and
// write 0: only the server reads and writes it. Head2head reads it as the
// system and publishes only the title holders, never the counters.
//
// The doc holds lifetime counters and 14 daily buckets keyed by UTC date. A
// write drops the buckets older than 14 days.

import (
	"context"
	"crypto/rand"
	"encoding/json"
	"fmt"
	"math/big"
	"sort"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

const (
	UserActivityStatsCollection = "user_activity_stats"
	userActivityStatsKey        = "stats"

	// activityDaysCap is the number of UTC days the daily buckets cover,
	// today included.
	activityDaysCap = 14

	// shortSessionMinMin and shortSessionMaxMin bound a short session in
	// minutes, both ends included. Rage quit counts them.
	shortSessionMinMin = 2
	shortSessionMaxMin = 10
)

// ActivityDay is one UTC day of activity. LatestEndLocalMin is the minute of
// the local day of the latest session end that day, where latest means latest
// from 18:00 (see activityNightKey). Nil when no session that day carried a
// time zone offset.
type ActivityDay struct {
	Date              string `json:"date"` // "YYYY-MM-DD" (UTC)
	VoiceMin          int    `json:"voice_min"`
	GameMin           int    `json:"game_min"`
	Sessions          int    `json:"sessions"`
	ShortSessions     int    `json:"short_sessions"`
	LongestSessionMin int    `json:"longest_session_min"`
	LatestEndLocalMin *int   `json:"latest_end_local_min,omitempty"`
	WinMoments        int    `json:"win_moments"`
	Clips             int    `json:"clips"`
}

// UserActivityStats is the per-user doc. The top-level counters are lifetime.
// Days holds the last 14 UTC days, oldest first.
type UserActivityStats struct {
	VoiceMin          int `json:"voice_min"`
	GameMin           int `json:"game_min"`
	Sessions          int `json:"sessions"`
	ShortSessions     int `json:"short_sessions"`
	LongestSessionMin int `json:"longest_session_min"`
	WinMoments        int `json:"win_moments"`
	Clips             int `json:"clips"`
	// LatestEndLocalMin is the minute of the local day of the latest session
	// end seen so far, ranked from 18:00. LatestEndDate is the local date of
	// that session. Nil and empty until a session carries an offset.
	LatestEndLocalMin *int   `json:"latest_end_local_min,omitempty"`
	LatestEndDate     string `json:"latest_end_date,omitempty"`
	// TzOffsetMin is the UTC offset of the member's clock in minutes, east
	// positive, from the latest session that sent one. The Monday recap push
	// reads it to send at 18:00 local. Nil until a session carries an offset.
	TzOffsetMin *int          `json:"tz_offset_min,omitempty"`
	Days        []ActivityDay `json:"days"`
	UpdatedAt   int64         `json:"updated_at"`
}

// activityDelta is one write to the doc. A zero field changes nothing.
type activityDelta struct {
	VoiceMin      int
	GameMin       int
	Sessions      int
	ShortSessions int
	// SessionMin is the length of one finished game session. It only feeds
	// the longest session figures.
	SessionMin int
	// EndLocalMin and EndLocalDate describe the end of one finished game
	// session on the local clock. Nil EndLocalMin means no offset was sent.
	EndLocalMin  *int
	EndLocalDate string
	// TzOffsetMin is the offset of that clock. Nil keeps the stored one.
	TzOffsetMin *int
	WinMoments  int
	Clips       int
}

// activityNightKey ranks a local minute of the day from 18:00, so 01:15 ranks
// after 23:30.
func activityNightKey(localMin int) int {
	return (localMin - head2headNightHr*60 + 1440) % 1440
}

// sessionEndLocal returns the minute of the local day and the local date for a
// session that ended at endMs on a clock tzOffsetMin minutes east of UTC.
func sessionEndLocal(endMs int64, tzOffsetMin int) (int, string) {
	local := time.UnixMilli(endMs).UTC().Add(time.Duration(tzOffsetMin) * time.Minute)
	return local.Hour()*60 + local.Minute(), local.Format("2006-01-02")
}

// isShortSession reports whether a session of the given minutes counts as a
// short session.
func isShortSession(min int) bool {
	return min >= shortSessionMinMin && min <= shortSessionMaxMin
}

// applyActivityDelta folds one delta into the doc at the given time and drops
// the buckets older than 14 days. Pure, so the roll is unit-testable.
func applyActivityDelta(s *UserActivityStats, d activityDelta, now time.Time) {
	s.VoiceMin += d.VoiceMin
	s.GameMin += d.GameMin
	s.Sessions += d.Sessions
	s.ShortSessions += d.ShortSessions
	s.WinMoments += d.WinMoments
	s.Clips += d.Clips
	if d.SessionMin > s.LongestSessionMin {
		s.LongestSessionMin = d.SessionMin
	}
	if d.TzOffsetMin != nil {
		v := *d.TzOffsetMin
		s.TzOffsetMin = &v
	}
	if d.EndLocalMin != nil {
		if s.LatestEndLocalMin == nil || activityNightKey(*d.EndLocalMin) >= activityNightKey(*s.LatestEndLocalMin) {
			v := *d.EndLocalMin
			s.LatestEndLocalMin = &v
			s.LatestEndDate = d.EndLocalDate
		}
	}

	date := now.UTC().Format("2006-01-02")
	idx := -1
	for i := range s.Days {
		if s.Days[i].Date == date {
			idx = i
			break
		}
	}
	if idx < 0 {
		s.Days = append(s.Days, ActivityDay{Date: date})
		idx = len(s.Days) - 1
	}
	day := &s.Days[idx]
	day.VoiceMin += d.VoiceMin
	day.GameMin += d.GameMin
	day.Sessions += d.Sessions
	day.ShortSessions += d.ShortSessions
	day.WinMoments += d.WinMoments
	day.Clips += d.Clips
	if d.SessionMin > day.LongestSessionMin {
		day.LongestSessionMin = d.SessionMin
	}
	if d.EndLocalMin != nil {
		if day.LatestEndLocalMin == nil || activityNightKey(*d.EndLocalMin) >= activityNightKey(*day.LatestEndLocalMin) {
			v := *d.EndLocalMin
			day.LatestEndLocalMin = &v
		}
	}

	// Drop what is older than the window, counted back from the newest day
	// held, so a late session does not push the window into the past.
	newest := now.UnixMilli()
	for _, dd := range s.Days {
		if t, err := time.Parse("2006-01-02", dd.Date); err == nil && t.UnixMilli() > newest {
			newest = t.UnixMilli()
		}
	}
	cutoff := cutoffDate(newest, activityDaysCap)
	kept := s.Days[:0]
	for _, dd := range s.Days {
		if dd.Date >= cutoff {
			kept = append(kept, dd)
		}
	}
	s.Days = kept
	sort.Slice(s.Days, func(i, j int) bool { return s.Days[i].Date < s.Days[j].Date })
	s.UpdatedAt = now.UnixMilli()
}

func readUserActivityStats(ctx context.Context, nk runtime.NakamaModule, userID string) (*UserActivityStats, string) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: UserActivityStatsCollection, Key: userActivityStatsKey, UserID: userID},
	})
	if err != nil || len(objects) == 0 {
		return &UserActivityStats{Days: []ActivityDay{}}, ""
	}
	var s UserActivityStats
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &s); err != nil {
		return &UserActivityStats{Days: []ActivityDay{}}, ""
	}
	if s.Days == nil {
		s.Days = []ActivityDay{}
	}
	return &s, objects[0].GetVersion()
}

func writeUserActivityStats(ctx context.Context, nk runtime.NakamaModule, userID string, s *UserActivityStats, version string) error {
	data, err := json.Marshal(s)
	if err != nil {
		return err
	}
	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{{
		Collection:      UserActivityStatsCollection,
		Key:             userActivityStatsKey,
		UserID:          userID,
		Value:           string(data),
		Version:         version,
		PermissionRead:  0, // server only: head2head reads it as the system
		PermissionWrite: 0, // server only
	}})
	return err
}

// UpdateUserActivityStats reads, applies the delta and writes with optimistic
// concurrency retry, like UpdateUserGameStatsForSession.
func UpdateUserActivityStats(ctx context.Context, nk runtime.NakamaModule, userID string, d activityDelta, now time.Time) error {
	for attempt := 0; attempt < 3; attempt++ {
		s, version := readUserActivityStats(ctx, nk, userID)
		applyActivityDelta(s, d, now)
		if err := writeUserActivityStats(ctx, nk, userID, s, version); err == nil {
			return nil
		}
		jitter, _ := rand.Int(rand.Reader, big.NewInt(50))
		time.Sleep(time.Duration(50*(attempt+1)+int(jitter.Int64())) * time.Millisecond)
	}
	return fmt.Errorf("user_activity_stats write failed after 3 retries for user %s", userID)
}

// recordGameSessionActivity writes one finished game session: its minutes, the
// session and short session counts, the longest session and the local end
// clock. endMs is the session end, tz the validated offset or nil. A session
// under one minute counts as nothing. Never fails the caller.
func recordGameSessionActivity(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID string, durationMin int, endMs int64, tz *int) {
	if durationMin < 1 {
		return
	}
	d := activityDelta{GameMin: durationMin, Sessions: 1, SessionMin: durationMin}
	if isShortSession(durationMin) {
		d.ShortSessions = 1
	}
	if tz != nil {
		d.EndLocalMin = new(int)
		*d.EndLocalMin, d.EndLocalDate = sessionEndLocal(endMs, *tz)
		v := *tz
		d.TzOffsetMin = &v
	}
	if err := UpdateUserActivityStats(ctx, nk, userID, d, time.UnixMilli(endMs)); err != nil {
		logger.Warn("user_activity_stats game session write failed: %v", err)
	}
}

// recordClipActivity counts one posted clip. Never fails the caller.
func recordClipActivity(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID string) {
	if err := UpdateUserActivityStats(ctx, nk, userID, activityDelta{Clips: 1}, time.Now()); err != nil {
		logger.Warn("user_activity_stats clip write failed: %v", err)
	}
}

// recordWinMomentActivity counts one posted win moment. Never fails the caller.
func recordWinMomentActivity(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID string) {
	if err := UpdateUserActivityStats(ctx, nk, userID, activityDelta{WinMoments: 1}, time.Now()); err != nil {
		logger.Warn("user_activity_stats win moment write failed: %v", err)
	}
}

// recordVoiceActivity adds voice minutes. Never fails the caller.
func recordVoiceActivity(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID string, minutes int, at time.Time) {
	if minutes < 1 {
		return
	}
	if err := UpdateUserActivityStats(ctx, nk, userID, activityDelta{VoiceMin: minutes}, at); err != nil {
		logger.Warn("user_activity_stats voice write failed: %v", err)
	}
}
