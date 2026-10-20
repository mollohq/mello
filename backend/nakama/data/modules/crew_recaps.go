package main

import (
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/json"
	"fmt"
	"math/big"
	"sort"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// ---------------------------------------------------------------------------
// Durable recaps store — separate per-crew document, outside the ledger trim
// ---------------------------------------------------------------------------

const CrewRecapsCollection = "crew_recaps"

type CrewRecapsDoc struct {
	CrewID string            `json:"crew_id"`
	Recaps []WeeklyRecapData `json:"recaps"` // newest appended last
	// LastRecapWeek is the ISO week ("2026-W43") of the last recap the job
	// wrote for this crew. The job runs for a crew when the current week is
	// not this value, so a missed Monday runs later (recapDue).
	LastRecapWeek string `json:"last_recap_week,omitempty"`
	UpdatedAt     int64  `json:"updated_at"`
}

func readRecapsDoc(ctx context.Context, nk runtime.NakamaModule, crewID string) (*CrewRecapsDoc, string) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: CrewRecapsCollection, Key: crewID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return &CrewRecapsDoc{CrewID: crewID, Recaps: []WeeklyRecapData{}}, ""
	}

	var doc CrewRecapsDoc
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &doc); err != nil {
		return &CrewRecapsDoc{CrewID: crewID, Recaps: []WeeklyRecapData{}}, ""
	}
	return &doc, objects[0].GetVersion()
}

func writeRecapsDoc(ctx context.Context, nk runtime.NakamaModule, crewID string, doc *CrewRecapsDoc, version string) error {
	data, err := json.Marshal(doc)
	if err != nil {
		return err
	}
	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      CrewRecapsCollection,
			Key:             crewID,
			UserID:          SystemUserID,
			Value:           string(data),
			Version:         version,
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	})
	return err
}

// AppendRecap appends a recap to the durable recaps doc and retries on version
// conflict. No cap: recaps are tiny and one per crew per week.
func AppendRecap(ctx context.Context, nk runtime.NakamaModule, crewID string, recap WeeklyRecapData) error {
	for attempt := 0; attempt < 3; attempt++ {
		doc, version := readRecapsDoc(ctx, nk, crewID)
		doc.Recaps = append(doc.Recaps, recap)
		doc.LastRecapWeek = isoWeekKey(time.UnixMilli(recap.WeekEnd))
		doc.UpdatedAt = time.Now().UnixMilli()
		if err := writeRecapsDoc(ctx, nk, crewID, doc, version); err == nil {
			return nil
		}
		jitter, _ := rand.Int(rand.Reader, big.NewInt(50))
		time.Sleep(time.Duration(50*(attempt+1)+int(jitter.Int64())) * time.Millisecond)
	}
	return fmt.Errorf("crew_recaps write failed after 3 retries for crew %s", crewID)
}

// ---------------------------------------------------------------------------
// Weekly recap job (runs Monday 00:00 UTC)
// ---------------------------------------------------------------------------

type RecapMember struct {
	UserID      string `json:"user_id"` // for client-side avatar resolution (spec 19)
	DisplayName string `json:"display_name"`
	HangoutMin  int    `json:"hangout_min"`
}

// RecapGameRecord is a member's win/loss record across the week (spec 18).
type RecapGameRecord struct {
	UserID      string `json:"user_id"` // for client-side avatar resolution (spec 19)
	DisplayName string `json:"display_name"`
	Wins        int    `json:"wins"`
	Losses      int    `json:"losses"`
}

// RecapGameTally is a crew-wide match count for one game (spec 19).
type RecapGameTally struct {
	Game    string `json:"game"`
	Matches int    `json:"matches"`
}

// RecapAward is a fun, shareable superlative for the week (spec 19).
type RecapAward struct {
	Kind   string `json:"kind"` // "grinder" | "heater" | "skid"
	Title  string `json:"title"`
	Name   string `json:"name"`
	Detail string `json:"detail"`
}

