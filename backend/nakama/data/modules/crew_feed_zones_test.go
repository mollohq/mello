package main

import (
	"encoding/json"
	"strings"
	"testing"
)

// Thursday 5 November 2026, 12:00 UTC.
const zonesNow int64 = 1_793_880_000_000

func zClip(id, kind string, daysAgo float64, poster bool) StoredClip {
	c := StoredClip{
		EventID:         id,
		ClipID:          id,
		ActorID:         "u-" + id,
		ClipperName:     "Clipper " + id,
		ClipType:        kind,
		Ts:              zonesNow - int64(daysAgo*float64(dayMs)),
		DurationSeconds: 30,
		MediaURL:        "https://media.example/" + id + ".mp4",
	}
	if poster {
		c.PosterURL = "https://media.example/" + id + ".jpg"
	}
	return c
}

func zStream(id string, daysAgo float64, snapshots int) StoredStreamSession {
	s := StoredStreamSession{
		EventID:      id,
		StreamerID:   "u-" + id,
		StreamerName: "Streamer " + id,
		DurationMin:  60,
		Ts:           zonesNow - int64(daysAgo*float64(dayMs)),
	}
	for i := 0; i < snapshots; i++ {
		s.SnapshotURLs = append(s.SnapshotURLs, "https://media.example/"+id+"/snap.jpg")
	}
	return s
}

func zGameSession(id string, daysAgo float64, players ...string) feedCard {
	return feedCard{
		id: id, feedType: "session", backendType: "game_session",
		ts:   zonesNow - int64(daysAgo*float64(dayMs)),
		data: GameSessionData{GameName: "Counter-Strike 2", DurationMin: 60, PlayerNames: players},
	}
}

func zCrew(members int) *Head2headFeedData {
	return &Head2headFeedData{MemberCount: members, OpenSeats: max(0, 5-members)}
}

func zInput() zonesInput {
	return zonesInput{ViewerID: "viewer", NowMs: zonesNow, FreeDays: 7, H2h: zCrew(12)}
}

func entryIDs(es []FeedEntry) []string {
	out := make([]string, 0, len(es))
	for _, e := range es {
		out = append(out, e.ID)
	}
	return out
}

// ── Window ────────────────────────────────────────────────────────────────

func TestZonesClipsHoldTheFreeWindowOnly(t *testing.T) {
	in := zInput()
	in.Clips = []StoredClip{zClip("new", clipTypeVoice, 2, false), zClip("old", clipTypeVoice, 10, false)}
	z := buildFeedZones(in)
	if got := entryIDs(z.Clips); len(got) != 1 || got[0] != "new" {
		t.Fatalf("clips = %v, want [new]", got)
	}
	if len(z.History) != 1 || z.History[0].ClipCount != 1 {
		t.Fatalf("the old clip is one week of history, got %+v", z.History)
	}
}

func TestZonesFreeDaysComesFromTheConfig(t *testing.T) {
	in := zInput()
	in.FreeDays = 3
	in.Clips = []StoredClip{zClip("five-days", clipTypeVoice, 5, false)}
	z := buildFeedZones(in)
	if len(z.Clips) != 0 || len(z.History) != 1 {
		t.Fatalf("a 5-day-old clip is history with a 3-day window: clips %v history %d", entryIDs(z.Clips), len(z.History))
	}
	if z.FreeDays != 3 {
		t.Fatalf("free_days = %d, want 3", z.FreeDays)
	}
}

func TestZonesClipsAreGameFirstThenNewest(t *testing.T) {
	in := zInput()
	in.Clips = []StoredClip{
		zClip("voice-new", clipTypeVoice, 0.1, false),
		zClip("game-old", clipTypeGame, 3, false),
		zClip("game-new", clipTypeGame, 1, false),
	}
	got := entryIDs(buildFeedZones(in).Clips)
	want := []string{"game-new", "game-old", "voice-new"}
	if strings.Join(got, ",") != strings.Join(want, ",") {
		t.Fatalf("clips = %v, want %v", got, want)
	}
}

