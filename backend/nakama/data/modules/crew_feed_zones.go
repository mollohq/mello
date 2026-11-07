package main

import (
	"fmt"
	"sort"
)

// The zone feed: the crew feed as five fixed
// zones (Stage, Board, Clips, People, Sessions) and Crew history, instead of a
// card list with roles and sizes.
//
// Free is the last FreeDays days (feed config, default 7). Everything older is
// Crew history and is m3llo+. The gate is per viewer: a viewer without m3llo+
// gets the weeks of history with counts and teasers, never a media, poster or
// snapshot URL. The media itself is on public URLs (S3PublicURL), so a URL a
// client already holds still works; this gate stops new ones from leaving the
// server.
//
// Live streams, live game sessions and voice channels are client state from
// crew_state. The client puts a live stream on the Stage ahead of anything
// here, and turns a quiet board back to tabs while something is live.

const (
	// zonesHistoryPageWeeks is the number of past weeks in one page of history.
	zonesHistoryPageWeeks = 4
	// zonesTeasersMax caps the teasers of one locked week; the counts carry
	// the rest.
	zonesTeasersMax = 6
	// zonesDormantDays without activity makes the board show the last week.
	zonesDormantDays = 14
	// zonesBeltHeroDays is how old a belt move involving the viewer can be and
	// still lead a quiet feed.
	zonesBeltHeroDays = 14
	// zonesInstalledMax is the most installed games on a quiet week.
	zonesInstalledMax = 3
)

// FeedZones is the zones payload.
type FeedZones struct {
	FreeDays  int                `json:"free_days"`
	Plus      bool               `json:"plus"`
	Stage     ZoneStage          `json:"stage"`
	Board     ZoneBoard          `json:"board"`
	Head2head *Head2headFeedData `json:"head2head,omitempty"`
	// Clips holds every clip of the free window: game clips first, then
	// newest first. The client puts the unread ones first.
	Clips    []FeedEntry `json:"clips"`
	People   ZonePeople  `json:"people"`
	Sessions []FeedEntry `json:"sessions"`
	// Installed is set on a quiet week with no session (the games most
	// members have) and for a crew of one (its own games, the solo board).
	Installed []RecentGameTile  `json:"installed,omitempty"`
	History   []ZoneHistoryWeek `json:"history"`
	// HistoryNext is the history_before of the next page. Zero at the end.
	HistoryNext int64 `json:"history_next,omitempty"`
	// Quests is the quest card of a new crew, while its window runs. The
	// client shows it ahead of Stage, as it does a live stream, and puts
	// Stage back when the card goes. While it is here, the Stage entry also
	// stays in Clips or Sessions, so it is not lost under the card.
	Quests *ZoneQuests `json:"quests,omitempty"`
}

// ZoneStage is the one thing to watch. Kind is game_clip, stream_replay,
// belt (the client builds the belt hero from head2head), own_belts (a crew of
// one: the client builds the sealed own chips) or empty.
type ZoneStage struct {
	Kind  string     `json:"kind"`
	Entry *FeedEntry `json:"entry,omitempty"`
}

// ZoneBoard is the crew in numbers. Mode is tabs, quiet, past (dormant: the
// last real week) or solo (a crew of one). The client shows the recap sealed
// in the board slot until the viewer opened it (SEALED-REVEAL.md).
type ZoneBoard struct {
	Mode  string     `json:"mode"`
	Recap *FeedEntry `json:"recap,omitempty"`
	// Today is the daily recap of the current UTC day, when the day has one.
	Today *FeedEntry `json:"today,omitempty"`
	// LastActivityMs is the newest session, clip or stream, for the quiet and
	// dormant lines. Zero when nothing is known.
	LastActivityMs int64 `json:"last_activity_ms,omitempty"`
	// PlayersLastWeek names who played a game in the last 7 days, newest
	// first: "Sara, Max and 2 more still played."
	PlayersLastWeek []string `json:"players_last_week,omitempty"`
}

// ZonePeople is who is here: the first five and the joins of 24 hours. Voice
// is client state.
type ZonePeople struct {
	MemberCount int        `json:"member_count"`
	OpenSeats   int        `json:"open_seats"`
	Joined      []FeedJoin `json:"joined,omitempty"`
}

