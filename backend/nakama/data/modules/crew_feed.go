package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"sort"
	"strings"
	"time"

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
	feedMemoryPageSize = 60

	// feedMinQuality mirrors the Rust i32::MIN/2 sentinel for non-preview cards.
	feedMinQuality = -(1 << 30)

	// Game-session budget (spec 19 + game-surfacing B0): adaptive floor and cap
	// scale with how many game_session cards are in the input week. Quiet crews
	// may surface compact T0 cards for routine play; loud crews keep only the
	// most notable sessions. The weekly recap remains the durable aggregate.
	feedGameSessionMaxCards   = 2  // default cap when input has >5 game sessions
	feedGameSessionNotableMin = 50 // default floor when input has >15 game sessions

	// Hero priority (plans/next-gen-feed.md section 2): a stream preview, or a
	// moved belt, leads only while it is this young.
	feedHeroFreshMs = 24 * 60 * 60 * 1000

	// Recent games card: tiles shown. The header counts every session.
	feedRecentGamesMaxTiles = 6
)

// feedQuietBackendTypes are the low-signal pulse events rendered as quiet rows
// (ports isQuietRow from Timeline.swift plus the desktop priority). Moments and
// clips are full cards and are intentionally absent here.
// game_session is not in this map: T0-only sessions render compact (quiet);
// telemetry-backed sessions that survive pruning use standard size (spec 19 B2).
var feedQuietBackendTypes = map[string]bool{
	"voice_session": true,
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
	Type string      `json:"type"` // clip | recap | session-preview | session | catchup | recent-games
	Role string      `json:"role"` // hero | standard | quiet | recap | locked
	Size string      `json:"size"` // sm | md | lg
	Ts   int64       `json:"ts"`
	Data interface{} `json:"data"`
}

// RecentGameTile is one game on the recent games card, keyed by game id.
// GameID falls back to the lower-cased name for legacy events without an id.
//
// Minutes is the lifetime playtime from the members' library documents when
// one has it, else the minutes of the sessions in the window. LastPlayed is Unix
// milliseconds. MemberIDs are the members who have the game installed or
// played it in the window.
type RecentGameTile struct {
	GameID     string   `json:"game_id"`
	GameName   string   `json:"game_name"`
	Minutes    int      `json:"minutes"`
	LastPlayed int64    `json:"last_played,omitempty"`
	MemberIDs  []string `json:"member_ids,omitempty"`
}