// ── Stage ─────────────────────────────────────────────────────────────────

func TestZonesStageIsTheNewestGameClipWithAPoster(t *testing.T) {
	in := zInput()
	in.Clips = []StoredClip{
		zClip("voice", clipTypeVoice, 0.1, false),
		zClip("no-poster", clipTypeGame, 0.2, false),
		zClip("hero", clipTypeGame, 1, true),
	}
	in.Streams = []StoredStreamSession{zStream("stream", 0.5, 6)}
	z := buildFeedZones(in)
	if z.Stage.Kind != "game_clip" || z.Stage.Entry.ID != "hero" {
		t.Fatalf("stage = %+v, want the game clip with a poster", z.Stage)
	}
	for _, id := range entryIDs(z.Clips) {
		if id == "hero" {
			t.Fatal("the stage clip is not repeated in the clips")
		}
	}
}

func TestZonesStageFallsBackToAStreamReplay(t *testing.T) {
	in := zInput()
	in.Clips = []StoredClip{zClip("voice", clipTypeVoice, 0.1, false)}
	in.Streams = []StoredStreamSession{zStream("short", 1, 1), zStream("best", 2, 12), zStream("old", 9, 12)}
	z := buildFeedZones(in)
	if z.Stage.Kind != "stream_replay" || z.Stage.Entry.ID != "best" {
		t.Fatalf("stage = %+v, want the best replay of the window", z.Stage)
	}
}

func TestZonesStageIsTheBeltHeroOfAQuietFeed(t *testing.T) {
	in := zInput()
	in.H2h.Titles = []Head2headTitle{{TitleID: "night_owl", HolderUserID: "sara", PreviousHolderUserID: "viewer", MovedAtMs: zonesNow - 6*dayMs}}
	if k := buildFeedZones(in).Stage.Kind; k != "belt" {
		t.Fatalf("stage = %q, want belt", k)
	}
	in.H2h.Titles[0].PreviousHolderUserID = "leo"
	if k := buildFeedZones(in).Stage.Kind; k != "" {
		t.Fatalf("a move between two others leads nothing, got %q", k)
	}
}

func TestZonesStageOfACrewOfOneIsItsOwnBelts(t *testing.T) {
	in := zInput()
	in.H2h = zCrew(1)
	in.H2h.OwnValues = []Head2headOwnValue{{TitleID: "night_owl"}}
	z := buildFeedZones(in)
	if z.Stage.Kind != "own_belts" {
		t.Fatalf("stage = %q, want own_belts", z.Stage.Kind)
	}
	if z.People.OpenSeats != 4 {
		t.Fatalf("open seats = %d, want 4", z.People.OpenSeats)
	}
}

// ── Board ─────────────────────────────────────────────────────────────────

func TestZonesBoardModes(t *testing.T) {
	cases := []struct {
		name string
		edit func(*zonesInput)
		want string
	}{
		{"a crew of one", func(in *zonesInput) { in.H2h = zCrew(1) }, "solo"},
		{"nothing for 20 days", func(in *zonesInput) {
			in.Clips = []StoredClip{zClip("c", clipTypeVoice, 20, false)}
		}, "past"},
		{"played 3 days ago, nothing new", func(in *zonesInput) {
			in.Cards = []feedCard{zGameSession("s", 3, "Sara")}
		}, "quiet"},
		{"a stream in 24 h", func(in *zonesInput) {
			in.Cards = []feedCard{zGameSession("s", 3, "Sara")}
			in.Streams = []StoredStreamSession{zStream("live-ish", 0.2, 4)}
		}, "tabs"},
		{"a recap this week", func(in *zonesInput) {
			in.Cards = []feedCard{zGameSession("s", 3, "Sara")}
			in.Recaps = []WeeklyRecapData{{WeekStart: weekStartOf(zonesNow) - 7*dayMs, TotalMin: 600, SessionCount: 4, GeneratedAt: zonesNow - 3*dayMs}}
		}, "tabs"},
	}
	for _, c := range cases {
		in := zInput()
		c.edit(&in)
		if got := buildFeedZones(in).Board.Mode; got != c.want {
			t.Errorf("%s: mode = %q, want %q", c.name, got, c.want)
		}
	}
}

