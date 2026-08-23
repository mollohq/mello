package main

import (
	"fmt"
	"testing"
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
		typedCard("catch", "catchup", "chat_activity"),
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
	if counts["catchup"] == 0 {
		t.Fatal("expected a catchup")
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

func TestTextlessMomentIsStandard(t *testing.T) {
	cards := []feedCard{typedCard("m", "catchup", "moment")}
	entries := buildThisWeek(cards, feedTestNowMs)
	e := findEntry(entries, "m")
	if e == nil {
		t.Fatal("moment entry missing")
	}
	if e.Role != "standard" {
		t.Fatalf("text-less moment role: got %q want standard", e.Role)
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

// The single best non-hero visual card is promoted to the wide (lg) grid cell.
// With no session-preview there is no hero, so the wide filler is the only lg.
func TestWideSlotGetsLargeSize(t *testing.T) {
	cards := make([]feedCard, 0, 6)
	for i := 0; i < 6; i++ {
		cards = append(cards, typedCard("clip"+string(rune('0'+i)), "clip", "clip"))
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

func TestFreshClipIsHero(t *testing.T) {
	now := feedTestNowMs
	cards := []feedCard{
		clipCard("fresh", now-1_000),
		previewCard("long", 21, 50),
		typedCardAt("voice", "session", "voice_session", now-500),
	}
	entries := buildThisWeek(cards, now)
	if entries[0].ID != "fresh" || entries[0].Role != "hero" || entries[0].Size != "lg" {
		t.Fatalf("hero: got id=%q role=%q size=%q want fresh/hero/lg", entries[0].ID, entries[0].Role, entries[0].Size)
	}
	for _, e := range entries {
		if e.Role == "hero" && e.Type == "session-preview" {
			t.Fatalf("session-preview should not be hero when a fresh clip qualifies: %q", e.ID)
		}
	}
	preview := findEntry(entries, "long")
	if preview == nil {
		t.Fatal("strong preview missing from fillers")
	}
	if preview.Role == "hero" {
		t.Fatalf("preview role: got %q want non-hero filler", preview.Role)
	}
}

func TestStaleClipFallsBackToPreviewHero(t *testing.T) {
	now := feedTestNowMs
	cards := []feedCard{
		clipCard("stale", now-feedClipHeroMaxAgeMs-1),
		previewCard("long", 21, 50),
	}
	entries := buildThisWeek(cards, now)
	if entries[0].ID != "long" || entries[0].Role != "hero" {
		t.Fatalf("hero: got id=%q role=%q want long/hero", entries[0].ID, entries[0].Role)
	}
	clip := findEntry(entries, "stale")
	if clip == nil {
		t.Fatal("stale clip missing from fillers")
	}
	if clip.Role != "standard" {
		t.Fatalf("stale clip role: got %q want standard", clip.Role)
	}
}

func TestBuriedClipNotHero(t *testing.T) {
	now := feedTestNowMs
	clipTs := now - 1_000
	cards := []feedCard{
		clipCard("buried", clipTs),
		typedCardAt("n1", "catchup", "chat_activity", clipTs+1),
		typedCardAt("n2", "catchup", "moment", clipTs+2),
		typedCardAt("n3", "session", "voice_session", clipTs+3),
		typedCardAt("n4", "catchup", "member_joined", clipTs+4),
		previewCard("long", 21, 50),
	}
	entries := buildThisWeek(cards, now)
	if entries[0].ID != "long" || entries[0].Role != "hero" {
		t.Fatalf("hero: got id=%q role=%q want long/hero", entries[0].ID, entries[0].Role)
	}
	clip := findEntry(entries, "buried")
	if clip == nil {
		t.Fatal("buried clip missing from fillers")
	}
	if clip.Role == "hero" {
		t.Fatal("buried clip should not be hero")
	}
}

func TestNoClipsKeepsPreviewHero(t *testing.T) {
	now := feedTestNowMs
	cards := []feedCard{
		typedCard("recap", "recap", "weekly_recap"),
		previewCard("long", 21, 50),
		typedCard("voice", "session", "voice_session"),
	}
	entries := buildThisWeek(cards, now)
	if entries[0].ID != "long" || entries[0].Role != "hero" {
		t.Fatalf("hero: got id=%q role=%q want long/hero", entries[0].ID, entries[0].Role)
	}
}

func TestFreshClipBeatsStrongPreview(t *testing.T) {
	now := feedTestNowMs
	cards := []feedCard{
		clipCard("fresh", now-500),
		previewCard("long", 21, 50),
	}
	entries := buildThisWeek(cards, now)
	if entries[0].ID != "fresh" || entries[0].Type != "clip" {
		t.Fatalf("hero: got id=%q type=%q want fresh/clip", entries[0].ID, entries[0].Type)
	}
	if findEntry(entries, "long") == nil {
		t.Fatal("strong preview should still appear among fillers")
	}
}

func TestFreshClipHeroBoundaries(t *testing.T) {
	now := feedTestNowMs
	clipTs := now - feedClipHeroMaxAgeMs

	// Exactly max age → still premieres.
	atAge := buildThisWeek([]feedCard{clipCard("edge-age", clipTs)}, now)
	if atAge[0].Role != "hero" {
		t.Fatalf("clip at max age should hero, got role=%q", atAge[0].Role)
	}

	// One millisecond older → demoted.
	overAge := buildThisWeek([]feedCard{clipCard("over-age", clipTs-1)}, now)
	if overAge[0].Role == "hero" && overAge[0].Type == "clip" {
		t.Fatal("clip past max age should not hero")
	}

	// Exactly max-newer - 1 newer cards → still premieres.
	almostBuried := []feedCard{clipCard("almost", clipTs+1_000)}
	for i := 0; i < feedClipHeroMaxNewer-1; i++ {
		almostBuried = append(almostBuried, typedCardAt(
			fmt.Sprintf("n%d", i), "catchup", "chat_activity", clipTs+1_000+int64(i+1),
		))
	}
	abEntries := buildThisWeek(almostBuried, now)
	if abEntries[0].ID != "almost" || abEntries[0].Role != "hero" {
		t.Fatalf("clip with %d newer cards should hero, got id=%q role=%q",
			feedClipHeroMaxNewer-1, abEntries[0].ID, abEntries[0].Role)
	}

	// Exactly max-newer newer cards → buried.
	buried := []feedCard{clipCard("exact-buried", clipTs+1_000)}
	for i := 0; i < feedClipHeroMaxNewer; i++ {
		buried = append(buried, typedCardAt(
			fmt.Sprintf("b%d", i), "catchup", "chat_activity", clipTs+1_000+int64(i+1),
		))
	}
	bEntries := buildThisWeek(buried, now)
	if bEntries[0].Role == "hero" && bEntries[0].Type == "clip" {
		t.Fatal("clip with exactly max-newer newer cards should not hero")
	}
}
