package main

import (
	"context"
	"encoding/json"
	"testing"
	"time"
)

func tm(y int, mo time.Month, d, h, mi int) time.Time {
	return time.Date(y, mo, d, h, mi, 0, 0, time.UTC)
}

func readActivity(t *testing.T, nk *fakeGuestNk, userID string) UserActivityStats {
	t.Helper()
	raw, ok := nk.storage[fakeStorageKey(UserActivityStatsCollection, userActivityStatsKey, userID)]
	if !ok {
		t.Fatalf("no activity doc for %s", userID)
	}
	var s UserActivityStats
	if err := json.Unmarshal([]byte(raw), &s); err != nil {
		t.Fatal(err)
	}
	return s
}

func dayOf(t *testing.T, s UserActivityStats, date string) ActivityDay {
	t.Helper()
	for _, d := range s.Days {
		if d.Date == date {
			return d
		}
	}
	t.Fatalf("no bucket %s in %+v", date, s.Days)
	return ActivityDay{}
}

func TestActivityBucketsRollAndDropAfterFourteenDays(t *testing.T) {
	s := &UserActivityStats{Days: []ActivityDay{}}
	base := tm(2026, time.October, 1, 12, 0)
	// One clip on each of 20 days.
	for i := 0; i < 20; i++ {
		applyActivityDelta(s, activityDelta{Clips: 1}, base.AddDate(0, 0, i))
	}
	if len(s.Days) != activityDaysCap {
		t.Fatalf("buckets = %d, want %d", len(s.Days), activityDaysCap)
	}
	if s.Days[0].Date != "2026-10-07" || s.Days[13].Date != "2026-10-20" {
		t.Fatalf("window = %s..%s", s.Days[0].Date, s.Days[13].Date)
	}
	if s.Clips != 20 {
		t.Fatalf("lifetime clips = %d, want 20: lifetime counters never roll", s.Clips)
	}
}

func TestActivityBucketsStaySortedAndMergeSameDay(t *testing.T) {
	s := &UserActivityStats{}
	now := tm(2026, time.October, 19, 9, 0)
	applyActivityDelta(s, activityDelta{VoiceMin: 5}, now)
	applyActivityDelta(s, activityDelta{VoiceMin: 7}, now.Add(3*time.Hour))
	applyActivityDelta(s, activityDelta{VoiceMin: 2}, now.AddDate(0, 0, -2))
	if len(s.Days) != 2 || s.Days[0].Date != "2026-10-17" {
		t.Fatalf("days: %+v", s.Days)
	}
	if got := dayOf(t, *s, "2026-10-19").VoiceMin; got != 12 {
		t.Fatalf("same-day voice = %d, want 12", got)
	}
	if s.VoiceMin != 14 {
		t.Fatalf("lifetime voice = %d, want 14", s.VoiceMin)
	}
}

func TestActivityLongestKeepsLifetimeAndPerDayMax(t *testing.T) {
	s := &UserActivityStats{}
	now := tm(2026, time.October, 19, 9, 0)
	applyActivityDelta(s, activityDelta{SessionMin: 90}, now)
	applyActivityDelta(s, activityDelta{SessionMin: 30}, now)
	applyActivityDelta(s, activityDelta{SessionMin: 40}, now.AddDate(0, 0, 1))
	if s.LongestSessionMin != 90 {
		t.Fatalf("lifetime longest = %d", s.LongestSessionMin)
	}
	if dayOf(t, *s, "2026-10-19").LongestSessionMin != 90 || dayOf(t, *s, "2026-10-20").LongestSessionMin != 40 {
		t.Fatalf("per-day longest: %+v", s.Days)
	}
	// The lifetime figure survives the day it was set on leaving the window.
	applyActivityDelta(s, activityDelta{SessionMin: 10}, now.AddDate(0, 0, 30))
	if s.LongestSessionMin != 90 {
		t.Fatalf("lifetime longest after the roll = %d", s.LongestSessionMin)
	}
}

func TestActivityLatestEndRanksFromEighteen(t *testing.T) {
	s := &UserActivityStats{}
	now := tm(2026, time.October, 19, 9, 0)
	at := func(m int) activityDelta {
		v := m
		return activityDelta{EndLocalMin: &v, EndLocalDate: "2026-10-19"}
	}
	applyActivityDelta(s, at(23*60+30), now)
	applyActivityDelta(s, at(60+15), now) // 01:15 is later than 23:30
	applyActivityDelta(s, at(19*60), now) // earlier, must not replace
	if s.LatestEndLocalMin == nil || *s.LatestEndLocalMin != 75 {
		t.Fatalf("lifetime latest = %v, want 75", s.LatestEndLocalMin)
	}
	if got := dayOf(t, *s, "2026-10-19").LatestEndLocalMin; got == nil || *got != 75 {
		t.Fatalf("day latest = %v", got)
	}
	if s.LatestEndDate != "2026-10-19" {
		t.Fatalf("date = %q", s.LatestEndDate)
	}
}

func TestSessionEndLocal(t *testing.T) {
	end := tm(2026, time.October, 19, 23, 30).UnixMilli()
	m, date := sessionEndLocal(end, 120) // UTC+2: 01:30 the next day
	if m != 90 || date != "2026-10-20" {
		t.Fatalf("got %d %s", m, date)
	}
	m, date = sessionEndLocal(end, -300)
	if m != 18*60+30 || date != "2026-10-19" {
		t.Fatalf("got %d %s", m, date)
	}
}