// RecapBeltMoved is one head2head title that changed hands in the recap week
// (plans/next-gen-feed.md section 8). The value uses the encoding of
// Head2headTitle.Value.
type RecapBeltMoved struct {
	TitleID        string      `json:"title_id"`
	HolderUserID   string      `json:"holder_user_id"`
	HolderName     string      `json:"holder_name"`
	PreviousUserID string      `json:"previous_holder_user_id"`
	PreviousName   string      `json:"previous_holder_name"`
	Value          interface{} `json:"value"`
	Unit           string      `json:"unit"`
	MovedAtMs      int64       `json:"moved_at_ms"`
}

// RecapBeltHeld is the number of titles one member holds when the recap is
// generated, with the ids of those titles.
type RecapBeltHeld struct {
	UserID      string   `json:"user_id"` // for client-side avatar resolution (spec 19)
	DisplayName string   `json:"display_name"`
	Count       int      `json:"count"`
	TitleIDs    []string `json:"title_ids"`
}

type WeeklyRecapData struct {
	CrewID    string `json:"crew_id"`
	WeekStart int64  `json:"week_start"`
	WeekEnd   int64  `json:"week_end"`
	// Voice plus stream minutes only (CLIPS.md 10.1). Game time is separate.
	TotalHangoutMin int `json:"total_hangout_min"`
	// Game session minutes, counted by reportableMinutes so the recap agrees
	// with the Crew play card.
	TotalGameMin int `json:"total_game_min"`
	// TotalMin is TotalHangoutMin + TotalGameMin: the figure the card leads with.
	// Recaps stored before this field existed have 0 here; fall back to
	// TotalHangoutMin.
	TotalMin int `json:"total_min"`
	// Voice, stream and game sessions in the week.
	SessionCount      int           `json:"session_count"`
	TopGame           string        `json:"top_game"`
	LongestSession    string        `json:"longest_session"`
	LongestSessionMin int           `json:"longest_session_min"`
	ClipCount         int           `json:"clip_count"`
	MostActive        string        `json:"most_active"`
	MostClipped       string        `json:"most_clipped"`
	TopMembers        []RecapMember `json:"top_members"`
	// Telemetry-derived (spec 18/19); omitted when no game outcomes were recorded.
	GameRecords    []RecapGameRecord `json:"game_records,omitempty"` // leaderboard, most wins first
	GamesPlayed    []RecapGameTally  `json:"games_played,omitempty"` // crew-wide match counts
	Awards         []RecapAward      `json:"awards,omitempty"`
	BestStreak     int               `json:"best_streak,omitempty"`
	BestStreakName string            `json:"best_streak_name,omitempty"`
	// Head2head belts, read from the holders doc at generation time. Both are
	// omitted for a recap with no activity.
	//
	//   - BeltsMoved: titles whose moved_at_ms is in the recap week. The same
	//     titles are also appended to Awards (kind "belt").
	//   - BeltsHeld: titles held by each member, most first. Leaderboard has
	//     the same rows, for the RecapCard leaderboard field (spec 19, 3.5).
	BeltsMoved  []RecapBeltMoved `json:"belts_moved,omitempty"`
	BeltsHeld   []RecapBeltHeld  `json:"belts_held,omitempty"`
	Leaderboard []RecapBeltHeld  `json:"leaderboard,omitempty"`
	GeneratedAt int64            `json:"generated_at"`
}

// isEmptyRecap reports whether a recap has nothing to show: no hangout or game
// minutes and no clips. The weekly job skips these, and the feed hides any
// already stored.
//
// SessionCount is not checked. A session with zero minutes would render the
// "0m" hero this guard exists to prevent. A week with only joins or chat adds
// no minutes and no clips, so it is empty too. A week with clips and no
// minutes is not empty: the card shows the clip count.
//
// Old recaps: recaps stored before TotalGameMin and TotalMin existed hold 0
// there. One with game play only has TotalHangoutMin 0 and no clips, so it is
// now hidden. This is accepted: it was the "0m" card, and its game time was
// never stored, so it cannot be rebuilt.
func isEmptyRecap(r WeeklyRecapData) bool {
	return r.TotalHangoutMin == 0 && r.TotalGameMin == 0 && r.TotalMin == 0 &&
		r.ClipCount == 0
}