func TestZonesBoardNamesWhoPlayedLastWeek(t *testing.T) {
	in := zInput()
	in.Cards = []feedCard{zGameSession("a", 1, "Sara", "Max"), zGameSession("b", 2, "Max", "Leo"), zGameSession("old", 9, "Ida")}
	got := buildFeedZones(in).Board.PlayersLastWeek
	if strings.Join(got, ",") != "Sara,Max,Leo" {
		t.Fatalf("players = %v, want Sara, Max, Leo", got)
	}
}

func TestZonesBoardRecapIsNotHistory(t *testing.T) {
	in := zInput()
	last := weekStartOf(zonesNow) - 7*dayMs
	in.Recaps = []WeeklyRecapData{
		{WeekStart: last, TotalMin: 600, SessionCount: 4, GeneratedAt: zonesNow - 3*dayMs, WeekNumber: 44},
		{WeekStart: last - 7*dayMs, TotalMin: 300, SessionCount: 2, GeneratedAt: zonesNow - 10*dayMs, WeekNumber: 43},
	}
	z := buildFeedZones(in)
	if z.Board.Recap == nil || z.Board.Recap.ID != recapEntryID(last) {
		t.Fatalf("board recap = %+v, want the newest", z.Board.Recap)
	}
	if len(z.History) != 1 || z.History[0].WeekNumber != 43 {
		t.Fatalf("history = %+v, want week 43 only", z.History)
	}
}

// ── Sessions ──────────────────────────────────────────────────────────────

func TestZonesSessionsAreEveryGameAndVoiceSessionNewestFirst(t *testing.T) {
	in := zInput()
	in.Cards = []feedCard{
		zGameSession("g-old", 4),
		{id: "voice", feedType: "session", backendType: "voice_session", ts: zonesNow - dayMs},
		zGameSession("g-new", 0.5),
		zGameSession("g-out", 9),
		clipCard("clip", zonesNow-dayMs),
	}
	got := entryIDs(buildFeedZones(in).Sessions)
	if strings.Join(got, ",") != "g-new,voice,g-old" {
		t.Fatalf("sessions = %v, want g-new, voice, g-old", got)
	}
}

// ── History and the gate ──────────────────────────────────────────────────

func TestZonesLockedHistoryHoldsNoMediaURL(t *testing.T) {
	in := zInput()
	in.Clips = []StoredClip{zClip("old-game", clipTypeGame, 12, true), zClip("old-voice", clipTypeVoice, 13, false)}
	in.Streams = []StoredStreamSession{zStream("old-stream", 12, 8)}
	in.Recaps = []WeeklyRecapData{{WeekStart: weekStartOf(zonesNow - 12*dayMs), TotalMin: 600, SessionCount: 3, WeekNumber: 44}}
	z := buildFeedZones(in)
	if len(z.History) == 0 {
		t.Fatal("no history")
	}
	for _, w := range z.History {
		if !w.Locked || w.Recap != nil || len(w.Items) != 0 {
			t.Fatalf("a free viewer's week is locked with no recap and no items: %+v", w)
		}
	}
	raw, err := json.Marshal(z.History)
	if err != nil {
		t.Fatal(err)
	}
	for _, leak := range []string{"media.example", "media_url", "poster_url", "snapshot"} {
		if strings.Contains(string(raw), leak) {
			t.Fatalf("locked history leaks %q: %s", leak, raw)
		}
	}
	if got := z.History[0].Teasers; len(got) != 3 || got[0].Kind != "clip" {
		t.Fatalf("teasers = %+v, want 3, game clip first", got)
	}
}