// ZoneHistoryWeek is one past week of Crew history. Locked: the viewer has no
// m3llo+, so the week has counts and teasers only.
type ZoneHistoryWeek struct {
	WeekStart   int64        `json:"week_start"`
	WeekNumber  int          `json:"week_number,omitempty"`
	Locked      bool         `json:"locked"`
	ClipCount   int          `json:"clip_count"`
	StreamCount int          `json:"stream_count"`
	Recap       *FeedEntry   `json:"recap,omitempty"`
	Items       []FeedEntry  `json:"items,omitempty"`
	Teasers     []ZoneTeaser `json:"teasers,omitempty"`
}

// ZoneTeaser is one locked clip or stream: who, what kind, how long. It holds
// no URL by design.
type ZoneTeaser struct {
	Kind            string  `json:"kind"` // clip | voice | stream
	ActorID         string  `json:"actor_id"`
	ActorName       string  `json:"actor_name"`
	DurationSeconds float64 `json:"duration_seconds"`
	Ts              int64   `json:"ts"`
}

// zonesInput is everything the zones read. Pure in, pure out, so every rule is
// testable without Nakama.
type zonesInput struct {
	ViewerID string
	NowMs    int64
	FreeDays int
	Plus     bool
	// HistoryBefore asks for the history weeks that start before it. Zero for
	// the first page.
	HistoryBefore int64
	// Cards is the merged timeline as feed cards, empty ones dropped.
	Cards   []feedCard
	Clips   []StoredClip
	Streams []StoredStreamSession
	Recaps  []WeeklyRecapData
	H2h     *Head2headFeedData
	Libs    map[string]*LibraryDoc
	Joins   []FeedJoin
	Belts   []RecapBeltMoved
	// Quests is the quest record of the crew, nil when it has none.
	Quests *CrewQuests
}

func (in zonesInput) windowStart() int64 {
	days := in.FreeDays
	if days < feedFreeDaysMin || days > feedFreeDaysMax {
		days = feedFreeDaysDefault
	}
	return in.NowMs - int64(days)*dayMs
}

// weekStartOf is Monday 00:00 UTC of the week of ms.
func weekStartOf(ms int64) int64 {
	day := floorDiv(ms, dayMs)
	// 1970-01-01 was a Thursday: day 0 is weekday 3 when Monday is 0.
	weekday := ((day+3)%7 + 7) % 7
	return (day - weekday) * dayMs
}

func floorDiv(a, b int64) int64 {
	q := a / b
	if (a%b != 0) && ((a < 0) != (b < 0)) {
		q--
	}
	return q
}

func clipEntry(c StoredClip) FeedEntry {
	return FeedEntry{ID: c.EventID, Type: "clip", Ts: c.Ts, Data: c}
}

func streamEntry(s StoredStreamSession) FeedEntry {
	t := "session"
	if len(s.SnapshotURLs) > 0 {
		t = "session-preview"
	}
	return FeedEntry{ID: s.EventID, Type: t, Ts: s.Ts, Data: s}
}

func streamQuality(s StoredStreamSession) int {
	return sessionPreviewQuality(feedCard{feedType: "session-preview", snapshotN: len(s.SnapshotURLs), durationMin: s.DurationMin})
}

// rankClips orders clips game first, then newest first.
func rankClips(clips []StoredClip) {
	sort.SliceStable(clips, func(i, j int) bool {
		gi, gj := clips[i].ClipType == clipTypeGame, clips[j].ClipType == clipTypeGame
		if gi != gj {
			return gi
		}
		return clips[i].Ts > clips[j].Ts
	})
}

