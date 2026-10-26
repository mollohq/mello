package main

import (
	"fmt"
	"testing"
	"time"
)

const feedTestNowMs int64 = 1_700_000_000_000

func clipCard(id string, ts int64) feedCard {
	return feedCard{id: id, feedType: "clip", backendType: "clip", ts: ts}
}

func previewCard(id string, durationMin, snapshots int) feedCard {
	return feedCard{
		id:          id,
		feedType:    "session-preview",
		backendType: "stream_session",
		snapshotN:   snapshots,
		durationMin: durationMin,
	}
}

func typedCard(id, feedType, backendType string) feedCard {
	return feedCard{id: id, feedType: feedType, backendType: backendType}
}

func typedCardAt(id, feedType, backendType string, ts int64) feedCard {
	return feedCard{id: id, feedType: feedType, backendType: backendType, ts: ts}
}

func findEntry(entries []FeedEntry, id string) *FeedEntry {
	for i := range entries {
		if entries[i].ID == id {
			return &entries[i]
		}
	}
	return nil
}

// Ported from feed_layout.rs::weak_short_preview_scores_below_strong
func TestWeakShortPreviewScoresBelowStrong(t *testing.T) {
	weak := previewCard("short", 1, 2)
	strong := previewCard("long", 21, 20)
	if sessionPreviewQuality(strong) <= sessionPreviewQuality(weak) {
		t.Fatalf("strong %d should exceed weak %d", sessionPreviewQuality(strong), sessionPreviewQuality(weak))
	}
}

// Ported from feed_layout.rs::hero_prefers_strong_session_preview_over_clip
func TestHeroPrefersStrongSessionPreviewOverClip(t *testing.T) {
	cards := []feedCard{
		typedCard("recap", "recap", "weekly_recap"),
		typedCard("clip", "clip", "clip"),
		previewCard("short", 1, 2),
		typedCard("voice", "session", "voice_session"),
		previewCard("long", 21, 50),
	}
	entries := buildThisWeek(cards, feedTestNowMs)
	if entries[0].Type != "session-preview" {
		t.Fatalf("hero type: got %q want session-preview", entries[0].Type)
	}
	if entries[0].ID != "long" {
		t.Fatalf("hero id: got %q want long", entries[0].ID)
	}
	if entries[0].Role != "hero" {
		t.Fatalf("hero role: got %q want hero", entries[0].Role)
	}
}

// Ported from feed_layout.rs::includes_one_of_each_type_when_present
func TestIncludesOneOfEachTypeWhenPresent(t *testing.T) {
	cards := []feedCard{
		typedCard("recap", "recap", "weekly_recap"),
		typedCard("game", "session", "game_session"),
		typedCard("voice", "session", "voice_session"),
		previewCard("p1", 1, 2),
		typedCard("clip", "clip", "clip"),
		previewCard("p2", 21, 30),
	}
	entries := buildThisWeek(cards, feedTestNowMs)

	counts := map[string]int{}
	for _, e := range entries {
		counts[e.Type]++
	}
	if counts["recap"] == 0 {
		t.Fatal("expected a recap")
	}
	if counts["clip"] == 0 {
		t.Fatal("expected a clip")
	}
	if counts["session"] == 0 {
		t.Fatal("expected a session")
	}
	if counts["session-preview"] < 2 {
		t.Fatalf("expected >=2 session-preview, got %d", counts["session-preview"])
	}
	if counts["catchup"] != 0 {
		t.Fatal("the catch-up card is gone")
	}
}

// Ported from feed_layout.rs::long_preview_survives_noise_sessions
func TestLongPreviewSurvivesNoiseSessions(t *testing.T) {
	cards := []feedCard{
		typedCard("recap", "recap", "weekly_recap"),
		typedCard("g70", "session", "game_session"),
		typedCard("v4", "session", "voice_session"),
		typedCard("v49", "session", "voice_session"),
		previewCard("short", 1, 2),
		previewCard("long", 21, 40),
	}
	entries := buildThisWeek(cards, feedTestNowMs)
	if entries[0].ID != "long" {
		t.Fatalf("hero id: got %q want long", entries[0].ID)
	}
	if findEntry(entries, "long") == nil {
		t.Fatal("long preview missing from output")
	}
}

