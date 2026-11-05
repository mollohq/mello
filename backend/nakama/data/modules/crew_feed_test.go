package main

import (
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

// ---- game clip hero (CLIPS.md 15.6) ----

// typedClipCard is a clip card that carries its StoredClip, as
// buildMergedTimeline makes it.
func typedClipCard(id, clipType string, ts int64, poster string) feedCard {
	return feedCard{id: id, feedType: "clip", backendType: "clip", ts: ts,
		data: StoredClip{EventID: id, ClipID: id, ClipType: clipType, Ts: ts, PosterURL: poster}}
}

func beltMovedCard(now int64) feedCard {
	return feedCard{id: head2headFeedID, feedType: head2headFeedID, ts: now - 1_000,
		data: &Head2headFeedData{BeltMovedRecent: &Head2headTitle{MovedAtMs: now - 1_000}}}
}