func buildFeedZones(in zonesInput) FeedZones {
	start := in.windowStart()
	z := FeedZones{
		FreeDays:  int((in.NowMs - start) / dayMs),
		Plus:      in.Plus,
		Head2head: in.H2h,
		Clips:     []FeedEntry{},
		Sessions:  []FeedEntry{},
		History:   []ZoneHistoryWeek{},
	}
	z.Stage = zoneStage(in, start)
	z.Quests = zoneQuests(in.Quests, in.NowMs)
	stageID := ""
	if z.Stage.Entry != nil && z.Quests == nil {
		stageID = z.Stage.Entry.ID
	}
	z.Clips = zoneClips(in, start, stageID)
	z.Board = zoneBoard(in)
	z.Sessions = zoneSessions(in, start, stageID)
	// A quiet week with no session shows what the crew has installed; a crew
	// of one shows its own games on the solo board.
	if (z.Board.Mode == "quiet" && len(z.Sessions) == 0) || z.Board.Mode == "solo" {
		z.Installed = zoneInstalled(in)
	}
	if in.H2h != nil {
		z.People = ZonePeople{MemberCount: in.H2h.MemberCount, OpenSeats: in.H2h.OpenSeats}
	}
	z.People.Joined = in.Joins
	boardWeek := int64(-1)
	if z.Board.Recap != nil {
		if r, ok := z.Board.Recap.Data.(WeeklyRecapData); ok {
			boardWeek = r.WeekStart
		}
	}
	z.History, z.HistoryNext = zoneHistory(in, start, boardWeek)
	return z
}

// zoneStage picks the one thing to watch: a game clip with a poster, else the
// best stream replay, both in the free window; else the belt hero of a quiet
// feed; else a crew of one's own belts.
func zoneStage(in zonesInput, start int64) ZoneStage {
	var clip *StoredClip
	for i := range in.Clips {
		c := in.Clips[i]
		if c.ClipType != clipTypeGame || c.PosterURL == "" || c.Ts < start {
			continue
		}
		if clip == nil || c.Ts > clip.Ts {
			clip = &in.Clips[i]
		}
	}
	if clip != nil {
		e := clipEntry(*clip)
		return ZoneStage{Kind: "game_clip", Entry: &e}
	}
	var stream *StoredStreamSession
	for i := range in.Streams {
		s := in.Streams[i]
		if len(s.SnapshotURLs) == 0 || s.Ts < start {
			continue
		}
		if stream == nil || streamQuality(s) > streamQuality(*stream) ||
			(streamQuality(s) == streamQuality(*stream) && s.Ts > stream.Ts) {
			stream = &in.Streams[i]
		}
	}
	if stream != nil {
		e := streamEntry(*stream)
		return ZoneStage{Kind: "stream_replay", Entry: &e}
	}
	if in.H2h == nil {
		return ZoneStage{}
	}
	if in.H2h.MemberCount >= 2 {
		for _, t := range in.H2h.Titles {
			involves := t.HolderUserID == in.ViewerID || t.PreviousHolderUserID == in.ViewerID
			if t.MovedAtMs > 0 && involves && in.NowMs-t.MovedAtMs <= zonesBeltHeroDays*dayMs {
				return ZoneStage{Kind: "belt"}
			}
		}
		return ZoneStage{}
	}
	if len(in.H2h.OwnValues) > 0 {
		return ZoneStage{Kind: "own_belts"}
	}
	return ZoneStage{}
}

// zoneClips is every clip of the free window but the one on the Stage.
func zoneClips(in zonesInput, start int64, stageID string) []FeedEntry {
	clips := make([]StoredClip, 0, len(in.Clips))
	for _, c := range in.Clips {
		if c.Ts >= start && c.EventID != stageID {
			clips = append(clips, c)
		}
	}
	rankClips(clips)
	out := make([]FeedEntry, 0, len(clips))
	for _, c := range clips {
		out = append(out, clipEntry(c))
	}
	return out
}