// generateWeeklyRecap writes the recap of the 7 days up to now. The dev tools
// use it. The job uses generateWeeklyRecapAt with the Monday that ends the week.
func generateWeeklyRecap(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, crewID string) {
	generateWeeklyRecapAt(ctx, nk, logger, crewID, time.Now())
}

// generateWeeklyRecapAt writes the recap of the 7 days that end at weekEnd.
// Events and clips after weekEnd are left out, so a late run (recapLookback)
// still reports the week that ended on Monday 00:00 UTC.
func generateWeeklyRecapAt(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, crewID string, weekEnd time.Time) {
	ledger, _ := readLedger(ctx, nk, crewID)

	weekStart := weekEnd.Add(-7 * 24 * time.Hour)
	startMs := weekStart.UnixMilli()
	endMs := weekEnd.UnixMilli()

	var totalHangoutMin int
	var totalGameMin int
	sessionCount := 0
	gameDurations := make(map[string]int)
	actorActivity := make(map[string]int)
	actorClips := make(map[string]int)
	actorWins := make(map[string]int)
	actorLosses := make(map[string]int)
	actorMatches := make(map[string]int)
	gameMatches := make(map[string]int)
	bestStreak := 0
	bestStreakActor := ""
	worstStreak := 0
	worstStreakActor := ""
	clipCount := 0
	longestSessionDesc := ""
	longestSessionMin := 0

	for _, e := range ledger.Events {
		if e.Timestamp < startMs || e.Timestamp > endMs {
			continue
		}
		// System events (e.g. voice_session) have an empty actor; don't let them
		// pollute the MVP tally (otherwise the top "MVP" can render as a blank "?").
		if e.ActorID != "" {
			actorActivity[e.ActorID]++
		}

		dataBytes, _ := json.Marshal(e.Data)
		switch e.Type {
		case "voice_session":
			var d VoiceSessionData
			json.Unmarshal(dataBytes, &d)
			sessionCount++
			totalHangoutMin += d.DurationMin
			if d.DurationMin > longestSessionMin {
				longestSessionMin = d.DurationMin
				longestSessionDesc = fmt.Sprintf("%s in %s (%dm)",
					joinNamesList(d.ParticipantNames, 2), d.ChannelName, d.DurationMin)
			}
		case "stream_session":
			var d StreamSessionData
			json.Unmarshal(dataBytes, &d)
			sessionCount++
			totalHangoutMin += d.DurationMin
			if d.DurationMin > longestSessionMin {
				longestSessionMin = d.DurationMin
				longestSessionDesc = fmt.Sprintf("%s streaming %s (%dm)",
					d.StreamerName, d.Title, d.DurationMin)
			}
		case "game_session":
			var d GameSessionData
			json.Unmarshal(dataBytes, &d)
			sessionCount++
			totalGameMin += reportableMinutes(d.DurationMin, d.ActiveMin)
			gameDurations[d.GameName] += d.DurationMin
			matches := d.Wins + d.Losses + d.Draws
			if matches > 0 {
				gameMatches[d.GameName] += matches
				actorMatches[e.ActorID] += matches
				actorWins[e.ActorID] += d.Wins
				actorLosses[e.ActorID] += d.Losses
				if d.StreakAfter > bestStreak {
					bestStreak = d.StreakAfter
					bestStreakActor = e.ActorID
				}
				if d.StreakAfter < worstStreak {
					worstStreak = d.StreakAfter
					worstStreakActor = e.ActorID
				}
			}
		}
	}

	// Clips are durable and no longer in the ledger; count them from the clips doc.
	clipsDoc, _ := readClipsDoc(ctx, nk, crewID)
	for _, c := range clipsDoc.Clips {
		if c.Ts >= startMs && c.Ts <= endMs {
			clipCount++
			actorClips[c.ActorID]++
		}
	}

	topGame := ""
	topGameMin := 0
	for game, dur := range gameDurations {
		if dur > topGameMin {
			topGame = game
			topGameMin = dur
		}
	}

	mostActive := topActor(actorActivity, ctx, nk)
	mostClipped := topActor(actorClips, ctx, nk)
	topMembers := topActors(actorActivity, 3, ctx, nk)
	gameRecords := buildGameRecords(actorWins, actorLosses, ctx, nk)

	bestStreakName := resolveOrID(ctx, nk, bestStreakActor)
	gamesPlayed := buildGamesPlayed(gameMatches)
	awards := buildAwards(ctx, nk, actorMatches, bestStreak, bestStreakName, worstStreak, worstStreakActor)

	recap := WeeklyRecapData{
		CrewID:            crewID,
		WeekStart:         startMs,
		WeekEnd:           weekEnd.UnixMilli(),
		TotalHangoutMin:   totalHangoutMin,
		TotalGameMin:      totalGameMin,
		TotalMin:          totalHangoutMin + totalGameMin,
		SessionCount:      sessionCount,
		TopGame:           topGame,
		LongestSession:    longestSessionDesc,
		LongestSessionMin: longestSessionMin,
		ClipCount:         clipCount,
		MostActive:        mostActive,
		MostClipped:       mostClipped,
		TopMembers:        topMembers,
		GameRecords:       gameRecords,
		GamesPlayed:       gamesPlayed,
		Awards:            awards,
		BestStreak:        bestStreak,
		BestStreakName:    bestStreakName,
		GeneratedAt:       time.Now().UnixMilli(),
	}

	if isEmptyRecap(recap) {
		logger.Debug("Weekly recap skipped for crew %s: no activity", crewID)
		return
	}

	// Belts only join a recap that has activity. Holding titles is a standing
	// state and must not turn an empty week into a recap with content.
	attachRecapBelts(ctx, nk, logger, crewID, &recap, startMs, weekEnd.UnixMilli())

	if err := AppendRecap(ctx, nk, crewID, recap); err != nil {
		logger.Error("Failed to store weekly recap for crew %s: %v", crewID, err)
	} else {
		logger.Info("Weekly recap generated for crew %s: hangout=%dm clips=%d top_game=%s",
			crewID, totalHangoutMin, clipCount, topGame)
		scheduleRecapPushes(ctx, nk, logger, crewID, recap)
	}
}