// The live-stream hero is deferred to the multi-stream PR, so the best
// session-preview always leads this_week.
func TestBestPreviewIsHeroWithoutLiveStream(t *testing.T) {
	cards := []feedCard{
		typedCard("recap", "recap", "weekly_recap"),
		previewCard("long", 21, 50),
		typedCard("clip", "clip", "clip"),
	}
	entries := buildThisWeek(cards, feedTestNowMs)
	if entries[0].ID != "long" || entries[0].Role != "hero" {
		t.Fatalf("hero: got id=%q role=%q want long/hero", entries[0].ID, entries[0].Role)
	}
}

func TestAppendLockedCard(t *testing.T) {
	base := []FeedEntry{{ID: "clip", Type: "clip", Role: "standard", Size: "md"}}

	if got := appendLockedCard(base, false); len(got) != 1 {
		t.Fatalf("premium user should not get a locked card, got %d entries", len(got))
	}

	got := appendLockedCard(base, true)
	if len(got) != 2 {
		t.Fatalf("expected locked card appended, got %d entries", len(got))
	}
	last := got[len(got)-1]
	if last.Role != "locked" || last.Type != "locked" {
		t.Fatalf("locked card: got role=%q type=%q want locked/locked", last.Role, last.Type)
	}
}

func TestUnknownBackendTypeSkipped(t *testing.T) {
	entries := mergedToCards([]TimelineEntry{
		{ID: "x", Type: "some_unknown", Ts: 1},
		{ID: "c", Type: "clip", Ts: 2},
	})
	if len(entries) != 1 {
		t.Fatalf("expected 1 card after skipping unknown, got %d", len(entries))
	}
	if entries[0].id != "c" {
		t.Fatalf("expected clip card, got %q", entries[0].id)
	}
}

// The ledger events a catch-up card showed have no card now: a join is the
// joined row, and a day of activity is a daily recap.
func TestCatchupEventsHaveNoCard(t *testing.T) {
	for _, typ := range []string{"moment", "member_joined", "member_left", "chat_activity"} {
		if ft, ok := mapCardType(typ, false); ok {
			t.Errorf("%s maps to %q, want no card", typ, ft)
		}
	}
	cards := mergedToCards([]TimelineEntry{
		{ID: "m", Type: "moment"}, {ID: "j", Type: "member_joined"}, {ID: "c", Type: "chat_activity"},
	})
	if len(cards) != 0 {
		t.Fatalf("want no cards, got %d", len(cards))
	}
}

// stream_session maps to session-preview only when it carries snapshots; without
// them it is a plain session. This is the split clients used to compute themselves.
func TestStreamSessionMappingBySnapshots(t *testing.T) {
	cards := mergedToCards([]TimelineEntry{
		{ID: "withSnaps", Type: "stream_session", Data: map[string]interface{}{
			"snapshot_urls": []string{"a", "b"}, "duration_min": 10,
		}},
		{ID: "noSnaps", Type: "stream_session", Data: map[string]interface{}{
			"duration_min": 10,
		}},
	})
	if len(cards) != 2 {
		t.Fatalf("expected 2 cards, got %d", len(cards))
	}
	if cards[0].feedType != "session-preview" {
		t.Fatalf("with snapshots: got %q want session-preview", cards[0].feedType)
	}
	if cards[0].snapshotN != 2 {
		t.Fatalf("snapshot count: got %d want 2", cards[0].snapshotN)
	}
	if cards[1].feedType != "session" {
		t.Fatalf("without snapshots: got %q want session", cards[1].feedType)
	}
}

// Low-signal pulse types collapse to a compact quiet row; living content stays
// standard. This is the iOS quiet-row rule moved server-side.
func TestLowSignalFillersAreQuiet(t *testing.T) {
	cards := []feedCard{
		typedCard("clip", "clip", "clip"),
		typedCard("voice", "session", "voice_session"),
	}
	entries := buildThisWeek(cards, feedTestNowMs)

	voice := findEntry(entries, "voice")
	if voice == nil {
		t.Fatal("voice entry missing")
	}
	if voice.Role != "quiet" || voice.Size != "sm" {
		t.Fatalf("voice session: got role=%q size=%q want quiet/sm", voice.Role, voice.Size)
	}
	clip := findEntry(entries, "clip")
	if clip == nil {
		t.Fatal("clip entry missing")
	}
	if clip.Role != "standard" {
		t.Fatalf("clip role: got %q want standard", clip.Role)
	}
}