// zoneBoard picks the mode and the recap. The quiet and dormant rules are the
// ones of the client's quiet_row.rs, without the live stream (client state).
func zoneBoard(in zonesInput) ZoneBoard {
	b := ZoneBoard{}
	var recap *WeeklyRecapData
	for i := range in.Recaps {
		r := in.Recaps[i]
		if isEmptyRecap(r) {
			continue
		}
		if recap == nil || r.WeekStart > recap.WeekStart {
			recap = &in.Recaps[i]
		}
	}
	if recap != nil {
		b.Recap = &FeedEntry{ID: recapEntryID(recap.WeekStart), Type: "recap", Ts: recap.GeneratedAt, Data: *recap}
	}
	today := floorDiv(in.NowMs, dayMs) * dayMs
	for _, e := range buildDailyRecaps(in.Cards, in.Belts, in.NowMs) {
		if d, ok := e.Data.(DailyRecapData); ok && d.DayStart == today {
			e := e
			b.Today = &e
			break
		}
	}

	// Facts. A recap is a summary of past weeks: it is not activity.
	var newestStream, newestBelt int64
	for _, c := range in.Cards {
		if isSessionCard(c) && c.ts > b.LastActivityMs {
			b.LastActivityMs = c.ts
		}
	}
	for _, c := range in.Clips {
		if c.Ts > b.LastActivityMs {
			b.LastActivityMs = c.Ts
		}
	}
	for _, s := range in.Streams {
		if s.Ts > b.LastActivityMs {
			b.LastActivityMs = s.Ts
		}
		if len(s.SnapshotURLs) > 0 && s.Ts > newestStream {
			newestStream = s.Ts
		}
	}
	members := 0
	if in.H2h != nil {
		members = in.H2h.MemberCount
		for _, t := range in.H2h.Titles {
			if t.MovedAtMs > newestBelt {
				newestBelt = t.MovedAtMs
			}
		}
	}
	b.PlayersLastWeek = playersLastWeek(in.Cards, in.NowMs)

	within := func(ts, ms int64) bool { return ts > 0 && in.NowMs-ts < ms }
	recapThisWeek := recap != nil && within(recap.GeneratedAt, 7*dayMs)
	switch {
	case members <= 1 && in.H2h != nil:
		b.Mode = "solo"
	case b.LastActivityMs > 0 && in.NowMs-b.LastActivityMs >= zonesDormantDays*dayMs:
		b.Mode = "past"
	case b.LastActivityMs > 0 && !within(newestStream, dayMs) && !within(newestBelt, dayMs) && !recapThisWeek:
		b.Mode = "quiet"
	default:
		b.Mode = "tabs"
	}
	return b
}

// recapEntryID is the id the memory section gives a recap.
func recapEntryID(weekStart int64) string {
	return fmt.Sprintf("recap_%d", weekStart)
}

func isSessionCard(c feedCard) bool {
	switch c.backendType {
	case "game_session", "voice_session", "stream_session":
		return true
	}
	return false
}

// playersLastWeek names who played a game in the last 7 days, newest first,
// each once.
func playersLastWeek(cards []feedCard, nowMs int64) []string {
	sorted := append([]feedCard(nil), cards...)
	sort.SliceStable(sorted, func(i, j int) bool { return sorted[i].ts > sorted[j].ts })
	seen := map[string]bool{}
	var out []string
	for _, c := range sorted {
		if c.backendType != "game_session" || nowMs-c.ts > 7*dayMs {
			continue
		}
		d, ok := decodeGameSessionData(c.data)
		if !ok {
			continue
		}
		for _, n := range d.PlayerNames {
			if n != "" && !seen[n] {
				seen[n] = true
				out = append(out, n)
			}
		}
	}
	return out
}

// zoneSessions is every game, voice and stream session of the free window,
// newest first. The stream on the Stage is not repeated.
func zoneSessions(in zonesInput, start int64, stageID string) []FeedEntry {
	cards := make([]feedCard, 0, len(in.Cards))
	for _, c := range in.Cards {
		if isSessionCard(c) && c.ts >= start && c.id != stageID {
			cards = append(cards, c)
		}
	}
	sort.SliceStable(cards, func(i, j int) bool { return cards[i].ts > cards[j].ts })
	out := make([]FeedEntry, 0, len(cards))
	for _, c := range cards {
		out = append(out, feedEntryFromCard(c))
	}
	return out
}

