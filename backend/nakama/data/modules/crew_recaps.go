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
	GeneratedAt       int64         `json:"generated_at"`
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
		GeneratedAt:       time.Now().UnixMilli(),
	}

	if err := AppendRecap(ctx, nk, crewID, recap); err != nil {
		logger.Error("Failed to store weekly recap for crew %s: %v", crewID, err)
	} else {
		logger.Info("Weekly recap generated for crew %s: hangout=%dm clips=%d top_game=%s",
			crewID, totalHangoutMin, clipCount, topGame)
	}
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