// The wide (lg) grid cell holds one card. With no session-preview there is no
// hero, so the wide filler is the only lg. Clips never take it (TestClipNeverWide).
func TestWideSlotGetsLargeSize(t *testing.T) {
	cards := make([]feedCard, 0, 6)
	for i := 0; i < 6; i++ {
		cards = append(cards, typedCard("voice"+string(rune('0'+i)), "session", "voice_session"))
	}
	entries := buildThisWeek(cards, feedTestNowMs)

	large := 0
	for _, e := range entries {
		if e.Size == "lg" {
			large++
		}
		if e.Role == "hero" {
			t.Fatalf("unexpected hero %q without a session-preview", e.ID)
		}
	}
	if large != 1 {
		t.Fatalf("expected exactly one lg (wide) filler, got %d", large)
	}
}

func freshPreview(id string, now int64, ageMs int64) feedCard {
	c := previewCard(id, 21, 50)
	c.ts = now - ageMs
	return c
}

func recapAt(id string, ts int64) feedCard {
	return feedCard{id: id, feedType: "recap", backendType: "weekly_recap", ts: ts}
}

// A Monday 12:00 UTC and a Thursday 12:00 UTC, for the recap-on-Monday rule.
var (
	feedTestMonday   = time.Date(2026, time.October, 19, 12, 0, 0, 0, time.UTC).UnixMilli()
	feedTestThursday = time.Date(2026, time.October, 22, 12, 0, 0, 0, time.UTC).UnixMilli()
)

// A voice clip is never the hero, however fresh, and a stream preview that is
// older than the clip still leads.
func TestFreshClipIsNeverHero(t *testing.T) {
	now := feedTestNowMs
	cards := []feedCard{
		clipCard("fresh", now-1_000),
		previewCard("long", 21, 50),
		typedCardAt("voice", "session", "voice_session", now-500),
	}
	entries := buildThisWeek(cards, now)
	if entries[0].ID != "long" || entries[0].Role != "hero" {
		t.Fatalf("hero: got id=%q role=%q want long/hero", entries[0].ID, entries[0].Role)
	}
	clip := findEntry(entries, "fresh")
	if clip == nil || clip.Role == "hero" {
		t.Fatalf("fresh clip must be a non-hero filler, got %+v", clip)
	}
}

func TestClipOnlyFeedHasNoHero(t *testing.T) {
	now := feedTestNowMs
	entries := buildThisWeek([]feedCard{clipCard("fresh", now-500)}, now)
	for _, e := range entries {
		if e.Role == "hero" || e.Size == "lg" {
			t.Fatalf("clip %q got role=%q size=%q, want standard md", e.ID, e.Role, e.Size)
		}
	}
	clips := 0
	for _, e := range entries {
		if e.Type == "clip" {
			clips++
		}
	}
	if clips != 1 {
		t.Fatalf("want the clip emitted once, got %d clip entries", clips)
	}
	// The clip makes a day, so the day has its daily recap ahead of the fillers.
	if len(entries) != 2 || entries[0].Type != "daily-recap" || entries[1].Type != "clip" {
		t.Fatalf("want the daily recap then the clip, got %+v", entries)
	}
}

// With fillers of several kinds, a clip never lands on the wide cell and never
// carries the lg size, even when it would otherwise be picked for that slot.
func TestClipNeverWide(t *testing.T) {
	now := feedTestNowMs
	cards := []feedCard{}
	for i := 0; i < 8; i++ {
		cards = append(cards, clipCard(fmt.Sprintf("clip%d", i), now-int64(i+1)))
	}
	cards = append(cards, typedCardAt("voice", "session", "voice_session", now-100))
	entries := buildThisWeek(cards, now)
	for _, e := range entries {
		if e.Type == "clip" && e.Size == "lg" {
			t.Fatalf("clip %q has lg size", e.ID)
		}
	}
}

// A weak preview does not outrank a non-clip for the wide cell, and a clip
// that would have been promoted is swapped off it.
func TestKeepClipOutOfWideSlot(t *testing.T) {
	cards := []feedCard{
		clipCard("c0", 1), clipCard("c1", 2), clipCard("c2", 3),
		clipCard("c3", 4), clipCard("c4", 5), clipCard("c5", 6),
		typedCard("v", "session", "voice_session"),
	}
	picks := []int{0, 1, 2, 3, 4, 5, 6}
	keepClipOutOfWideSlot(cards, picks)
	if cards[picks[feedWideFillerIdx]].feedType == "clip" {
		t.Fatalf("wide position still holds a clip: picks=%v", picks)
	}
	if cards[picks[feedWideFillerIdx]].id != "v" {
		t.Fatalf("want the voice session swapped in, got %q", cards[picks[feedWideFillerIdx]].id)
	}
}