// head2headTitleLabels is the English label of each title, for recap awards.
// The client owns the display copy of the head2head card.
var head2headTitleLabels = map[string]string{
	titleTheCarry:   "The Carry",
	titleLockedIn:   "Locked In",
	titleHasALife:   "Has a Life",
	titleNightOwl:   "Night Owl",
	titleTheCollect: "The Collector",
	titleSampler:    "Sampler",
	titleOneTrick:   "One Trick",
	titleMouth:      "Mouth",
	titleMarathon:   "Marathon",
	titleLastOneUp:  "Last One Up",
	titleClipper:    "Clipper",
	titleRageQuit:   "Rage Quit",
	titleWinner:     "Winner",
}

// buildRecapBelts derives the belt sections from a holders doc. A title moved
// in the week when its moved_at_ms is in [startMs, endMs]. Held counts every
// contested title in the doc. Pure, so the rules are unit-testable. name
// resolves a user id to a display name.
func buildRecapBelts(doc *Head2headDoc, startMs, endMs int64, name func(string) string) ([]RecapBeltMoved, []RecapBeltHeld) {
	if doc == nil {
		return nil, nil
	}
	var moved []RecapBeltMoved
	held := map[string]*RecapBeltHeld{}
	for _, t := range doc.Titles {
		if t.HolderUserID == "" {
			continue
		}
		h := held[t.HolderUserID]
		if h == nil {
			h = &RecapBeltHeld{UserID: t.HolderUserID, DisplayName: name(t.HolderUserID), TitleIDs: []string{}}
			held[t.HolderUserID] = h
		}
		h.Count++
		h.TitleIDs = append(h.TitleIDs, t.TitleID)
		if t.MovedAtMs >= startMs && t.MovedAtMs <= endMs {
			moved = append(moved, RecapBeltMoved{
				TitleID:        t.TitleID,
				HolderUserID:   t.HolderUserID,
				HolderName:     name(t.HolderUserID),
				PreviousUserID: t.PreviousHolderUserID,
				PreviousName:   name(t.PreviousHolderUserID),
				Value:          t.Value,
				Unit:           t.Unit,
				MovedAtMs:      t.MovedAtMs,
			})
		}
	}
	sort.SliceStable(moved, func(i, j int) bool { return moved[i].MovedAtMs < moved[j].MovedAtMs })
	heldList := make([]RecapBeltHeld, 0, len(held))
	for _, h := range held {
		heldList = append(heldList, *h)
	}
	sort.Slice(heldList, func(i, j int) bool {
		if heldList[i].Count != heldList[j].Count {
			return heldList[i].Count > heldList[j].Count
		}
		return heldList[i].UserID < heldList[j].UserID
	})
	return moved, heldList
}