// zoneInstalled is the games most members have, for a quiet week.
func zoneInstalled(in zonesInput) []RecentGameTile {
	card, ok := buildRecentGames(in.Cards, in.Libs)
	if !ok {
		return nil
	}
	d, ok := card.data.(RecentGamesData)
	if !ok {
		return nil
	}
	games := append([]RecentGameTile(nil), d.Games...)
	sort.SliceStable(games, func(i, j int) bool { return len(games[i].MemberIDs) > len(games[j].MemberIDs) })
	if len(games) > zonesInstalledMax {
		games = games[:zonesInstalledMax]
	}
	return games
}

// zoneHistory groups everything older than the free window by week, newest
// first, one page. boardWeek is the recap on the board: it is not history.
func zoneHistory(in zonesInput, start, boardWeek int64) ([]ZoneHistoryWeek, int64) {
	type week struct {
		recap   *WeeklyRecapData
		clips   []StoredClip
		streams []StoredStreamSession
	}
	weeks := map[int64]*week{}
	at := func(ws int64) *week {
		w := weeks[ws]
		if w == nil {
			w = &week{}
			weeks[ws] = w
		}
		return w
	}
	for _, c := range in.Clips {
		if c.Ts < start {
			w := at(weekStartOf(c.Ts))
			w.clips = append(w.clips, c)
		}
	}
	for _, s := range in.Streams {
		if s.Ts < start {
			w := at(weekStartOf(s.Ts))
			w.streams = append(w.streams, s)
		}
	}
	for i := range in.Recaps {
		r := in.Recaps[i]
		if r.WeekStart <= 0 || r.WeekStart == boardWeek || isEmptyRecap(r) {
			continue
		}
		at(weekStartOf(r.WeekStart)).recap = &in.Recaps[i]
	}

	starts := make([]int64, 0, len(weeks))
	for ws := range weeks {
		if in.HistoryBefore <= 0 || ws < in.HistoryBefore {
			starts = append(starts, ws)
		}
	}
	sort.Slice(starts, func(i, j int) bool { return starts[i] > starts[j] })
	next := int64(0)
	if len(starts) > zonesHistoryPageWeeks {
		starts = starts[:zonesHistoryPageWeeks]
		next = starts[len(starts)-1]
	}

	out := make([]ZoneHistoryWeek, 0, len(starts))
	for _, ws := range starts {
		w := weeks[ws]
		rankClips(w.clips)
		sort.SliceStable(w.streams, func(i, j int) bool { return w.streams[i].Ts > w.streams[j].Ts })
		h := ZoneHistoryWeek{
			WeekStart:   ws,
			Locked:      !in.Plus,
			ClipCount:   len(w.clips),
			StreamCount: len(w.streams),
		}
		if w.recap != nil {
			h.WeekNumber = w.recap.WeekNumber
		}
		if in.Plus {
			if w.recap != nil {
				h.Recap = &FeedEntry{ID: recapEntryID(w.recap.WeekStart), Type: "recap", Ts: w.recap.GeneratedAt, Data: *w.recap}
			}
			for _, c := range w.clips {
				h.Items = append(h.Items, clipEntry(c))
			}
			for _, s := range w.streams {
				h.Items = append(h.Items, streamEntry(s))
			}
		} else {
			h.Teasers = weekTeasers(w.clips, w.streams)
		}
		out = append(out, h)
	}
	return out, next
}

// weekTeasers is what a locked week shows: who, what kind, how long. No URL.
func weekTeasers(clips []StoredClip, streams []StoredStreamSession) []ZoneTeaser {
	out := make([]ZoneTeaser, 0, zonesTeasersMax)
	for _, c := range clips {
		if len(out) == zonesTeasersMax {
			return out
		}
		kind := "voice"
		if c.ClipType == clipTypeGame {
			kind = "clip"
		}
		out = append(out, ZoneTeaser{Kind: kind, ActorID: c.ActorID, ActorName: c.ClipperName, DurationSeconds: c.DurationSeconds, Ts: c.Ts})
	}
	for _, s := range streams {
		if len(out) == zonesTeasersMax {
			return out
		}
		out = append(out, ZoneTeaser{Kind: "stream", ActorID: s.StreamerID, ActorName: s.StreamerName, DurationSeconds: float64(s.DurationMin) * 60, Ts: s.Ts})
	}
	return out
}
