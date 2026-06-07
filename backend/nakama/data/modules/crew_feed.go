package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"sort"

	"github.com/heroiclabs/nakama-common/runtime"
)

// ---------------------------------------------------------------------------
// Tunables (the curation experimentation surface)
//
// Changing these weights or what counts as quiet is a server config edit and a
// redeploy, no client release. Order + role + section + size are the only knobs
// that cross the boundary; layout, styling, and copy stay client-side.
// ---------------------------------------------------------------------------

const (
	feedFillerSlots    = 7 // grid slots after hero + recap (hero + recap + 7 = up to 9)
	feedWideFillerIdx  = 5 // index within the fillers promoted to the wide (lg) cell
	feedMemoryPageSize = 20

	// feedMinQuality mirrors the Rust i32::MIN/2 sentinel for non-preview cards.
	feedMinQuality = -(1 << 30)
)

// feedQuietBackendTypes are the low-signal pulse events rendered as quiet rows
// (ports isQuietRow from Timeline.swift plus the desktop priority). Moments and
// clips are full cards and are intentionally absent here.
var feedQuietBackendTypes = map[string]bool{
	"voice_session": true,
	"game_session":  true,
	"member_joined": true,
	"member_left":   true,
	"chat_activity": true,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

type FeedRequest struct {
	CrewID string `json:"crew_id"`
}

type FeedEntry struct {
	ID   string      `json:"id"`
	Type string      `json:"type"` // clip | recap | session-preview | session | catchup
	Role string      `json:"role"` // hero | standard | quiet | recap | locked
	Size string      `json:"size"` // sm | md | lg
	Ts   int64       `json:"ts"`
	Data interface{} `json:"data"`
}

type FeedSection struct {
	ID      string      `json:"id"` // this_week | memory
	Entries []FeedEntry `json:"entries"`
}

type FeedResponse struct {
	CrewID   string        `json:"crew_id"`
	Sections []FeedSection `json:"sections"`
}

// feedCard is the internal working representation used by the pure curation
// helpers so ordering stays testable without a Nakama module.
type feedCard struct {
	id          string
	feedType    string
	backendType string
	ts          int64
	snapshotN   int
	durationMin int
	data        interface{}
}

// ---------------------------------------------------------------------------
// Card type mapping (ported from map_card_type in client/src/handlers/clip.rs)
// ---------------------------------------------------------------------------

// mapCardType maps a backend event type to a feed card type. ok is false for
// unknown types, which are skipped (not coerced to catchup as the client did).
func mapCardType(backendType string, hasSnapshots bool) (string, bool) {
	switch backendType {
	case "clip":
		return "clip", true
	case "weekly_recap":
		return "recap", true
	case "stream_session":
		if hasSnapshots {
			return "session-preview", true
		}
		return "session", true
	case "voice_session", "game_session":
		return "session", true
	case "moment", "member_joined", "member_left", "chat_activity":
		return "catchup", true
	default:
		return "", false
	}
}

// mergedToCards maps merged-timeline entries to feed cards, skipping unknown
// types. stream_session entries are decoded for snapshot count and duration.
func mergedToCards(entries []TimelineEntry) []feedCard {
	cards := make([]feedCard, 0, len(entries))
	for _, e := range entries {
		snapshotN := 0
		durationMin := 0
		if e.Type == "stream_session" {
			if d, err := decodeStreamSessionData(e.Data); err == nil {
				snapshotN = len(d.SnapshotURLs)
				durationMin = d.DurationMin
			}
		}
		feedType, ok := mapCardType(e.Type, snapshotN > 0)
		if !ok {
			continue
		}
		cards = append(cards, feedCard{
			id:          e.ID,
			feedType:    feedType,
			backendType: e.Type,
			ts:          e.Ts,
			snapshotN:   snapshotN,
			durationMin: durationMin,
			data:        e.Data,
		})
	}
	return cards
}

// ---------------------------------------------------------------------------
// Quality + ordering (ported from client/src/feed_layout.rs)
// ---------------------------------------------------------------------------

// sessionPreviewQuality scores session-preview cards. Higher = more deserving
// of hero / wide slots. Short streams with only a couple of snapshots are
// heavily deprioritized. Non-preview cards return the sentinel.
func sessionPreviewQuality(c feedCard) int {
	if c.feedType != "session-preview" {
		return feedMinQuality
	}
	snapshotN := c.snapshotN
	dur := c.durationMin

	if dur <= 2 && snapshotN <= 4 {
		return -10000 + snapshotN
	}

	score := dur*10 + snapshotN*3
	if dur >= 15 {
		score += 40
	}
	if snapshotN >= 8 {
		score += 30
	}
	return score
}

func fillerPriority(c feedCard) int {
	switch c.feedType {
	case "clip":
		return 10000
	case "session-preview":
		return sessionPreviewQuality(c)
	case "session":
		return 100
	case "catchup":
		return 10
	default:
		return 0
	}
}

// feedBestIndex returns the unused card of feedType with the highest score.
// Ties resolve to the last such card, matching Rust's max_by_key. -1 if none.
func feedBestIndex(cards []feedCard, used []bool, feedType string, score func(feedCard) int) int {
	best := -1
	for i, c := range cards {
		if used[i] || c.feedType != feedType {
			continue
		}
		if best == -1 || score(c) >= score(cards[best]) {
			best = i
		}
	}
	return best
}

func feedFirstIndexOfType(cards []feedCard, used []bool, feedType string) int {
	for i, c := range cards {
		if !used[i] && c.feedType == feedType {
			return i
		}
	}
	return -1
}

// pickFillerIndices picks up to feedFillerSlots indices: one of each present
// type first (diversity), then by filler priority desc. Mutates used.
func pickFillerIndices(cards []feedCard, used []bool) []int {
	picks := make([]int, 0, feedFillerSlots)

	for _, ft := range []string{"clip", "session", "session-preview", "catchup"} {
		if len(picks) >= feedFillerSlots {
			break
		}
		idx := -1
		if ft == "session-preview" {
			idx = feedBestIndex(cards, used, "session-preview", sessionPreviewQuality)
		} else {
			idx = feedFirstIndexOfType(cards, used, ft)
		}
		if idx >= 0 {
			picks = append(picks, idx)
			used[idx] = true
		}
	}

	remaining := make([]int, 0)
	for i := range cards {
		if !used[i] {
			remaining = append(remaining, i)
		}
	}
	sort.SliceStable(remaining, func(a, b int) bool {
		return fillerPriority(cards[remaining[a]]) > fillerPriority(cards[remaining[b]])
	})

	for _, i := range remaining {
		if len(picks) >= feedFillerSlots {
			break
		}
		picks = append(picks, i)
		used[i] = true
	}

	return picks
}

// promoteWideSlot swaps the best visual filler (preview by quality, else a clip)
// into the wide cell position. Clips beat weak previews, lose to strong ones.
func promoteWideSlot(cards []feedCard, picks []int) {
	if len(picks) <= feedWideFillerIdx {
		return
	}
	from := -1
	bestScore := 0
	for pos, i := range picks {
		t := cards[i].feedType
		if t != "session-preview" && t != "clip" {
			continue
		}
		s := 5000 // clips beat weak previews for wide, lose to strong previews
		if t == "session-preview" {
			s = sessionPreviewQuality(cards[i])
		}
		if from == -1 || s >= bestScore {
			from = pos
			bestScore = s
		}
	}
	if from >= 0 {
		picks[from], picks[feedWideFillerIdx] = picks[feedWideFillerIdx], picks[from]
	}
}

// ---------------------------------------------------------------------------
// this_week assembly
// ---------------------------------------------------------------------------

func feedEntryFromCard(c feedCard, role, size string) FeedEntry {
	return FeedEntry{ID: c.id, Type: c.feedType, Role: role, Size: size, Ts: c.ts, Data: c.data}
}

// fillerRole assigns standard vs quiet. Quiet: the low-signal pulse types and
// short/weak session-previews. Everything else (clips, strong previews,
// moments) is standard.
func fillerRole(c feedCard) string {
	if feedQuietBackendTypes[c.backendType] {
		return "quiet"
	}
	if c.feedType == "session-preview" && sessionPreviewQuality(c) < 0 {
		return "quiet"
	}
	return "standard"
}

// buildThisWeek curates the recent feed: hero (best session-preview), the
// latest recap, then diversity+priority fillers capped at feedFillerSlots with
// the wide-slot promotion. Pure given its inputs.
//
// A live stream is not surfaced here yet; the live-stream hero is owned by the
// separate multi-stream PR. Clients keep showing live streams from crew_state.
func buildThisWeek(cards []feedCard) []FeedEntry {
	used := make([]bool, len(cards))
	entries := make([]FeedEntry, 0, feedFillerSlots+2)

	if hi := feedBestIndex(cards, used, "session-preview", sessionPreviewQuality); hi >= 0 {
		used[hi] = true
		entries = append(entries, feedEntryFromCard(cards[hi], "hero", "lg"))
	}

	if ri := feedFirstIndexOfType(cards, used, "recap"); ri >= 0 {
		used[ri] = true
		entries = append(entries, feedEntryFromCard(cards[ri], "recap", "md"))
	}

	fillers := pickFillerIndices(cards, used)
	promoteWideSlot(cards, fillers)
	for pos, i := range fillers {
		role := fillerRole(cards[i])
		size := "md"
		if role == "quiet" {
			size = "sm"
		}
		if pos == feedWideFillerIdx {
			size = "lg" // the wide grid cell, ported from the desktop promotion
		}
		entries = append(entries, feedEntryFromCard(cards[i], role, size))
	}

	return entries
}

// ---------------------------------------------------------------------------
// RPC
// ---------------------------------------------------------------------------

func CrewFeedRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req FeedRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	// this_week: curated recent feed.
	cards := mergedToCards(buildMergedTimeline(ctx, nk, req.CrewID))
	thisWeek := buildThisWeek(cards)

	// Items shown in this_week are excluded from the memory spine.
	shownClipIDs := map[string]bool{}
	shownSessionIDs := map[string]bool{}
	shownRecapWeekStart := int64(-1)
	for _, e := range thisWeek {
		switch e.Type {
		case "clip":
			shownClipIDs[e.ID] = true
		case "session", "session-preview":
			shownSessionIDs[e.ID] = true
		case "recap":
			fmt.Sscanf(e.ID, "recap_%d", &shownRecapWeekStart)
		}
	}

	memory := buildMemorySection(ctx, nk, req.CrewID, shownRecapWeekStart, shownClipIDs, shownSessionIDs)
	// The locked card is the m3llo+ upsell pinned at the end of memory. Gating is
	// not enforced yet, so IsUserPremium is always false and everyone sees it.
	memory = appendLockedCard(memory, !IsUserPremium(userID))

	updateLastSeen(ctx, nk, userID, req.CrewID)

	resp := FeedResponse{
		CrewID: req.CrewID,
		Sections: []FeedSection{
			{ID: "this_week", Entries: thisWeek},
			{ID: "memory", Entries: memory},
		},
	}
	data, _ := json.Marshal(resp)
	return string(data), nil
}

