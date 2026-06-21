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
	CrewID    string            `json:"crew_id"`
	Recaps    []WeeklyRecapData `json:"recaps"` // newest appended last
	UpdatedAt int64             `json:"updated_at"`
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
	DisplayName string `json:"display_name"`
	HangoutMin  int    `json:"hangout_min"`
}

// RecapGameRecord is a member's win/loss record across the week (spec 18).
type RecapGameRecord struct {
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

type WeeklyRecapData struct {
	CrewID            string        `json:"crew_id"`
	WeekStart         int64         `json:"week_start"`
	WeekEnd           int64         `json:"week_end"`
	TotalHangoutMin   int           `json:"total_hangout_min"`
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
	GeneratedAt    int64             `json:"generated_at"`
}

func generateWeeklyRecap(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, crewID string) {
	ledger, _ := readLedger(ctx, nk, crewID)

	weekEnd := time.Now()
	weekStart := weekEnd.Add(-7 * 24 * time.Hour)
	startMs := weekStart.UnixMilli()

	var totalHangoutMin int
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
		if e.Timestamp < startMs {
			continue
		}
		actorActivity[e.ActorID]++

		dataBytes, _ := json.Marshal(e.Data)
		switch e.Type {
		case "voice_session":
			var d VoiceSessionData
			json.Unmarshal(dataBytes, &d)
			totalHangoutMin += d.DurationMin
			if d.DurationMin > longestSessionMin {
				longestSessionMin = d.DurationMin
				longestSessionDesc = fmt.Sprintf("%s in %s (%dm)",
					joinNamesList(d.ParticipantNames, 2), d.ChannelName, d.DurationMin)
			}
		case "stream_session":
			var d StreamSessionData
			json.Unmarshal(dataBytes, &d)
			totalHangoutMin += d.DurationMin
			if d.DurationMin > longestSessionMin {
				longestSessionMin = d.DurationMin
				longestSessionDesc = fmt.Sprintf("%s streaming %s (%dm)",
					d.StreamerName, d.Title, d.DurationMin)
			}
		case "game_session":
			var d GameSessionData
			json.Unmarshal(dataBytes, &d)
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
		if c.Ts >= startMs {
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

	if err := AppendRecap(ctx, nk, crewID, recap); err != nil {
		logger.Error("Failed to store weekly recap for crew %s: %v", crewID, err)
	} else {
		logger.Info("Weekly recap generated for crew %s: hangout=%dm clips=%d top_game=%s",
			crewID, totalHangoutMin, clipCount, topGame)
	}
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

// StartWeeklyRecapJob runs every hour, checks if it's Monday 00:xx UTC,
// and generates recaps for all active crews.
func StartWeeklyRecapJob(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger) {
	ticker := time.NewTicker(1 * time.Hour)
	defer ticker.Stop()

	lastRunWeek := -1

	for {
		select {
		case <-ctx.Done():
			return
		case t := <-ticker.C:
			_, week := t.UTC().ISOWeek()
			if t.UTC().Weekday() == time.Monday && t.UTC().Hour() == 0 && week != lastRunWeek {
				lastRunWeek = week
				logger.Info("Weekly recap job started for week %d", week)
				generateRecapsForAllCrews(ctx, nk, logger)
			}
		}
	}
}

func generateRecapsForAllCrews(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger) {
	// List all crews (Nakama groups)
	cursor := ""
	for {
		groups, nextCursor, err := nk.GroupsList(ctx, "", "", nil, nil, 100, cursor)
		if err != nil {
			logger.Error("Weekly recap: failed to list groups: %v", err)
			return
		}
		for _, g := range groups {
			generateWeeklyRecap(ctx, nk, logger, g.GetId())
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