// RecentGamesData is the payload of the recent games card. SessionCount and
// TotalMin feed the header line ("14 sessions, 31 h this week") and count every
// game session in the window, not only the tiles shown.
type RecentGamesData struct {
	Games        []RecentGameTile `json:"games"`
	SessionCount int              `json:"session_count"`
	TotalMin     int              `json:"total_min"`
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

func decodeGameSessionData(raw interface{}) (GameSessionData, bool) {
	b, err := json.Marshal(raw)
	if err != nil {
		return GameSessionData{}, false
	}
	var d GameSessionData
	if err := json.Unmarshal(b, &d); err != nil {
		return GameSessionData{}, false
	}
	return d, true
}

// gameSessionT0Score awards duration and co-play signals that need no telemetry.
func gameSessionT0Score(d GameSessionData) int {
	score := 0
	switch {
	case d.DurationMin >= 240:
		score += 40
	case d.DurationMin >= 120:
		score += 25
	case d.DurationMin >= 30:
		score += 10
	}
	coBonus := 0
	if n := len(d.PlayerIDs); n > 0 {
		coBonus = (n - 1) * 20
		if coBonus > 60 {
			coBonus = 60
		}
	}
	return score + coBonus
}

// gameSessionNotableFloor returns the quality floor for game_session pruning
// based on how many game_session cards are in the input week.
func gameSessionNotableFloor(gameSessionCount int) int {
	switch {
	case gameSessionCount <= 5:
		return 10
	case gameSessionCount <= 15:
		return 30
	default:
		return feedGameSessionNotableMin
	}
}

// gameSessionCardCap returns how many game_session cards may survive pruning.
func gameSessionCardCap(gameSessionCount int) int {
	if gameSessionCount <= 5 {
		return 4
	}
	return feedGameSessionMaxCards
}

// gameSessionQuality scores a game_session card's notability (spec 19 + B0).
// Every session gets a T0 base; telemetry bonuses stack on top when present.
func gameSessionQuality(c feedCard) int {
	if c.backendType != "game_session" {
		return feedMinQuality
	}
	d, ok := decodeGameSessionData(c.data)
	if !ok {
		return 0
	}
	score := gameSessionT0Score(d)
	matches := d.Wins + d.Losses + d.Draws
	if matches == 0 {
		return score
	}
	streak := d.StreakAfter
	if streak < 0 {
		streak = -streak
	}
	switch {
	case streak >= 5:
		score += 120
	case streak >= 3:
		score += 80
	}
	if d.Wins >= 3 && d.Losses == 0 {
		score += 70 // flawless night
	}
	if d.Losses >= 5 && d.Wins == 0 {
		score += 50 // rough night (sympathy card)
	}
	switch {
	case matches >= 8:
		score += 50
	case matches >= 5:
		score += 20
	}
	return score
}

// pruneGameSessions drops low-scoring game_session cards and keeps only the
// most notable ones (above an adaptive floor, capped by volume). Non-game cards
// pass through untouched and in order. Successfully decoded pruned sessions are
// returned separately. Pure, so the budget is testable.
func pruneGameSessions(cards []feedCard) (remaining []feedCard, pruned []feedCard) {
	gameSessionCount := 0
	for _, c := range cards {
		if c.backendType == "game_session" {
			gameSessionCount++
		}
	}
	floor := gameSessionNotableFloor(gameSessionCount)
	cap := gameSessionCardCap(gameSessionCount)

	type scored struct {
		card feedCard
		q    int
	}
	out := make([]feedCard, 0, len(cards))
	notable := make([]scored, 0)
	for _, c := range cards {
		if c.backendType != "game_session" {
			out = append(out, c)
			continue
		}
		if q := gameSessionQuality(c); q >= floor {
			notable = append(notable, scored{c, q})
		}
	}
	sort.SliceStable(notable, func(a, b int) bool { return notable[a].q > notable[b].q })
	if len(notable) > cap {
		notable = notable[:cap]
	}
	kept := make(map[string]bool, len(notable))
	for _, s := range notable {
		kept[s.card.id] = true
		out = append(out, s.card)
	}
	for _, c := range cards {
		if c.backendType != "game_session" || kept[c.id] {
			continue
		}
		if _, ok := decodeGameSessionData(c.data); ok {
			pruned = append(pruned, c)
		}
	}
	return out, pruned
}

// reportableMinutes picks the play figure a surface may claim, applying the
// overnight guard from plans/GAME-SURFACING.md §4: wall time normally, active
// time when wall time is inflated far past it.
//
// A game left open overnight has honest wall time and near-zero active time.
// Claiming the wall figure is the one number a crewmate can trivially disprove,
// and the whole "proof it happened" pitch rests on not doing that. The client
// applies the same rule per session (game_session_duration_display in
// clip.rs) and per week (weekly_time_text in stats.rs) — keep the three in
// step, or the recent games card will contradict the cards below it.
func reportableMinutes(durationMin, activeMin int) int {
	if activeMin > 0 && activeMin*3 < durationMin {
		return activeMin
	}
	return durationMin
}

// feedRecentGamesMaxPips caps the member ids on one tile.
const feedRecentGamesMaxPips = 12

// recentGameAgg collects what the sessions and the library documents say
// about one game.
type recentGameAgg struct {
	id, name       string
	sessionMin     int
	sessionTs      int64
	libraryMin     int
	libraryTs      int64 // Unix milliseconds
	libraryName    string
	libraryID      string
	members        map[string]bool
	hasLibraryMins bool
}

// buildRecentGames builds the recent games card from the game sessions in the
// window and the library documents of the crew members (libs, keyed by user
// id; nil is fine). It replaces the Crew play rollup: the session count and
// total time stay the card header and come from sessions only. The tiles are
// the union of both sources, keyed by game id. A library game whose id matches
// no session falls back to a match on the lower-cased name, because sessions
// carry catalogue ids and the library carries launcher ids (spec 17 section
// 3.3).
//
//   - name: the library name when present, else the session name;
//   - minutes: the members' summed playtime when present, else session minutes;
//   - last played: the newer of the two sources;
//   - members: who has the game installed or played it.
//
// Sessions with no reportable minutes are not counted. Returns false when
// there is nothing to show, so the card never emits empty.
func buildRecentGames(cards []feedCard, libs map[string]*LibraryDoc) (feedCard, bool) {
	games := make(map[string]*recentGameAgg)
	byName := make(map[string]string) // lower-cased name -> games key
	var maxTs int64
	sessions, totalMin := 0, 0

	register := func(key, name string, g *recentGameAgg) {
		games[key] = g
		if n := strings.ToLower(strings.TrimSpace(name)); n != "" {
			if _, taken := byName[n]; !taken {
				byName[n] = key
			}
		}
	}

	for _, c := range cards {
		if c.backendType != "game_session" {
			continue
		}
		d, ok := decodeGameSessionData(c.data)
		if !ok {
			continue
		}
		mins := reportableMinutes(d.DurationMin, d.ActiveMin)
		if mins <= 0 {
			continue
		}
		key := d.GameID
		if key == "" {
			key = strings.ToLower(strings.TrimSpace(d.GameName))
		}
		if key == "" {
			continue
		}
		if c.ts > maxTs {
			maxTs = c.ts
		}
		sessions++
		totalMin += mins
		g, exists := games[key]
		if !exists {
			g = &recentGameAgg{id: d.GameID, name: d.GameName, members: map[string]bool{}}
			register(key, d.GameName, g)
		}
		if g.name == "" {
			g.name = d.GameName
		}
		g.sessionMin += mins
		if c.ts > g.sessionTs {
			g.sessionTs = c.ts
		}
		for _, id := range d.PlayerIDs {
			if id != "" {
				g.members[id] = true
			}
		}
	}

	// Library documents, in user id order so the result does not depend on
	// map iteration.
	userIDs := make([]string, 0, len(libs))
	for id := range libs {
		userIDs = append(userIDs, id)
	}
	sort.Strings(userIDs)
	var maxLibraryUpdate int64
	for _, userID := range userIDs {
		doc := libs[userID]
		if doc == nil {
			continue
		}
		if doc.UpdatedAt > maxLibraryUpdate {
			maxLibraryUpdate = doc.UpdatedAt
		}
		for _, lg := range doc.Games {
			if lg.GameID == "" {
				continue
			}
			key := lg.GameID
			g, exists := games[key]
			if !exists {
				if k, ok := byName[strings.ToLower(strings.TrimSpace(lg.Name))]; ok {
					key, g, exists = k, games[k], true
				}
			}
			if !exists {
				g = &recentGameAgg{id: lg.GameID, name: lg.Name, members: map[string]bool{}}
				register(key, lg.Name, g)
			}
			if g.libraryName == "" {
				g.libraryName = lg.Name
			}
			if g.libraryID == "" {
				g.libraryID = lg.GameID
			}
			g.members[userID] = true
			if lg.PlaytimeMin != nil && *lg.PlaytimeMin > 0 {
				g.libraryMin += *lg.PlaytimeMin
				g.hasLibraryMins = true
			}
			if lg.LastPlayedUnix != nil && *lg.LastPlayedUnix > 0 {
				if ms := *lg.LastPlayedUnix * 1000; ms > g.libraryTs {
					g.libraryTs = ms
				}
			}
		}
	}
	if len(games) == 0 {
		return feedCard{}, false
	}

	tiles := make([]RecentGameTile, 0, len(games))
	for key, g := range games {
		id := g.id
		if id == "" {
			id = g.libraryID
		}
		if id == "" {
			id = key
		}
		name := g.libraryName
		if name == "" {
			name = g.name
		}
		minutes := g.sessionMin
		if g.hasLibraryMins {
			minutes = g.libraryMin
		}
		lastPlayed := g.sessionTs
		if g.libraryTs > lastPlayed {
			lastPlayed = g.libraryTs
		}
		members := make([]string, 0, len(g.members))
		for m := range g.members {
			members = append(members, m)
		}
		sort.Strings(members)
		if len(members) > feedRecentGamesMaxPips {
			members = members[:feedRecentGamesMaxPips]
		}
		tiles = append(tiles, RecentGameTile{
			GameID: id, GameName: name, Minutes: minutes,
			LastPlayed: lastPlayed, MemberIDs: members,
		})
	}
	sort.Slice(tiles, func(i, j int) bool {
		if tiles[i].LastPlayed != tiles[j].LastPlayed {
			return tiles[i].LastPlayed > tiles[j].LastPlayed
		}
		if tiles[i].Minutes != tiles[j].Minutes {
			return tiles[i].Minutes > tiles[j].Minutes
		}
		return tiles[i].GameID < tiles[j].GameID
	})
	if len(tiles) > feedRecentGamesMaxTiles {
		tiles = tiles[:feedRecentGamesMaxTiles]
	}

	ts := maxTs
	for _, t := range tiles {
		if t.LastPlayed > ts {
			ts = t.LastPlayed
		}
	}
	if ts == 0 {
		ts = maxLibraryUpdate
	}
	return feedCard{
		id:          "recent_games",
		feedType:    "recent-games",
		backendType: "recent_games",
		ts:          ts,
		data:        RecentGamesData{Games: tiles, SessionCount: sessions, TotalMin: totalMin},
	}, true
}

func fillerPriority(c feedCard) int {
	switch c.feedType {
	case "clip":
		return 10000
	case "session-preview":
		return sessionPreviewQuality(c)
	case "session":
		if c.backendType == "game_session" {
			q := gameSessionQuality(c)
			if q < 0 {
				q = 0
			}
			if q > 100 {
				q = 100
			}
			return 100 + q
		}
		return 100
	case "catchup":
		return 10
	case "recent-games":
		return 150
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

// promoteWideSlot swaps the best session-preview filler into the wide cell
// position. A voice clip never takes the wide cell (plans/next-gen-feed.md
// section 2), so clips are not candidates here.
func promoteWideSlot(cards []feedCard, picks []int) {
	if len(picks) <= feedWideFillerIdx {
		return
	}
	from := -1
	bestScore := 0
	for pos, i := range picks {
		if cards[i].feedType != "session-preview" {
			continue
		}
		s := sessionPreviewQuality(cards[i])
		if from == -1 || s >= bestScore {
			from = pos
			bestScore = s
		}
	}
	if from >= 0 {
		picks[from], picks[feedWideFillerIdx] = picks[feedWideFillerIdx], picks[from]
	}
	keepClipOutOfWideSlot(cards, picks)
}

// keepClipOutOfWideSlot moves a clip off the wide cell position by swapping it
// with the nearest non-clip filler, later positions first. If every filler is a
// clip there is nothing to swap with and the clip stays, capped at the standard
// size by buildThisWeek.
func keepClipOutOfWideSlot(cards []feedCard, picks []int) {
	if len(picks) <= feedWideFillerIdx || cards[picks[feedWideFillerIdx]].feedType != "clip" {
		return
	}
	for step := 1; step < len(picks); step++ {
		for _, pos := range []int{feedWideFillerIdx + step, feedWideFillerIdx - step} {
			if pos < 0 || pos >= len(picks) || cards[picks[pos]].feedType == "clip" {
				continue
			}
			picks[pos], picks[feedWideFillerIdx] = picks[feedWideFillerIdx], picks[pos]
			return
		}
	}
}

// ---------------------------------------------------------------------------
// Empty-card rule (plans/next-gen-feed.md section 2, principle 1)
// ---------------------------------------------------------------------------

// decodeCardData decodes a card payload that is a typed struct or a generic
// JSON map. ok is false for a missing or undecodable payload.
func decodeCardData[T any](raw interface{}) (T, bool) {
	var d T
	if raw == nil {
		return d, false
	}
	b, err := json.Marshal(raw)
	if err != nil {
		return d, false
	}
	if err := json.Unmarshal(b, &d); err != nil {
		return d, false
	}
	return d, true
}

// recapHasContent reports whether a recap shows anything: hangout minutes,
// clips, or game results. A recap with none of these is the "0m" card.
// fix/recap-real-activity adds isEmptyRecap with game minutes; once that is
// merged, replace this body with !isEmptyRecap(r).
func recapHasContent(r WeeklyRecapData) bool {
	return r.TotalHangoutMin > 0 || r.ClipCount > 0 ||
		len(r.GamesPlayed) > 0 || len(r.GameRecords) > 0
}

// cardHasContent is the one rule for whether a card may be emitted. Both
// sections apply it before they emit a card. feedType is the feed type,
// backendType the ledger type (stream_session for a plain stream session).
//
//   - recap: needs content (recapHasContent).
//   - session: needs reportable minutes. Zero-minute sessions never emit.
//   - session-preview: needs at least one snapshot.
//   - recent-games: needs a tile with minutes or with a member who has it.
//
// Every other type is its own content (a clip, a join, a lock). A payload that
// is absent or cannot be decoded cannot be judged, so the card is kept.
func cardHasContent(feedType, backendType string, data interface{}) bool {
	switch feedType {
	case "recap":
		r, ok := decodeCardData[WeeklyRecapData](data)
		return !ok || recapHasContent(r)
	case "session-preview":
		d, ok := decodeCardData[StreamSessionData](data)
		if !ok {
			return true
		}
		for _, u := range d.SnapshotURLs {
			if u != "" {
				return true
			}
		}
		return false
	case "recent-games":
		d, ok := decodeCardData[RecentGamesData](data)
		if !ok {
			return true
		}
		for _, g := range d.Games {
			if g.Minutes > 0 || len(g.MemberIDs) > 0 {
				return true
			}
		}
		return false
	case "session":
		switch backendType {
		case "game_session":
			d, ok := decodeCardData[GameSessionData](data)
			return !ok || reportableMinutes(d.DurationMin, d.ActiveMin) > 0
		case "voice_session":
			d, ok := decodeCardData[VoiceSessionData](data)
			return !ok || d.DurationMin > 0
		case "stream_session":
			d, ok := decodeCardData[StreamSessionData](data)
			return !ok || d.DurationMin > 0
		}
	}
	return true
}

// dropEmptyCards removes the cards cardHasContent rejects. Order is kept.
func dropEmptyCards(cards []feedCard) []feedCard {
	out := make([]feedCard, 0, len(cards))
	for _, c := range cards {
		if cardHasContent(c.feedType, c.backendType, c.data) {
			out = append(out, c)
		}
	}
	return out
}

// appendEntryIfContent appends e to a section unless it has no content.
func appendEntryIfContent(entries []FeedEntry, e FeedEntry, backendType string) []FeedEntry {
	if !cardHasContent(e.Type, backendType, e.Data) {
		return entries
	}
	return append(entries, e)
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
	if c.feedType == "recent-games" {
		return "standard"
	}
	if feedQuietBackendTypes[c.backendType] {
		return "quiet"
	}
	if c.backendType == "game_session" {
		if d, ok := decodeGameSessionData(c.data); ok {
			if d.Wins+d.Losses+d.Draws == 0 {
				return "quiet"
			}
		}
	}
	if c.feedType == "session-preview" && sessionPreviewQuality(c) < 0 {
		return "quiet"
	}
	return "standard"
}

// Hero priority, plans/next-gen-feed.md section 2:
//
//  1. a live stream. The feed builder cannot see live streams: they are client
//     state from crew_state. The client shows a live stream above this_week, so
//     this list starts at the next item.
//  2. a stream preview (session-preview) from the last 24 h
//  3. a belt that moved in the last 24 h (not available yet, see beltMovedHeroIndex)
//  4. a real recap, on Monday
//  5. the best session
//
// A voice clip is never the hero.

// freshPreviewHeroIndex returns the best session-preview younger than
// feedHeroFreshMs, or -1.
func freshPreviewHeroIndex(cards []feedCard, nowMs int64) int {
	best := -1
	for i, c := range cards {
		if c.feedType != "session-preview" || nowMs-c.ts > feedHeroFreshMs {
			continue
		}
		if best == -1 || sessionPreviewQuality(c) >= sessionPreviewQuality(cards[best]) {
			best = i
		}
	}
	return best
}

// beltMovedHeroIndex is the slot for "a belt moved in the last 24 h". Belts
// need the per-user stats doc (plan phase 5), so no card carries one yet. It
// returns -1 until then; the hero order already calls it in the right place.
func beltMovedHeroIndex(cards []feedCard, nowMs int64) int {
	return -1
}

// mondayRecapHeroIndex returns the recap to lead with on a Monday (UTC), the
// day the weekly job runs, while that recap is under 24 h old. Empty recaps
// never reach here: buildThisWeek drops them first. Otherwise -1.
func mondayRecapHeroIndex(cards []feedCard, nowMs int64) int {
	if time.UnixMilli(nowMs).UTC().Weekday() != time.Monday {
		return -1
	}
	best := -1
	for i, c := range cards {
		if c.feedType != "recap" || nowMs-c.ts > feedHeroFreshMs {
			continue
		}
		if best == -1 || c.ts > cards[best].ts {
			best = i
		}
	}
	return best
}

// bestSessionHeroIndex returns the strongest session-preview of any age, or -1.
func bestSessionHeroIndex(cards []feedCard) int {
	used := make([]bool, len(cards))
	return feedBestIndex(cards, used, "session-preview", sessionPreviewQuality)
}

// heroIndex applies the hero priority above. Pure, so the order is testable.
func heroIndex(cards []feedCard, nowMs int64) int {
	if i := freshPreviewHeroIndex(cards, nowMs); i >= 0 {
		return i
	}
	if i := beltMovedHeroIndex(cards, nowMs); i >= 0 {
		return i
	}
	if i := mondayRecapHeroIndex(cards, nowMs); i >= 0 {
		return i
	}
	return bestSessionHeroIndex(cards)
}

// buildThisWeek curates the recent feed: the hero (see heroIndex), the latest
// recap, then diversity+priority fillers capped at feedFillerSlots with the
// wide-slot promotion. Clips are standard cards: never the hero, never wide.
// Pure given its inputs.
//
// Live streams are not surfaced here. They are client state (crew_state), so
// the client shows them first and this list starts at the next hero choice.
func buildThisWeek(cards []feedCard, nowMs int64) []FeedEntry {
	return buildThisWeekWithLibraries(cards, nowMs, nil)
}

// buildThisWeekWithLibraries is buildThisWeek with the crew members' library
// documents (by user id) feeding the recent games card.
func buildThisWeekWithLibraries(cards []feedCard, nowMs int64, libs map[string]*LibraryDoc) []FeedEntry {
	// Empty-card rule first, so an empty card never takes a slot or a count.
	cards = dropEmptyCards(cards)

	// The recent games card counts every game session in the window, before
	// pruning, so its header matches the week and not only the pruned part.
	recent, hasRecent := buildRecentGames(cards, libs)

	// Budget game sessions first: adaptive floor/cap decides which earn a card.
	// Pruned sessions are not carded; the recent games card carries their time.
	cards, _ = pruneGameSessions(cards)
	if hasRecent {
		cards = append(cards, recent)
	}

	used := make([]bool, len(cards))
	entries := make([]FeedEntry, 0, feedFillerSlots+2)

	if hi := heroIndex(cards, nowMs); hi >= 0 {
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
		if pos == feedWideFillerIdx && cards[i].feedType != "clip" {
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
	thisWeek := buildThisWeekWithLibraries(cards, time.Now().UnixMilli(), crewLibraryDocs(ctx, nk, req.CrewID))

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

// crewLibraryDocs reads the library documents of the crew members as the
// system. The crew list is capped at 100 members, as in isCrewMember. A
// failure gives no documents, and the card falls back to sessions.
func crewLibraryDocs(ctx context.Context, nk runtime.NakamaModule, crewID string) map[string]*LibraryDoc {
	members, _, err := nk.GroupUsersList(ctx, crewID, 100, nil, "")
	if err != nil {
		return nil
	}
	ids := make([]string, 0, len(members))
	for _, m := range members {
		if id := m.GetUser().GetId(); id != "" {
			ids = append(ids, id)
		}
	}
	return readLibraryDocs(ctx, nk, ids)
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
		entries = appendEntryIfContent(entries, FeedEntry{
			ID:   fmt.Sprintf("recap_%d", r.WeekStart),
			Type: "recap",
			Role: "recap",
			Size: "md",
			Ts:   r.GeneratedAt,
			Data: r,
		}, "weekly_recap")
	}

	clipsDoc, _ := readClipsDoc(ctx, nk, crewID)
	for _, c := range clipsDoc.Clips {
		if shownClipIDs[c.EventID] {
			continue
		}
		entries = appendEntryIfContent(entries, FeedEntry{
			ID:   c.EventID,
			Type: "clip",
			Role: "standard",
			Size: "md",
			Ts:   c.Ts,
			Data: c,
		}, "clip")
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
		entries = appendEntryIfContent(entries, FeedEntry{
			ID:   s.EventID,
			Type: feedType,
			Role: "standard",
			Size: "md",
			Ts:   s.Ts,
			Data: s,
		}, "stream_session")
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