// beltAwards turns moved belts into recap awards, one for each belt.
func beltAwards(moved []RecapBeltMoved) []RecapAward {
	out := make([]RecapAward, 0, len(moved))
	for _, m := range moved {
		title := head2headTitleLabels[m.TitleID]
		if title == "" {
			title = m.TitleID
		}
		detail := ""
		if m.PreviousName != "" {
			detail = "took it from " + m.PreviousName
		}
		out = append(out, RecapAward{Kind: "belt", Title: title, Name: m.HolderName, Detail: detail})
	}
	return out
}

// attachRecapBelts fills the belt sections of a recap from the holders doc of
// the crew. It recomputes the doc first, because the session trigger is
// debounced and the doc can lag the last session. If the recompute fails it
// reads the stored doc. Never fails the recap.
func attachRecapBelts(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, crewID string, recap *WeeklyRecapData, startMs, endMs int64) {
	doc := recomputeHead2head(ctx, logger, nk, crewID, head2headTriggerLibrary, endMs)
	if doc == nil {
		doc, _ = readHead2headDoc(ctx, nk, crewID)
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
	moved, held := buildRecapBelts(doc, startMs, endMs, name)
	recap.BeltsMoved = moved
	recap.BeltsHeld = held
	recap.Leaderboard = held
	recap.Awards = append(recap.Awards, beltAwards(moved)...)
}

func resolveOrID(ctx context.Context, nk runtime.NakamaModule, id string) string {
	if id == "" {
		return ""
	}
	if name := resolveUsername(ctx, nk, id); name != "" {
		return name
	}
	return id
}

// topKeyCount returns the map key with the highest value (and that value).
func topKeyCount(counts map[string]int) (string, int) {
	topID := ""
	topCount := 0
	for id, c := range counts {
		if c > topCount {
			topID = id
			topCount = c
		}
	}
	return topID, topCount
}

// buildGamesPlayed turns crew-wide per-game match counts into a sorted list
// (most matches first). Pure, so the ordering is unit-testable.
func buildGamesPlayed(gameMatches map[string]int) []RecapGameTally {
	out := make([]RecapGameTally, 0, len(gameMatches))
	for g, m := range gameMatches {
		out = append(out, RecapGameTally{Game: g, Matches: m})
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Matches > out[j].Matches })
	return out
}

// buildAwards composes the week's fun superlatives: grinder of the week (most
// matches), biggest heater (best win streak), roughest patch (worst loss skid).
func buildAwards(ctx context.Context, nk runtime.NakamaModule, actorMatches map[string]int, bestStreak int, bestStreakName string, worstStreak int, worstStreakActor string) []RecapAward {
	awards := make([]RecapAward, 0, 3)

	if grinderID, grinderMatches := topKeyCount(actorMatches); grinderID != "" {
		awards = append(awards, RecapAward{
			Kind:   "grinder",
			Title:  "Grinder of the week",
			Name:   resolveOrID(ctx, nk, grinderID),
			Detail: fmt.Sprintf("%d matches", grinderMatches),
		})
	}
	if bestStreak >= 2 && bestStreakName != "" {
		awards = append(awards, RecapAward{
			Kind:   "heater",
			Title:  "Biggest heater",
			Name:   bestStreakName,
			Detail: fmt.Sprintf("%d-win streak", bestStreak),
		})
	}
	if worstStreak <= -2 && worstStreakActor != "" {
		awards = append(awards, RecapAward{
			Kind:   "skid",
			Title:  "Roughest patch",
			Name:   resolveOrID(ctx, nk, worstStreakActor),
			Detail: fmt.Sprintf("%d-loss skid", -worstStreak),
		})
	}
	return awards
}

