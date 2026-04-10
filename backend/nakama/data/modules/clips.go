package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"sort"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const (
	ClipMaxDurationSec    = 60
	ClipMaxPerUserPerDay  = 50
	TimelinePageSize      = 20
	WeeklyRecapCollection = "weekly_recaps"
)

// ---------------------------------------------------------------------------
// Clip data model
// ---------------------------------------------------------------------------

type ClipData struct {
	ClipID           string   `json:"clip_id"`
	ClipType         string   `json:"clip_type"`
	ClipperName      string   `json:"clipper_name"`
	DurationSeconds  float64  `json:"duration_seconds"`
	Participants     []string `json:"participants,omitempty"`
	ParticipantNames []string `json:"participant_names,omitempty"`
	Game             string   `json:"game,omitempty"`
	LocalPath        string   `json:"local_path,omitempty"`
	MediaURL         string   `json:"media_url,omitempty"`
}

// ---------------------------------------------------------------------------
// PostClip RPC — store clip metadata in the crew event ledger
// ---------------------------------------------------------------------------

type PostClipRequest struct {
	CrewID          string   `json:"crew_id"`
	ClipID          string   `json:"clip_id"`
	ClipType        string   `json:"clip_type"`
	DurationSeconds float64  `json:"duration_seconds"`
	Participants    []string `json:"participants,omitempty"`
	Game            string   `json:"game,omitempty"`
	LocalPath       string   `json:"local_path,omitempty"`
}

func PostClipRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req PostClipRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" || req.ClipID == "" {
		return "", runtime.NewError("crew_id and clip_id required", 3)
	}
	if req.ClipType == "" {
		req.ClipType = "voice"
	}
	if req.DurationSeconds <= 0 || req.DurationSeconds > ClipMaxDurationSec {
		return "", runtime.NewError(fmt.Sprintf("duration must be 0-%d seconds", ClipMaxDurationSec), 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	// Rate limit: max clips per user per day
	ledger, _ := readLedger(ctx, nk, req.CrewID)
	dayStart := time.Now().Truncate(24 * time.Hour).UnixMilli()
	clipCount := 0
	for _, e := range ledger.Events {
		if e.Type == "clip" && e.ActorID == userID && e.Timestamp >= dayStart {
			clipCount++
		}
	}
	if clipCount >= ClipMaxPerUserPerDay {
		return "", runtime.NewError("clip rate limit exceeded", 8)
	}

	username := resolveUsername(ctx, nk, userID)
	participantNames := make([]string, 0, len(req.Participants))
	for _, pid := range req.Participants {
		if n := resolveUsername(ctx, nk, pid); n != "" {
			participantNames = append(participantNames, n)
		}
	}

	eventID := generateEventID()
	event := CrewEvent{
		ID:        eventID,
		CrewID:    req.CrewID,
		Type:      "clip",
		ActorID:   userID,
		Timestamp: time.Now().UnixMilli(),
		Score:     50,
		Data: ClipData{
			ClipID:           req.ClipID,
			ClipType:         req.ClipType,
			ClipperName:      username,
			DurationSeconds:  req.DurationSeconds,
			Participants:     req.Participants,
			ParticipantNames: participantNames,
			Game:             req.Game,
			LocalPath:        req.LocalPath,
		},
	}

	if err := AppendCrewEvent(ctx, nk, req.CrewID, event); err != nil {
		logger.Error("Failed to append clip event: %v", err)
		return "", runtime.NewError("failed to save clip", 13)
	}

	logger.Info("User %s (%s) clipped in crew %s: clip_id=%s type=%s",
		userID, username, req.CrewID, req.ClipID, req.ClipType)

	resp, _ := json.Marshal(map[string]interface{}{
		"success":  true,
		"event_id": eventID,
		"clip_id":  req.ClipID,
	})
	return string(resp), nil
}

// ---------------------------------------------------------------------------
// CrewTimeline RPC — paginated feed of all crew events (newest first)
// ---------------------------------------------------------------------------

type TimelineRequest struct {
	CrewID string `json:"crew_id"`
	Cursor string `json:"cursor,omitempty"`
	Limit  int    `json:"limit,omitempty"`
}

type TimelineEntry struct {
	ID      string      `json:"id"`
	Type    string      `json:"type"`
	ActorID string      `json:"actor_id"`
	Ts      int64       `json:"ts"`
	Score   int         `json:"score"`
	Data    interface{} `json:"data"`
}

type TimelineResponse struct {
	CrewID  string          `json:"crew_id"`
	Entries []TimelineEntry `json:"entries"`
	Cursor  string          `json:"cursor,omitempty"`
	HasMore bool            `json:"has_more"`
}

func CrewTimelineRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req TimelineRequest
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

	ledger, _ := readLedger(ctx, nk, req.CrewID)

	// Sort by timestamp descending (newest first)
	sort.Slice(ledger.Events, func(i, j int) bool {
		return ledger.Events[i].Timestamp > ledger.Events[j].Timestamp
	})

	// Cursor-based pagination: cursor is the event ID of the last item on the previous page
	startIdx := 0
	if req.Cursor != "" {
		for i, e := range ledger.Events {
			if e.ID == req.Cursor {
				startIdx = i + 1
				break
			}
		}
	}

	end := startIdx + limit
	if end > len(ledger.Events) {
		end = len(ledger.Events)
	}

	page := ledger.Events[startIdx:end]
	entries := make([]TimelineEntry, 0, len(page))
	for _, e := range page {
		entries = append(entries, TimelineEntry{
			ID:      e.ID,
			Type:    e.Type,
			ActorID: e.ActorID,
			Ts:      e.Timestamp,
			Score:   e.Score,
			Data:    e.Data,
		})
	}

	var cursor string
	hasMore := end < len(ledger.Events)
	if hasMore && len(page) > 0 {
		cursor = page[len(page)-1].ID
	}

	// Update last_seen for the user
	updateLastSeen(ctx, nk, userID, req.CrewID)

	resp := TimelineResponse{
		CrewID:  req.CrewID,
		Entries: entries,
		Cursor:  cursor,
		HasMore: hasMore,
	}
	data, _ := json.Marshal(resp)
	return string(data), nil
}

