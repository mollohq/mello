package main

import (
	"context"
	"fmt"
	"testing"
	"time"
)

func TestResolveSessionTimes(t *testing.T) {
	now := tm(2026, time.October, 20, 12, 0)
	nowMs := now.UnixMilli()
	min := int64(60_000)
	day := 24 * 60 * min
	cases := []struct {
		name       string
		start, end int64
		wantUsed   bool // the client times are used
		wantOK     bool // no warning
	}{
		{"recent session", nowMs - 3*day - 60*min, nowMs - 3*day, true, true},
		{"slight future skew", nowMs - 30*min, nowMs + 4*min, true, true},
		{"end too far in the future", nowMs, nowMs + 6*min, false, false},
		{"end before start", nowMs - 10*min, nowMs - 20*min, false, false},
		{"span of a day", nowMs - 25*60*min, nowMs - 60*min, false, false},
		{"older than 90 days", nowMs - 91*day - 60*min, nowMs - 91*day, false, false},
		{"start missing", 0, nowMs - 60*min, false, false},
	}
	for _, c := range cases {
		start, end, ok := resolveSessionTimes(c.start, c.end, 30, now)
		if ok != c.wantOK {
			t.Errorf("%s: ok = %v, want %v", c.name, ok, c.wantOK)
		}
		if c.wantUsed && (start != c.start || end != c.end) {
			t.Errorf("%s: got %d..%d, want the client times", c.name, start, end)
		}
		if !c.wantUsed && (end != nowMs || start != nowMs-30*min) {
			t.Errorf("%s: got %d..%d, want receive time", c.name, start, end)
		}
	}
	// An older client sends nothing: receive time, no warning.
	if _, end, ok := resolveSessionTimes(0, 0, 30, now); end != nowMs || !ok {
		t.Errorf("no times: end=%d ok=%v", end, ok)
	}
}

// A session that waited in the client's queue lands on the day it was played:
// ledger event, user_game_stats bucket and user_activity_stats bucket.
func TestGameSessionEndRPCStampsClientTimes(t *testing.T) {
	nk := newFakeGuestNk()
	nk.addMember("crew", "u1", "a")
	endMs := time.Now().Add(-5 * 24 * time.Hour).Truncate(time.Minute).UnixMilli()
	startMs := endMs - 40*60_000
	payload := fmt.Sprintf(`{"crew_id":"crew","game_name":"G","game_id":"steam-1","duration_min":40,"active_min":40,"tz_offset_min":0,"start_ms":%d,"end_ms":%d}`, startMs, endMs)
	if _, err := GameSessionEndRPC(ctxWithUser("u1"), testLogger(), nil, nk, payload); err != nil {
		t.Fatal(err)
	}
	day := time.UnixMilli(endMs).UTC().Format("2006-01-02")

	ledger, _ := readLedger(context.Background(), nk, "crew")
	if len(ledger.Events) != 1 || ledger.Events[0].Timestamp != endMs {
		t.Fatalf("ledger event stamp: %+v, want %d", ledger.Events, endMs)
	}
	g, _ := readUserGameStats(context.Background(), nk, "u1", "steam-1")
	if len(g.RecentDays) != 1 || g.RecentDays[0].Date != day || g.RecentDays[0].WallMin != 40 {
		t.Fatalf("user_game_stats days: %+v, want %s", g.RecentDays, day)
	}
	if g.LastPlayed != endMs {
		t.Fatalf("last played %d, want %d", g.LastPlayed, endMs)
	}
	a := readActivity(t, nk, "u1")
	if d := dayOf(t, a, day); d.GameMin != 40 || d.Sessions != 1 {
		t.Fatalf("activity day: %+v", d)
	}
}

func TestGameSessionEndRPCFallsBackToReceiveTime(t *testing.T) {
	nk := newFakeGuestNk()
	nk.addMember("crew", "u1", "a")
	before := time.Now().UnixMilli()
	// End in the future by an hour: not sane.
	endMs := time.Now().Add(time.Hour).UnixMilli()
	payload := fmt.Sprintf(`{"crew_id":"crew","game_name":"G","game_id":"steam-1","duration_min":40,"active_min":40,"start_ms":%d,"end_ms":%d}`, endMs-40*60_000, endMs)
	if _, err := GameSessionEndRPC(ctxWithUser("u1"), testLogger(), nil, nk, payload); err != nil {
		t.Fatal(err)
	}
	ledger, _ := readLedger(context.Background(), nk, "crew")
	ts := ledger.Events[0].Timestamp
	if ts < before || ts > time.Now().UnixMilli() {
		t.Fatalf("stamp %d, want the receive time", ts)
	}
}

// Sessions arrive out of order (a queue flushed late): the day buckets stay
// sorted and a late old session does not push the newest out.
func TestRecentDaysStaySortedForALateSession(t *testing.T) {
	s := &UserGameStats{GameID: "steam-1"}
	base := tm(2026, time.October, 10, 12, 0)
	applySessionPlayTime(s, 10, 10, base)
	applySessionPlayTime(s, 20, 20, base.AddDate(0, 0, -3))
	if len(s.RecentDays) != 2 || s.RecentDays[0].Date != "2026-10-07" || s.RecentDays[1].Date != "2026-10-10" {
		t.Fatalf("days: %+v", s.RecentDays)
	}
}