func topActors(counts map[string]int, limit int, ctx context.Context, nk runtime.NakamaModule) []RecapMember {
	type kv struct {
		id    string
		count int
	}
	sorted := make([]kv, 0, len(counts))
	for id, c := range counts {
		sorted = append(sorted, kv{id, c})
	}
	for i := 0; i < len(sorted); i++ {
		for j := i + 1; j < len(sorted); j++ {
			if sorted[j].count > sorted[i].count {
				sorted[i], sorted[j] = sorted[j], sorted[i]
			}
		}
	}
	if len(sorted) > limit {
		sorted = sorted[:limit]
	}
	members := make([]RecapMember, 0, len(sorted))
	for _, s := range sorted {
		name := resolveUsername(ctx, nk, s.id)
		if name == "" {
			name = s.id
		}
		members = append(members, RecapMember{
			UserID:      s.id,
			DisplayName: name,
			HangoutMin:  s.count,
		})
	}
	return members
}

// buildGameRecords turns per-actor win/loss tallies into a sorted record list
// (most wins first, then fewest losses). Resolves display names.
func buildGameRecords(wins, losses map[string]int, ctx context.Context, nk runtime.NakamaModule) []RecapGameRecord {
	seen := make(map[string]bool)
	ids := make([]string, 0, len(wins)+len(losses))
	for id := range wins {
		if !seen[id] {
			seen[id] = true
			ids = append(ids, id)
		}
	}
	for id := range losses {
		if !seen[id] {
			seen[id] = true
			ids = append(ids, id)
		}
	}
	if len(ids) == 0 {
		return nil
	}

	records := make([]RecapGameRecord, 0, len(ids))
	for _, id := range ids {
		name := resolveUsername(ctx, nk, id)
		if name == "" {
			name = id
		}
		records = append(records, RecapGameRecord{
			UserID:      id,
			DisplayName: name,
			Wins:        wins[id],
			Losses:      losses[id],
		})
	}
	sort.Slice(records, func(i, j int) bool {
		if records[i].Wins != records[j].Wins {
			return records[i].Wins > records[j].Wins
		}
		return records[i].Losses < records[j].Losses
	})
	return records
}

func topActor(counts map[string]int, ctx context.Context, nk runtime.NakamaModule) string {
	topID := ""
	topCount := 0
	for id, c := range counts {
		if c > topCount {
			topID = id
			topCount = c
		}
	}
	if topID == "" {
		return ""
	}
	if name := resolveUsername(ctx, nk, topID); name != "" {
		return name
	}
	return topID
}

// recapLookback is how long after Monday 00:00 UTC the job may still write the
// recap of the week that ended then. A missed Monday runs on a later tick. After
// the lookback the week is skipped: a recap on Friday for last week is stale.
const recapLookback = 72 * time.Hour

// isoWeekKey names an ISO week, for example "2026-W43".
func isoWeekKey(t time.Time) string {
	y, w := t.UTC().ISOWeek()
	return fmt.Sprintf("%04d-W%02d", y, w)
}

// mondayStartUTC is Monday 00:00 UTC of the ISO week that holds t. It is the
// end of the week a recap covers.
func mondayStartUTC(t time.Time) time.Time {
	u := t.UTC()
	back := (int(u.Weekday()) + 6) % 7 // Monday is 0
	return time.Date(u.Year(), u.Month(), u.Day()-back, 0, 0, 0, 0, time.UTC)
}

