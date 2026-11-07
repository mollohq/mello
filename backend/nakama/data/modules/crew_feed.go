package main

import (
	"context"
	"database/sql"
	"encoding/json"

	"sort"
	"strings"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// The crew feed RPC and the parts of the ledger it reads. The response is
// the zone feed (crew_feed_zones.go): five fixed zones and Crew history.

const (
	// feedMinQuality mirrors the Rust i32::MIN/2 sentinel for non-preview cards.
	feedMinQuality = -(1 << 30)

	// feedHeroFreshMs is how young a belt move is to count as recent
	// (head2head belt_moved_recent).
	feedHeroFreshMs = 24 * 60 * 60 * 1000

	// Recent games tiles: the most games the installed list reads.
	feedRecentGamesMaxTiles = 6
)

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

type FeedRequest struct {
	CrewID string `json:"crew_id"`
	// HistoryBefore pages Crew history: the weeks that start before it. Zero
	// for the first page.
	HistoryBefore int64 `json:"history_before,omitempty"`
}

type FeedEntry struct {
	ID   string      `json:"id"`
	Type string      `json:"type"` // clip | recap | daily-recap | session-preview | session
	Ts   int64       `json:"ts"`
	Data interface{} `json:"data"`
}

// RecentGameTile is one game on the recent games card, keyed by game id.
// GameID falls back to the lower-cased name for legacy events without an id.
//
// Minutes is the lifetime playtime from the members' library documents when
// one has it, else the minutes of the sessions in the window. LastPlayed is Unix
// milliseconds. MemberIDs are the members who have the game installed or
// played it in the window. Launcher is the launcher prefix of the library
// entry ("steam", "epic", "gog"), absent for a game only seen in sessions. The
// client shows it when no playtime is known.
type RecentGameTile struct {
	GameID     string   `json:"game_id"`
	GameName   string   `json:"game_name"`
	Launcher   string   `json:"launcher,omitempty"`
	Minutes    int      `json:"minutes"`
	LastPlayed int64    `json:"last_played,omitempty"`
	MemberIDs  []string `json:"member_ids,omitempty"`
}

// RecentGamesData is the payload of the recent games card. SessionCount and
// TotalMin feed the header line ("14 sessions, 31 h this week") and count every
// game session in the window, not only the tiles shown. PCCount is the number
// of members who have an installed or played game on the card, counted over
// every game before the tiles are capped. The client draws a poster grid from
// two PCs on and the list for one ("From 2 PCs", CLIPS 7).
type RecentGamesData struct {
	Games        []RecentGameTile `json:"games"`
	SessionCount int              `json:"session_count"`
	TotalMin     int              `json:"total_min"`
	PCCount      int              `json:"pc_count"`
}

// FeedJoin is one member who joined the crew in the last 24 hours, for the
// joined line at the top of the feed. It comes from the member_joined events
// of the ledger. Names are the ones the event stored. InvitedBy is empty when
// the joiner used no invite code.
type FeedJoin struct {
	UserID        string `json:"user_id"`
	Name          string `json:"name"`
	InvitedBy     string `json:"invited_by,omitempty"`
	InvitedByName string `json:"invited_by_name,omitempty"`
	Ts            int64  `json:"ts"`
}

type FeedResponse struct {
	CrewID string    `json:"crew_id"`
	Zones  FeedZones `json:"zones"`
}

const (
	// feedJoinedWindowMs is how long a join shows on the joined line.
	feedJoinedWindowMs = 24 * 60 * 60 * 1000
	// feedJoinedMax is the most rows the joined line shows.
	feedJoinedMax = 3
)

// buildFeedJoins reads the member_joined events of the timeline: the joins of
// the last 24 hours by someone who is still a member, newest first, at most
// three, one row for each joiner. The viewer's own join is not shown to them.
// members is the set of current member ids. Pure, so the rule is testable.
func buildFeedJoins(entries []TimelineEntry, members map[string]bool, viewerID string, nowMs int64) []FeedJoin {
	joins := make([]FeedJoin, 0, feedJoinedMax)
	seen := map[string]bool{}
	sorted := append([]TimelineEntry(nil), entries...)
	sort.SliceStable(sorted, func(i, j int) bool { return sorted[i].Ts > sorted[j].Ts })
	for _, e := range sorted {
		if e.Type != "member_joined" || e.ActorID == "" || e.ActorID == viewerID {
			continue
		}
		if nowMs-e.Ts >= feedJoinedWindowMs || e.Ts > nowMs || !members[e.ActorID] || seen[e.ActorID] {
			continue
		}
		seen[e.ActorID] = true
		d, _ := decodeCardData[MemberJoinedData](e.Data)
		name := d.DisplayName
		if name == "" {
			name = d.Username
		}
		joins = append(joins, FeedJoin{
			UserID: e.ActorID, Name: name,
			InvitedBy: d.InvitedBy, InvitedByName: d.InvitedByName, Ts: e.Ts,
		})
		if len(joins) == feedJoinedMax {
			break
		}
	}
	return joins
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
// unknown types, which are skipped. The ledger events a catch-up card used to
// show (moment, member_joined, member_left, chat_activity) have no card: a join
// is the joined row, and a day of activity is a daily recap.
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
	launcher       string
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
			if g.launcher == "" {
				g.launcher = lg.Launcher
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

	owners := map[string]bool{}
	for _, g := range games {
		for m := range g.members {
			owners[m] = true
		}
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
			GameID: id, GameName: name, Launcher: g.launcher, Minutes: minutes,
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
		data:        RecentGamesData{Games: tiles, SessionCount: sessions, TotalMin: totalMin, PCCount: len(owners)},
	}, true
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

// cardHasContent is the one rule for whether a card may be emitted. Both
// sections apply it before they emit a card. feedType is the feed type,
// backendType the ledger type (stream_session for a plain stream session).
//
//   - recap: not empty (isEmptyRecap).
//   - session: needs reportable minutes. Zero-minute sessions never emit.
//   - session-preview: needs at least one snapshot.
//   - recent-games: needs a tile with minutes or with a member who has it.
//   - daily-recap: needs minutes or a clip, and a clip, a belt move or three
//     active members (dailyRecapHasContent).
//
// Every other type is its own content (a clip, a join, a lock). A payload that
// is absent or cannot be decoded cannot be judged, so the card is kept.
func cardHasContent(feedType, backendType string, data interface{}) bool {
	switch feedType {
	case "recap":
		r, ok := decodeCardData[WeeklyRecapData](data)
		return !ok || !isEmptyRecap(r)
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
	case "daily-recap":
		d, ok := decodeCardData[DailyRecapData](data)
		return !ok || dailyRecapHasContent(d)
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

// feedEntryFromCard is the wire form of a card in the zones.
func feedEntryFromCard(c feedCard) FeedEntry {
	return FeedEntry{ID: c.id, Type: c.feedType, Ts: c.ts, Data: c.data}
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

	timeline := buildMergedTimeline(ctx, nk, req.CrewID)
	nowMs := time.Now().UnixMilli()
	h2h := crewHead2headForViewer(ctx, logger, nk, req.CrewID, userID, nowMs)
	joins := feedJoinsFor(ctx, nk, req.CrewID, timeline, userID, nowMs)
	zones := crewFeedZones(ctx, logger, nk, req, userID, timeline, h2h, joins, nowMs)

	// The first page is a visit to the feed; a history page is not.
	if req.HistoryBefore <= 0 {
		updateLastSeen(ctx, nk, userID, req.CrewID)
	}

	data, err := json.Marshal(FeedResponse{CrewID: req.CrewID, Zones: zones})
	if err != nil {
		return "", runtime.NewError("encode failed", 13)
	}
	return string(data), nil
}

// crewFeedZones reads the stores the zone feed needs and builds it. The gate
// is per viewer: IsUserPremium decides if Crew history is open.
func crewFeedZones(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, req FeedRequest, userID string, timeline []TimelineEntry, h2h *Head2headFeedData, joins []FeedJoin, nowMs int64) FeedZones {
	cfg := readFeedConfig(ctx, logger, nk)
	clipsDoc, _ := readClipsDoc(ctx, nk, req.CrewID)
	streamDoc, _ := readStreamSessionsDoc(ctx, nk, req.CrewID)
	recapsDoc, _ := readRecapsDoc(ctx, nk, req.CrewID)
	stampRecapWeeks(ctx, nk, req.CrewID, recapsDoc.Recaps)
	quests, _ := readCrewQuests(ctx, nk, req.CrewID)
	return buildFeedZones(zonesInput{
		ViewerID:      userID,
		NowMs:         nowMs,
		FreeDays:      cfg.FreeDays,
		Plus:          IsUserPremium(userID),
		HistoryBefore: req.HistoryBefore,
		Cards:         dropEmptyCards(mergedToCards(timeline)),
		Clips:         clipsDoc.Clips,
		Streams:       streamDoc.Sessions,
		Recaps:        recapsDoc.Recaps,
		H2h:           h2h,
		Libs:          crewLibraryDocs(ctx, nk, req.CrewID),
		Joins:         joins,
		Belts:         crewBeltsMovedSince(ctx, nk, req.CrewID, nowMs-int64(feedDailyRecapDays)*dayMs),
		Quests:        quests,
	})
}

// crewBeltsMovedSince reads the holders doc of the crew and returns the belts
// that moved since sinceMs, for the daily recaps. A crew with no doc has none.
func crewBeltsMovedSince(ctx context.Context, nk runtime.NakamaModule, crewID string, sinceMs int64) []RecapBeltMoved {
	doc, _ := readHead2headDoc(ctx, nk, crewID)
	if doc == nil {
		return nil
	}
	names := map[string]string{}
	name := func(id string) string {
		if id == "" {
			return ""
		}
		if n, ok := names[id]; ok {
			return n
		}
		n := resolveOrID(ctx, nk, id)
		names[id] = n
		return n
	}
	moved, _ := buildRecapBelts(doc, sinceMs, time.Now().UnixMilli(), name)
	return moved
}

// feedJoinsFor is buildFeedJoins with the member set of the crew. A failure to
// list the members gives no joined line.
func feedJoinsFor(ctx context.Context, nk runtime.NakamaModule, crewID string, timeline []TimelineEntry, viewerID string, nowMs int64) []FeedJoin {
	ids, err := crewMemberIDs(ctx, nk, crewID)
	if err != nil {
		return nil
	}
	members := make(map[string]bool, len(ids))
	for _, id := range ids {
		members[id] = true
	}
	return buildFeedJoins(timeline, members, viewerID, nowMs)
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