func TestZonesMelloPlusOpensHistory(t *testing.T) {
	in := zInput()
	in.Plus = true
	in.Clips = []StoredClip{zClip("old-game", clipTypeGame, 12, true)}
	in.Recaps = []WeeklyRecapData{
		// The newest recap is on the board; the older one is history.
		{WeekStart: weekStartOf(zonesNow) - 7*dayMs, TotalMin: 900, SessionCount: 5},
		{WeekStart: weekStartOf(zonesNow - 12*dayMs), TotalMin: 600, SessionCount: 3},
	}
	z := buildFeedZones(in)
	w := z.History[0]
	if w.Locked || w.Recap == nil || len(w.Items) != 1 || len(w.Teasers) != 0 {
		t.Fatalf("m3llo+ sees the week open: %+v", w)
	}
	if c, ok := w.Items[0].Data.(StoredClip); !ok || c.MediaURL == "" {
		t.Fatalf("an open week carries the media: %+v", w.Items[0])
	}
}

func TestZonesHistoryPagesByWeek(t *testing.T) {
	in := zInput()
	for i := 0; i < 6; i++ {
		in.Clips = append(in.Clips, zClip(formatTestID(i), clipTypeVoice, float64(10+7*i), false))
	}
	page1 := buildFeedZones(in)
	if len(page1.History) != zonesHistoryPageWeeks || page1.HistoryNext == 0 {
		t.Fatalf("page 1: %d weeks, next %d", len(page1.History), page1.HistoryNext)
	}
	in.HistoryBefore = page1.HistoryNext
	page2 := buildFeedZones(in)
	if len(page2.History) != 2 || page2.HistoryNext != 0 {
		t.Fatalf("page 2: %d weeks, next %d, want 2 and the end", len(page2.History), page2.HistoryNext)
	}
	if page2.History[0].WeekStart >= page1.History[len(page1.History)-1].WeekStart {
		t.Fatal("page 2 starts before the last week of page 1")
	}
}

func formatTestID(i int) string {
	return "clip-" + string(rune('a'+i))
}

func TestZonesWeekStartsOnMondayUTC(t *testing.T) {
	// Monday 2 November 2026, 00:00 UTC.
	const monday int64 = 1_793_577_600_000
	if got := weekStartOf(zonesNow); got != monday {
		t.Fatalf("week of Thursday = %d, want %d", got, monday)
	}
	if got := weekStartOf(monday); got != monday {
		t.Fatalf("week of Monday = %d, want itself", got)
	}
	if got := weekStartOf(monday - 1); got != monday-7*dayMs {
		t.Fatalf("Sunday night belongs to the week before, got %d", got)
	}
}

// ── Feed config ───────────────────────────────────────────────────────────

func TestFeedConfigOutOfRangeFallsBackToTheDefault(t *testing.T) {
	for _, raw := range []string{`{"free_days":0}`, `{"free_days":500}`, `not json`} {
		if got := parseFeedConfig(raw).FreeDays; got != feedFreeDaysDefault {
			t.Errorf("parse %q = %d, want the default", raw, got)
		}
	}
	if got := parseFeedConfig(`{"free_days":14}`).FreeDays; got != 14 {
		t.Errorf("parse 14 = %d", got)
	}
}

func TestFeedConfigWriteNeedsAnActorAndARange(t *testing.T) {
	if _, err := normalizeFeedConfigWrite(AdminFeedConfigWrite{FreeDays: 7}); err == nil {
		t.Error("a write without an actor is refused")
	}
	if _, err := normalizeFeedConfigWrite(AdminFeedConfigWrite{FreeDays: 91, Actor: "bob"}); err == nil {
		t.Error("91 days is refused")
	}
	if actor, err := normalizeFeedConfigWrite(AdminFeedConfigWrite{FreeDays: 30, Actor: " bob "}); err != nil || actor != "bob" {
		t.Errorf("a good write: actor %q err %v", actor, err)
	}
}