func TestStaleClipIsStandard(t *testing.T) {
	now := feedTestNowMs
	cards := []feedCard{clipCard("stale", now-48*60*60*1000), previewCard("long", 21, 50)}
	entries := buildThisWeek(cards, now)
	clip := findEntry(entries, "stale")
	if clip == nil || clip.Role != "standard" || clip.Size != "md" {
		t.Fatalf("stale clip: got %+v want standard/md", clip)
	}
}

// Hero order: stream preview under 24 h beats a Monday recap.
func TestHeroFreshPreviewBeatsMondayRecap(t *testing.T) {
	now := feedTestMonday
	cards := []feedCard{
		recapAt("recap_1", now-3_600_000),
		freshPreview("fresh", now, 2*3_600_000),
	}
	h := buildThisWeek(cards, now)
	if h[0].ID != "fresh" || h[0].Role != "hero" {
		t.Fatalf("hero: got %q/%q want fresh/hero", h[0].ID, h[0].Role)
	}
}

// A preview older than 24 h loses to a real Monday recap, which leads on
// Monday only.
func TestHeroMondayRecapBeatsOldPreview(t *testing.T) {
	now := feedTestMonday
	cards := []feedCard{
		freshPreview("old", now, 30*3_600_000),
		recapAt("recap_1", now-3_600_000),
	}
	h := buildThisWeek(cards, now)
	if h[0].ID != "recap_1" || h[0].Role != "hero" || h[0].Size != "lg" {
		t.Fatalf("hero: got %q/%q/%q want recap_1/hero/lg", h[0].ID, h[0].Role, h[0].Size)
	}
	if findEntry(h, "old") == nil {
		t.Fatal("old preview should stay in the feed as a filler")
	}
	// Same cards on a Thursday: the recap is not the hero, the best session is.
	th := buildThisWeek([]feedCard{
		freshPreview("old", feedTestThursday, 30*3_600_000),
		recapAt("recap_1", feedTestThursday-3_600_000),
	}, feedTestThursday)
	if th[0].ID != "old" || th[0].Role != "hero" {
		t.Fatalf("thursday hero: got %q/%q want old/hero", th[0].ID, th[0].Role)
	}
	if r := findEntry(th, "recap_1"); r == nil || r.Role != "recap" {
		t.Fatalf("thursday recap: got %+v want role recap", r)
	}
}

// A recap older than 24 h does not lead on a Monday.
func TestHeroStaleRecapOnMondayIsNotHero(t *testing.T) {
	now := feedTestMonday
	cards := []feedCard{recapAt("recap_0", now-8*24*3_600_000)}
	for _, e := range buildThisWeek(cards, now) {
		if e.Role == "hero" {
			t.Fatalf("stale recap became hero: %q", e.ID)
		}
	}
}

// With no fresh preview and no recap, the best session leads, whatever its age.
func TestHeroBestSessionFallback(t *testing.T) {
	now := feedTestNowMs
	cards := []feedCard{
		freshPreview("weak", now, 40*3_600_000),
		freshPreview("best", now, 90*3_600_000),
	}
	cards[0] = previewCard("weak", 3, 6)
	cards[0].ts = now - 40*3_600_000
	h := buildThisWeek(cards, now)
	if h[0].ID != "best" || h[0].Role != "hero" {
		t.Fatalf("hero: got %q/%q want best/hero", h[0].ID, h[0].Role)
	}
}

// Among previews under 24 h, quality picks the hero, not recency.
func TestHeroFreshPreviewPicksBestQuality(t *testing.T) {
	now := feedTestNowMs
	weak := previewCard("weak", 3, 6)
	weak.ts = now - 1_000
	strong := freshPreview("strong", now, 20*3_600_000)
	h := buildThisWeek([]feedCard{weak, strong}, now)
	if h[0].ID != "strong" {
		t.Fatalf("hero: got %q want strong", h[0].ID)
	}
}

// The belt hook finds nothing when no head2head card is in the list.
func TestBeltMovedHeroHookNeedsAHead2headCard(t *testing.T) {
	if got := beltMovedHeroIndex([]feedCard{clipCard("c", 1)}, feedTestNowMs); got != -1 {
		t.Fatalf("belt hook: got %d want -1", got)
	}
}

func TestNoHeroWhenNothingQualifies(t *testing.T) {
	now := feedTestNowMs
	cards := []feedCard{clipCard("c", now-1), typedCard("v", "session", "voice_session")}
	for _, e := range buildThisWeek(cards, now) {
		if e.Role == "hero" {
			t.Fatalf("unexpected hero %q", e.ID)
		}
	}
}