func TestIsShortSession(t *testing.T) {
	for min, want := range map[int]bool{0: false, 1: false, 2: true, 3: true, 10: true, 11: false, 90: false} {
		if isShortSession(min) != want {
			t.Errorf("isShortSession(%d) = %v", min, !want)
		}
	}
}

func TestActivityDocIsServerOnly(t *testing.T) {
	nk := newFakeGuestNk()
	recordClipActivity(context.Background(), testLogger(), nk, "u1")
	got := nk.perms[fakeStorageKey(UserActivityStatsCollection, userActivityStatsKey, "u1")]
	if got != [2]int{0, 0} {
		t.Fatalf("permissions read,write = %v, want 0,0", got)
	}
}

func TestGameSessionWriterCountsShortAndLongSessions(t *testing.T) {
	nk := newFakeGuestNk()
	ctx, lg := context.Background(), testLogger()
	end := tm(2026, time.October, 19, 22, 0).UnixMilli()
	tz := 60
	recordGameSessionActivity(ctx, lg, nk, "u1", 3, end, &tz)
	recordGameSessionActivity(ctx, lg, nk, "u1", 3, end, nil)
	recordGameSessionActivity(ctx, lg, nk, "u1", 90, end, &tz)
	recordGameSessionActivity(ctx, lg, nk, "u1", 0, end, nil) // counts as nothing
	s := readActivity(t, nk, "u1")
	if s.Sessions != 3 || s.ShortSessions != 2 || s.GameMin != 96 || s.LongestSessionMin != 90 {
		t.Fatalf("doc: %+v", s)
	}
	if s.LatestEndLocalMin == nil || *s.LatestEndLocalMin != 23*60 {
		t.Fatalf("latest end = %v, want 23:00 local", s.LatestEndLocalMin)
	}
	d := dayOf(t, s, "2026-10-19")
	if d.Sessions != 3 || d.ShortSessions != 2 || d.GameMin != 96 || d.LongestSessionMin != 90 {
		t.Fatalf("day: %+v", d)
	}
}

func TestGameSessionEndRPCWritesActivity(t *testing.T) {
	nk := newFakeGuestNk()
	nk.addMember("crew", "u1", "a")
	payload := `{"crew_id":"crew","game_name":"G","duration_min":5,"active_min":5,"tz_offset_min":0}`
	if _, err := GameSessionEndRPC(ctxWithUser("u1"), testLogger(), nil, nk, payload); err != nil {
		t.Fatal(err)
	}
	s := readActivity(t, nk, "u1")
	if s.Sessions != 1 || s.ShortSessions != 1 || s.GameMin != 5 || s.LatestEndLocalMin == nil {
		t.Fatalf("doc: %+v", s)
	}
}

func TestGameSessionEndRPCDuplicateIsNotCountedTwice(t *testing.T) {
	nk := newFakeGuestNk()
	nk.addMember("crew", "u1", "a")
	// Mark the session as seen already.
	nk.put(gameSessionDedupCollection, "s1", "u1", map[string]int{"ts": 1})
	payload := `{"crew_id":"crew","game_name":"G","duration_min":5,"session_id":"s1"}`
	if _, err := GameSessionEndRPC(ctxWithUser("u1"), testLogger(), nil, nk, payload); err != nil {
		t.Fatal(err)
	}
	if _, ok := nk.storage[fakeStorageKey(UserActivityStatsCollection, userActivityStatsKey, "u1")]; ok {
		t.Fatal("a duplicate session end must not write the activity doc")
	}
}

func TestPostMomentWinCountsOthersDoNot(t *testing.T) {
	nk := newFakeGuestNk()
	nk.addMember("crew", "u1", "a")
	post := func(sentiment string) {
		t.Helper()
		p := `{"crew_id":"crew","sentiment":"` + sentiment + `","text":"x"}`
		if _, err := PostMomentRPC(ctxWithUser("u1"), testLogger(), nil, nk, p); err != nil {
			t.Fatal(err)
		}
	}
	post("loss")
	post("highlight")
	if _, ok := nk.storage[fakeStorageKey(UserActivityStatsCollection, userActivityStatsKey, "u1")]; ok {
		t.Fatal("only a win moment counts")
	}
	post("win")
	post("win")
	s := readActivity(t, nk, "u1")
	if s.WinMoments != 2 || dayOf(t, s, time.Now().UTC().Format("2006-01-02")).WinMoments != 2 {
		t.Fatalf("doc: %+v", s)
	}
}

func TestPostClipCounts(t *testing.T) {
	nk := newFakeGuestNk()
	nk.addMember("crew", "u1", "a")
	for _, id := range []string{"c1", "c2"} {
		p := `{"crew_id":"crew","clip_id":"` + id + `","clip_type":"voice","duration_seconds":10}`
		if _, err := PostClipRPC(ctxWithUser("u1"), testLogger(), nil, nk, p); err != nil {
			t.Fatal(err)
		}
	}
	if got := readActivity(t, nk, "u1").Clips; got != 2 {
		t.Fatalf("clips = %d, want 2", got)
	}
}
