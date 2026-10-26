package main

import (
	"encoding/json"
	"fmt"
	"testing"
	"time"
)

// scaleMembers builds n members that all have data for all 13 titles, so every
// pair carries 13 values: the worst case for the pair list.
func scaleMembers(n int) []h2hMemberData {
	out := make([]h2hMemberData, 0, n)
	for i := 0; i < n; i++ {
		id := fmt.Sprintf("user-%03d", i)
		lib := hdoc(
			hgame("steam-730", 1000+i*37),
			hgame(fmt.Sprintf("steam-%d", 2000+i%12), 50+i),
			hgame(fmt.Sprintf("steam-%d", 3000+i%7), 90+i*3),
		)
		tz := (i%24)*60 - 720
		stats := []UserGameStats{
			{
				GameID: "steam-730", PlayedMinTotal: 1000 + i*37,
				RecentDays:      []RecentDayEntry{{Date: dayAgo(i % 5), WallMin: 30 + i*7}, {Date: dayAgo(8), WallMin: 20}},
				LastPlayed:      h2hNow - int64(i%5)*3_600_000,
				LastTzOffsetMin: &tz,
			},
		}
		end := (i * 41) % 600
		act := &UserActivityStats{
			VoiceMin: 10 + i*3, GameMin: 100 + i, Sessions: 3 + i%4, ShortSessions: i % 5,
			LongestSessionMin: 40 + i*2, WinMoments: i % 3, Clips: i % 6,
			LatestEndLocalMin: &end,
			Days: []ActivityDay{{
				Date: dayAgo(1), VoiceMin: 10 + i*3, GameMin: 100 + i, Sessions: 3, ShortSessions: i % 5,
				LongestSessionMin: 40 + i*2, LatestEndLocalMin: &end, WinMoments: i % 3, Clips: i % 6,
			}},
		}
		out = append(out, h2hMemberData{UserID: id, Library: lib, Stats: stats, Activity: act})
	}
	return out
}

// The pair list grows with the square of the crew. Production has crews of 40
// and more (plans/next-gen-feed.md section 9, HEAD2HEAD.md section 3). The
// holders doc must stay quick to compute and small enough to store, and the
// feed must trim it to the viewer's pairs.
func TestHead2headStaysQuickAndSmallAtFiftyMembers(t *testing.T) {
	for _, n := range []int{50, 100} { // 100 is the member list cap
		ms := scaleMembers(n)
		start := time.Now()
		doc := compute(nil, h2hNow, n, ms...)
		took := time.Since(start)
		data, err := json.Marshal(doc)
		if err != nil {
			t.Fatal(err)
		}
		if want := n * (n - 1) / 2; len(doc.Pairs) != want {
			t.Fatalf("%d members: %d pairs, want %d", n, len(doc.Pairs), want)
		}
		if len(doc.Titles) != 13 {
			t.Fatalf("%d members: %d contested titles, want 13", n, len(doc.Titles))
		}
		t.Logf("%d members: %d pairs, compute %v, doc %d KB", n, len(doc.Pairs), took, len(data)/1024)
		if took > time.Second {
			t.Fatalf("%d members: compute took %v, over 1 s", n, took)
		}
		// Nakama accepts a storage value up to 2 MB in the default config.
		if n == 50 && len(data) > 1<<20 {
			t.Fatalf("%d members: doc is %d bytes, over 1 MB", n, len(data))
		}

		feed := trimHead2headForViewer(doc, "crew", "user-007", n, h2hNow)
		if len(feed.Pairs) != n-1 {
			t.Fatalf("%d members: the viewer has %d pairs in the payload, want %d", n, len(feed.Pairs), n-1)
		}
		for _, p := range feed.Pairs {
			if p.A != "user-007" {
				t.Fatalf("a payload pair does not start with the viewer: %s", p.A)
			}
		}
		payload, _ := json.Marshal(feed)
		t.Logf("%d members: feed payload %d KB", n, len(payload)/1024)
	}
}
