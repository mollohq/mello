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
		{"recent games, installed only", "recent-games", "recent_games", RecentGamesData{Games: []RecentGameTile{{GameID: "g", MemberIDs: []string{"u1"}}}}, true},
		{"clip", "clip", "clip", nil, true},
		{"daily recap with minutes and three members", "daily-recap", "", DailyRecapData{TotalMin: 40, TotalHangoutMin: 40, MemberIDs: []string{"a", "b", "c"}}, true},
		{"daily recap with minutes and two members", "daily-recap", "", DailyRecapData{TotalMin: 40, TotalHangoutMin: 40, MemberIDs: []string{"a", "b"}}, false},
		{"daily recap with minutes and a belt", "daily-recap", "", DailyRecapData{TotalMin: 40, TotalHangoutMin: 40, BeltsMoved: []RecapBeltMoved{{TitleID: "the_carry"}}}, true},
		{"daily recap with a clip only", "daily-recap", "", DailyRecapData{ClipCount: 1}, true},
		{"daily recap with a belt only", "daily-recap", "", DailyRecapData{BeltsMoved: []RecapBeltMoved{{TitleID: "the_carry"}}}, false},
		{"empty daily recap", "daily-recap", "", DailyRecapData{DayStart: 5}, false},
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
	if findEntry(entries, "clip") == nil {
		t.Errorf("real card %q was dropped", "clip")
	}
	// A voice session with minutes is a row of the day card, not a card.
	if findEntry(entries, "real-voice") != nil {
		t.Errorf("the voice session was emitted as a card")
	}
}

// A voice session is never a card of this_week. The daily recap of its day
// still counts it and lists it (spec 19 section 3.5a).
func TestVoiceSessionsAreRowsOfTheDayCardNotCards(t *testing.T) {
	now := feedTestNowMs
	cards := []feedCard{
		{id: "voice", feedType: "session", backendType: "voice_session", ts: now - 1, data: VoiceSessionData{ChannelName: "General", DurationMin: 30, ParticipantIDs: []string{"u1", "u2", "u3"}}},
		clipCard("clip", now-6),
	}
	entries := buildThisWeek(cards, now)
	if findEntry(entries, "voice") != nil {
		t.Fatalf("the voice session is a card")
	}
	var daily *FeedEntry
	for i := range entries {
		if entries[i].Type == "daily-recap" {
			daily = &entries[i]
		}
	}
	if daily == nil {
		t.Fatalf("no daily recap for the day of the voice session: %+v", entries)
	}
	d, ok := daily.Data.(DailyRecapData)
	if !ok || d.SessionCount != 1 || len(d.Sessions) != 1 || d.Sessions[0].Kind != "voice" {
		t.Fatalf("daily recap %+v", daily.Data)
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

// The joined line: joins of the last 24 hours, newest first, at most three,
// one row for each joiner, current members only, never the viewer's own.
func TestBuildFeedJoins(t *testing.T) {
	now := feedTestNowMs
	h := int64(60 * 60 * 1000)
	join := func(id, actor string, ago int64, d MemberJoinedData) TimelineEntry {
		return TimelineEntry{ID: id, Type: "member_joined", ActorID: actor, Ts: now - ago, Data: d}
	}
	members := map[string]bool{"bob": true, "jonas": true, "max": true, "sara": true, "leo": true}
	entries := []TimelineEntry{
		join("e1", "max", 5*h, MemberJoinedData{DisplayName: "Max"}),
		join("e2", "jonas", 1*h, MemberJoinedData{DisplayName: "Jonas", InvitedBy: "bob", InvitedByName: "b0bben"}),
		join("e3", "sara", 23*h, MemberJoinedData{Username: "sara"}),
		join("e4", "leo", 25*h, MemberJoinedData{DisplayName: "Leo"}),                 // too old
		join("e5", "gone", 2*h, MemberJoinedData{DisplayName: "Gone"}),                // left the crew
		join("e6", "bob", 3*h, MemberJoinedData{DisplayName: "Bob"}),                  // the viewer
		join("e7", "jonas", 90*60*1000, MemberJoinedData{DisplayName: "Jonas again"}), // one row each
		{ID: "x", Type: "voice_session", ActorID: "max", Ts: now - h},
	}
	got := buildFeedJoins(entries, members, "bob", now)
	if len(got) != 3 {
		t.Fatalf("rows = %+v, want 3", got)
	}
	if got[0].UserID != "jonas" || got[0].Name != "Jonas" || got[0].InvitedByName != "b0bben" || got[0].InvitedBy != "bob" {
		t.Fatalf("first row = %+v, want Jonas invited by b0bben", got[0])
	}
	if got[1].UserID != "max" || got[2].UserID != "sara" || got[2].Name != "sara" {
		t.Fatalf("order = %+v, want newest first: jonas, max, sara", got)
	}

	// Four joiners in the day: the fourth waits.
	entries = append(entries, join("e8", "leo", 30*60*1000, MemberJoinedData{DisplayName: "Leo"}))
	if got = buildFeedJoins(entries, members, "bob", now); len(got) != 3 || got[0].UserID != "leo" {
		t.Fatalf("rows = %+v, want 3 with Leo first", got)
	}
	if got = buildFeedJoins(nil, members, "bob", now); len(got) != 0 {
		t.Fatalf("no events, rows = %+v", got)
	}
}

// An event read back from storage is a map, not the struct.
func TestBuildFeedJoinsReadsStoredEvents(t *testing.T) {
	now := feedTestNowMs
	entries := []TimelineEntry{{
		ID: "e", Type: "member_joined", ActorID: "jonas", Ts: now - 1000,
		Data: map[string]interface{}{"display_name": "Jonas", "invited_by": "bob", "invited_by_name": "b0bben"},
	}}
	got := buildFeedJoins(entries, map[string]bool{"jonas": true}, "bob", now)
	if len(got) != 1 || got[0].InvitedByName != "b0bben" {
		t.Fatalf("rows = %+v", got)
	}
}
