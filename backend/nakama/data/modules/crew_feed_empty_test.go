package main

import "testing"

func TestCardHasContent(t *testing.T) {
	snaps := []string{"a.jpg", "b.jpg"}
	cases := []struct {
		name        string
		feedType    string
		backendType string
		data        interface{}
		want        bool
	}{
		{"zero-minute voice session", "session", "voice_session", VoiceSessionData{DurationMin: 0}, false},
		{"voice session with minutes", "session", "voice_session", VoiceSessionData{DurationMin: 12}, true},
		{"zero-minute game session", "session", "game_session", GameSessionData{GameName: "Valorant"}, false},
		{"game session, inflated wall time, no active time", "session", "game_session", GameSessionData{DurationMin: 600, ActiveMin: 0}, true},
		{"game session with minutes", "session", "game_session", GameSessionData{DurationMin: 45}, true},
		{"zero-minute stream session", "session", "stream_session", StreamSessionData{}, false},
		{"stream session with minutes", "session", "stream_session", StreamSessionData{DurationMin: 5}, true},
		{"preview with no snapshots", "session-preview", "stream_session", StreamSessionData{DurationMin: 30}, false},
		{"preview with blank snapshot urls", "session-preview", "stream_session", StreamSessionData{DurationMin: 30, SnapshotURLs: []string{""}}, false},
		{"preview with snapshots", "session-preview", "stream_session", StreamSessionData{SnapshotURLs: snaps}, true},
		{"empty recap", "recap", "weekly_recap", WeeklyRecapData{CrewID: "c", WeekStart: 5}, false},
		{"recap with zeros in a map", "recap", "weekly_recap", map[string]interface{}{"total_hangout_min": 0, "clip_count": 0}, false},
		{"recap with hangout minutes", "recap", "weekly_recap", WeeklyRecapData{TotalHangoutMin: 90}, true},
		{"recap with clips only", "recap", "weekly_recap", WeeklyRecapData{ClipCount: 2}, true},
		{"recent games with no tiles", "recent-games", "recent_games", RecentGamesData{TotalMin: 10}, false},
		{"recent games with no minutes", "recent-games", "recent_games", RecentGamesData{Games: []RecentGameTile{{GameID: "g"}}}, false},
		{"recent games with a tile", "recent-games", "recent_games", RecentGamesData{TotalMin: 10, Games: []RecentGameTile{{GameID: "g", Minutes: 10}}}, true},
		{"clip", "clip", "clip", nil, true},
		{"catch-up", "catchup", "member_joined", nil, true},
		{"payload absent is kept", "session", "voice_session", nil, true},
	}
	for _, tc := range cases {
		if got := cardHasContent(tc.feedType, tc.backendType, tc.data); got != tc.want {
			t.Errorf("%s: got %v want %v", tc.name, got, tc.want)
		}
	}
}

// No empty card reaches this_week, whatever its type.
func TestBuildThisWeekDropsEmptyCards(t *testing.T) {
	now := feedTestNowMs
	cards := []feedCard{
		{id: "zero-voice", feedType: "session", backendType: "voice_session", ts: now - 1, data: VoiceSessionData{}},
		{id: "zero-game", feedType: "session", backendType: "game_session", ts: now - 2, data: GameSessionData{GameName: "Valorant"}},
		{id: "empty-recap", feedType: "recap", backendType: "weekly_recap", ts: now - 3, data: WeeklyRecapData{}},
		{id: "bare-preview", feedType: "session-preview", backendType: "stream_session", ts: now - 4, data: StreamSessionData{DurationMin: 20}},
		{id: "real-voice", feedType: "session", backendType: "voice_session", ts: now - 5, data: VoiceSessionData{DurationMin: 30}},
		clipCard("clip", now-6),
	}
	entries := buildThisWeek(cards, now)
	for _, id := range []string{"zero-voice", "zero-game", "empty-recap", "bare-preview"} {
		if findEntry(entries, id) != nil {
			t.Errorf("empty card %q was emitted", id)
		}
	}
	for _, id := range []string{"real-voice", "clip"} {
		if findEntry(entries, id) == nil {
			t.Errorf("real card %q was dropped", id)
		}
	}
}

// An empty recap must not take the hero slot on a Monday.
func TestEmptyRecapNeverHeroOnMonday(t *testing.T) {
	now := feedTestMonday
	cards := []feedCard{
		{id: "recap_1", feedType: "recap", backendType: "weekly_recap", ts: now - 1000, data: WeeklyRecapData{}},
	}
	if entries := buildThisWeek(cards, now); len(entries) != 0 {
		t.Fatalf("want no entries, got %d (%q)", len(entries), entries[0].ID)
	}
}

// Memory applies the same rule: stored empty recaps and bare previews stay out.
func TestMemoryEntriesDropEmptyCards(t *testing.T) {
	var out []FeedEntry
	out = appendEntryIfContent(out, FeedEntry{ID: "recap_1", Type: "recap", Data: WeeklyRecapData{}}, "weekly_recap")
	out = appendEntryIfContent(out, FeedEntry{ID: "recap_2", Type: "recap", Data: WeeklyRecapData{TotalHangoutMin: 60}}, "weekly_recap")
	out = appendEntryIfContent(out, FeedEntry{ID: "s1", Type: "session", Data: StoredStreamSession{}}, "stream_session")
	out = appendEntryIfContent(out, FeedEntry{ID: "s2", Type: "session-preview", Data: StoredStreamSession{DurationMin: 9}}, "stream_session")
	out = appendEntryIfContent(out, FeedEntry{ID: "s3", Type: "session-preview", Data: StoredStreamSession{SnapshotURLs: []string{"x"}}}, "stream_session")
	out = appendEntryIfContent(out, FeedEntry{ID: "c1", Type: "clip", Data: nil}, "clip")
	got := map[string]bool{}
	for _, e := range out {
		got[e.ID] = true
	}
	if len(out) != 3 || !got["recap_2"] || !got["s3"] || !got["c1"] {
		t.Fatalf("memory kept %v, want recap_2, s3, c1", got)
	}
}