// ---------------------------------------------------------------------------
// ClipUploadURL RPC — return a presigned PUT URL for direct R2/S3 upload
// ---------------------------------------------------------------------------

type ClipUploadURLRequest struct {
	ClipID string `json:"clip_id"`
	CrewID string `json:"crew_id"`
}

type ClipUploadURLResponse struct {
	UploadURL string `json:"upload_url"`
	MediaURL  string `json:"media_url"`
}

func ClipUploadURLRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	_, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req ClipUploadURLRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.ClipID == "" || req.CrewID == "" {
		return "", runtime.NewError("clip_id and crew_id required", 3)
	}

	if !S3IsConfigured() {
		logger.Warn("clip_upload_url: S3 not configured, returning empty URLs")
		resp := ClipUploadURLResponse{}
		data, _ := json.Marshal(resp)
		return string(data), nil
	}

	key := fmt.Sprintf("crews/%s/%s.mp4", req.CrewID, req.ClipID)
	uploadURL, err := GeneratePresignedPUT(key, "audio/mp4", 15*time.Minute)
	if err != nil {
		logger.Error("clip_upload_url: presign failed: %v", err)
		return "", runtime.NewError("failed to generate upload URL", 13)
	}

	mediaURL := S3PublicURL(key)

	resp := ClipUploadURLResponse{
		UploadURL: uploadURL,
		MediaURL:  mediaURL,
	}
	data, _ := json.Marshal(resp)
	return string(data), nil
}

// ---------------------------------------------------------------------------
// ClipUploadComplete RPC — set media_url on clip event after successful upload
// ---------------------------------------------------------------------------

type ClipUploadCompleteRequest struct {
	ClipID string `json:"clip_id"`
	CrewID string `json:"crew_id"`
}

func ClipUploadCompleteRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req ClipUploadCompleteRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.ClipID == "" || req.CrewID == "" {
		return "", runtime.NewError("clip_id and crew_id required", 3)
	}

	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	key := fmt.Sprintf("crews/%s/%s.mp4", req.CrewID, req.ClipID)
	mediaURL := S3PublicURL(key)

	ledger, version := readLedger(ctx, nk, req.CrewID)

	found := false
	for i, e := range ledger.Events {
		if e.Type != "clip" {
			continue
		}
		dataBytes, _ := json.Marshal(e.Data)
		var cd ClipData
		if json.Unmarshal(dataBytes, &cd) == nil && cd.ClipID == req.ClipID {
			cd.MediaURL = mediaURL
			ledger.Events[i].Data = cd
			found = true
			break
		}
	}

	if !found {
		return "", runtime.NewError("clip not found in ledger", 5)
	}

	if err := writeLedger(ctx, nk, req.CrewID, ledger, version); err != nil {
		logger.Error("clip_upload_complete: writeLedger failed: %v", err)
		return "", runtime.NewError("failed to update ledger", 13)
	}

	logger.Info("Clip upload complete: crew=%s clip=%s media_url=%s", req.CrewID, req.ClipID, mediaURL)
	resp, _ := json.Marshal(map[string]interface{}{
		"success":   true,
		"media_url": mediaURL,
	})
	return string(resp), nil
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
		case "clip":
			clipCount++
			actorClips[e.ActorID]++
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

	// Store as a crew event
	event := CrewEvent{
		ID:        generateEventID(),
		CrewID:    crewID,
		Type:      "weekly_recap",
		ActorID:   "",
		Timestamp: time.Now().UnixMilli(),
		Score:     30,
		Data:      recap,
	}
	if err := AppendCrewEvent(ctx, nk, crewID, event); err != nil {
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