// recapDue reports whether the job must write a recap for a crew at now: the
// current ISO week has none yet, and now is inside the lookback after Monday
// 00:00 UTC. A recap already stored with a week end in this week counts as
// written, which covers a crew whose recap predates last_recap_week.
func recapDue(doc *CrewRecapsDoc, now time.Time) bool {
	weekEnd := mondayStartUTC(now)
	if now.Before(weekEnd) || now.Sub(weekEnd) >= recapLookback {
		return false
	}
	key := isoWeekKey(now)
	if doc.LastRecapWeek == key {
		return false
	}
	if n := len(doc.Recaps); n > 0 && isoWeekKey(time.UnixMilli(doc.Recaps[n-1].WeekEnd)) == key {
		return false
	}
	return true
}

// StartWeeklyRecapJob runs at start and then every hour. Each run writes the
// recap of every crew that is due (recapDue) and sends the recap pushes that
// are due. The state is in storage, so a restart or a missed Monday loses
// nothing within the lookback.
func StartWeeklyRecapJob(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger) {
	runWeeklyRecapTick(ctx, nk, logger, time.Now())

	ticker := time.NewTicker(1 * time.Hour)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case t := <-ticker.C:
			runWeeklyRecapTick(ctx, nk, logger, t)
		}
	}
}

// runWeeklyRecapTick is one pass of the job at now.
func runWeeklyRecapTick(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, now time.Time) {
	defer func() {
		if r := recover(); r != nil {
			logger.Error("Weekly recap tick panic: %v", r)
		}
	}()
	generateDueRecaps(ctx, nk, logger, now)
	sendDueRecapPushes(ctx, nk, logger, now)
}

// generateDueRecaps writes the recap of each crew that is due at now.
func generateDueRecaps(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, now time.Time) {
	if now.Before(mondayStartUTC(now)) || now.Sub(mondayStartUTC(now)) >= recapLookback {
		return
	}
	logger.Info("Weekly recap check for %s", isoWeekKey(now))
	cursor := ""
	for {
		groups, nextCursor, err := nk.GroupsList(ctx, "", "", nil, nil, 100, cursor)
		if err != nil {
			logger.Error("Weekly recap: failed to list groups: %v", err)
			return
		}
		for _, g := range groups {
			doc, _ := readRecapsDoc(ctx, nk, g.GetId())
			if recapDue(doc, now) {
				generateWeeklyRecapAt(ctx, nk, logger, g.GetId(), mondayStartUTC(now))
			}
		}
		if nextCursor == "" || len(groups) == 0 {
			break
		}
		cursor = nextCursor
	}
}

// ---------------------------------------------------------------------------
// CrewRecaps RPC — paginated durable recap history (newest first)
// ---------------------------------------------------------------------------

type RecapsPageRequest struct {
	CrewID string `json:"crew_id"`
	Cursor string `json:"cursor,omitempty"` // week_start of previous page's last item, as string
	Limit  int    `json:"limit,omitempty"`
}

type RecapsPageResponse struct {
	CrewID  string            `json:"crew_id"`
	Recaps  []WeeklyRecapData `json:"recaps"`
	Cursor  string            `json:"cursor,omitempty"`
	HasMore bool              `json:"has_more"`
}

func CrewRecapsRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req RecapsPageRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	limit := req.Limit
	if limit <= 0 || limit > TimelinePageSize {
		limit = TimelinePageSize
	}

	doc, _ := readRecapsDoc(ctx, nk, req.CrewID)

	sort.Slice(doc.Recaps, func(i, j int) bool {
		return doc.Recaps[i].WeekStart > doc.Recaps[j].WeekStart
	})

	startIdx := 0
	if req.Cursor != "" {
		for i, r := range doc.Recaps {
			if fmt.Sprintf("%d", r.WeekStart) == req.Cursor {
				startIdx = i + 1
				break
			}
		}
	}

	end := startIdx + limit
	if end > len(doc.Recaps) {
		end = len(doc.Recaps)
	}

	page := doc.Recaps[startIdx:end]

	var cursor string
	hasMore := end < len(doc.Recaps)
	if hasMore && len(page) > 0 {
		cursor = fmt.Sprintf("%d", page[len(page)-1].WeekStart)
	}

	resp := RecapsPageResponse{
		CrewID:  req.CrewID,
		Recaps:  page,
		Cursor:  cursor,
		HasMore: hasMore,
	}
	data, _ := json.Marshal(resp)
	return string(data), nil
}