// buildMemorySection assembles the durable spine: recaps (newest first),
// older clips, and stream replays, excluding whatever this_week already shows,
// sorted by timestamp descending and capped at feedMemoryPageSize.
func buildMemorySection(ctx context.Context, nk runtime.NakamaModule, crewID string, shownRecapWeekStart int64, shownClipIDs, shownSessionIDs map[string]bool) []FeedEntry {
	entries := make([]FeedEntry, 0, feedMemoryPageSize)

	recapsDoc, _ := readRecapsDoc(ctx, nk, crewID)
	for _, r := range recapsDoc.Recaps {
		if r.WeekStart == shownRecapWeekStart {
			continue
		}
		entries = append(entries, FeedEntry{
			ID:   fmt.Sprintf("recap_%d", r.WeekStart),
			Type: "recap",
			Role: "recap",
			Size: "md",
			Ts:   r.GeneratedAt,
			Data: r,
		})
	}

	clipsDoc, _ := readClipsDoc(ctx, nk, crewID)
	for _, c := range clipsDoc.Clips {
		if shownClipIDs[c.EventID] {
			continue
		}
		entries = append(entries, FeedEntry{
			ID:   c.EventID,
			Type: "clip",
			Role: "standard",
			Size: "md",
			Ts:   c.Ts,
			Data: c,
		})
	}

	// Stream replays are durable memories too: a session-preview when snapshots
	// were captured, otherwise a plain session card.
	streamDoc, _ := readStreamSessionsDoc(ctx, nk, crewID)
	for _, s := range streamDoc.Sessions {
		if shownSessionIDs[s.EventID] {
			continue
		}
		feedType := "session"
		if len(s.SnapshotURLs) > 0 {
			feedType = "session-preview"
		}
		entries = append(entries, FeedEntry{
			ID:   s.EventID,
			Type: feedType,
			Role: "standard",
			Size: "md",
			Ts:   s.Ts,
			Data: s,
		})
	}

	sort.SliceStable(entries, func(i, j int) bool {
		return entries[i].Ts > entries[j].Ts
	})

	if len(entries) > feedMemoryPageSize {
		entries = entries[:feedMemoryPageSize]
	}

	return entries
}

// appendLockedCard pins the m3llo+ upsell at the end of the memory spine when
// include is true. The card carries no data; clients own the upsell copy and
// visual. Kept pure so the placement is unit-testable.
func appendLockedCard(entries []FeedEntry, include bool) []FeedEntry {
	if !include {
		return entries
	}
	return append(entries, FeedEntry{
		ID:   "locked",
		Type: "locked",
		Role: "locked",
		Size: "md",
	})
}
