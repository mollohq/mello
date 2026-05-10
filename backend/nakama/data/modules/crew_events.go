package main

import (
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/json"
	"fmt"
	"math/big"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const (
	CrewEventsCollection   = "crew_events"
	CrewLastSeenCollection = "crew_last_seen"

	EventRollingWindowDays = 7
	CatchupThresholdMs     = 4 * 60 * 60 * 1000 // 4 hours
	CatchupTopN            = 3
	MomentMaxTextLen       = 140
	MomentMaxGameNameLen   = 100
	MomentMaxPerUserPerDay = 10
	ChatActivityThreshold  = 10
)

// ---------------------------------------------------------------------------
// Event types and data structs
// ---------------------------------------------------------------------------

type CrewEvent struct {
	ID        string      `json:"id"`
	CrewID    string      `json:"crew_id"`
	Type      string      `json:"type"`
	ActorID   string      `json:"actor_id"`
	Timestamp int64       `json:"ts"`
	Data      interface{} `json:"data"`
	Score     int         `json:"score"`
}

type CrewEventLedger struct {
	CrewID    string      `json:"crew_id"`
	Events    []CrewEvent `json:"events"`
	UpdatedAt int64       `json:"updated_at"`
}

type VoiceSessionData struct {
	ChannelID        string   `json:"channel_id"`
	ChannelName      string   `json:"channel_name"`
	ParticipantIDs   []string `json:"participant_ids"`
	ParticipantNames []string `json:"participant_names"`
	DurationMin      int      `json:"duration_min"`
	PeakCount        int      `json:"peak_count"`
}

type StreamSessionData struct {
	SessionID    string   `json:"session_id,omitempty"`
	StreamerID   string   `json:"streamer_id"`
	StreamerName string   `json:"streamer_name"`
	Title        string   `json:"title"`
	Game         string   `json:"game,omitempty"`
	DurationMin  int      `json:"duration_min"`
	PeakViewers  int      `json:"peak_viewers"`
	ViewerIDs    []string `json:"viewer_ids,omitempty"`
	SnapshotURLs []string `json:"snapshot_urls"` // empty if none captured yet
}

type GameSessionData struct {
	GameName    string   `json:"game_name"`
	GameIGDBID  int      `json:"game_igdb_id"`
	PlayerIDs   []string `json:"player_ids"`
	PlayerNames []string `json:"player_names"`
	DurationMin int      `json:"duration_min"`
}

type MemberJoinedData struct {
	Username    string `json:"username"`
	DisplayName string `json:"display_name"`
}

type MemberLeftData struct {
	Username    string `json:"username"`
	DisplayName string `json:"display_name"`
}

type ChatActivityData struct {
	MessageCount    int   `json:"message_count"`
	WindowStart     int64 `json:"window_start"`
	WindowEnd       int64 `json:"window_end"`
	ActiveUserCount int   `json:"active_user_count"`
}

type MomentData struct {
	Text      string `json:"text"`
	Sentiment string `json:"sentiment"`
	GameName  string `json:"game_name"`
}

// ---------------------------------------------------------------------------
// Time-sortable event ID (timestamp hex + random suffix, no external dep)
// ---------------------------------------------------------------------------

func generateEventID() string {
	now := time.Now().UnixMilli()
	b := make([]byte, 5)
	rand.Read(b)
	return fmt.Sprintf("%013x%x", now, b)
}

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

func readLedger(ctx context.Context, nk runtime.NakamaModule, crewID string) (*CrewEventLedger, string) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: CrewEventsCollection, Key: crewID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return &CrewEventLedger{CrewID: crewID, Events: []CrewEvent{}}, ""
	}

	var ledger CrewEventLedger
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &ledger); err != nil {
		return &CrewEventLedger{CrewID: crewID, Events: []CrewEvent{}}, ""
	}
	return &ledger, objects[0].GetVersion()
}

func writeLedger(ctx context.Context, nk runtime.NakamaModule, crewID string, ledger *CrewEventLedger, version string) error {
	data, err := json.Marshal(ledger)
	if err != nil {
		return err
	}
	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      CrewEventsCollection,
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

func decodeStreamSessionData(raw interface{}) (StreamSessionData, error) {
	var dataBytes []byte
	switch v := raw.(type) {
	case []byte:
		dataBytes = v
	case map[string]interface{}:
		var err error
		dataBytes, err = json.Marshal(v)
		if err != nil {
			return StreamSessionData{}, fmt.Errorf("marshal event data: %w", err)
		}
	default:
		return StreamSessionData{}, fmt.Errorf("unexpected Data type %T", raw)
	}

	var data StreamSessionData
	if err := json.Unmarshal(dataBytes, &data); err != nil {
		return StreamSessionData{}, fmt.Errorf("unmarshal event data: %w", err)
	}
	return data, nil
}

func streamSessionDataToObject(data StreamSessionData) (map[string]interface{}, error) {
	encoded, err := json.Marshal(data)
	if err != nil {
		return nil, fmt.Errorf("marshal updated data: %w", err)
	}
	var obj map[string]interface{}
	if err := json.Unmarshal(encoded, &obj); err != nil {
		return nil, fmt.Errorf("unmarshal updated data: %w", err)
	}
	return obj, nil
}

// UpdateLedgerEventSnapshotURLs finds a stream_session event by eventID and updates its SnapshotURLs.
// Used by the background backfill job when SFU uploads frames after StopStreamRPC completed.
func UpdateLedgerEventSnapshotURLs(ctx context.Context, nk runtime.NakamaModule, crewID, eventID string, snapshotURLs []string) error {
	for attempt := 0; attempt < 3; attempt++ {
		ledger, version := readLedger(ctx, nk, crewID)

		found := false
		for i, e := range ledger.Events {
			if e.ID == eventID {
				data, err := decodeStreamSessionData(e.Data)
				if err != nil {
					return err
				}
				data.SnapshotURLs = snapshotURLs
				updatedObj, err := streamSessionDataToObject(data)
				if err != nil {
					return err
				}
				ledger.Events[i].Data = updatedObj
				found = true
				break
			}
		}
		if !found {
			return fmt.Errorf("event %s not found in ledger", eventID)
		}

		ledger.UpdatedAt = time.Now().UnixMilli()
		err := writeLedger(ctx, nk, crewID, ledger, version)
		if err == nil {
			return nil
		}
		jitter, _ := rand.Int(rand.Reader, big.NewInt(50))
		time.Sleep(time.Duration(50*(attempt+1)+int(jitter.Int64())) * time.Millisecond)
	}
	return fmt.Errorf("UpdateLedgerEventSnapshotURLs failed after 3 retries for crew %s event %s", crewID, eventID)
}

// AppendCrewEvent appends an event to a crew's ledger with optimistic concurrency retry.
func AppendCrewEvent(ctx context.Context, nk runtime.NakamaModule, crewID string, event CrewEvent) error {
	for attempt := 0; attempt < 3; attempt++ {
		ledger, version := readLedger(ctx, nk, crewID)

		ledger.Events = append(ledger.Events, event)

		// Trim events older than the rolling window
		cutoff := time.Now().Add(-time.Duration(EventRollingWindowDays) * 24 * time.Hour).UnixMilli()
		trimmed := make([]CrewEvent, 0, len(ledger.Events))
		for _, e := range ledger.Events {
			if e.Timestamp >= cutoff {
				trimmed = append(trimmed, e)
			}
		}
		ledger.Events = trimmed
		ledger.UpdatedAt = time.Now().UnixMilli()

		err := writeLedger(ctx, nk, crewID, ledger, version)
		if err == nil {
			return nil
		}

		jitter, _ := rand.Int(rand.Reader, big.NewInt(50))
		time.Sleep(time.Duration(50*(attempt+1)+int(jitter.Int64())) * time.Millisecond)
	}
	return fmt.Errorf("crew_events write failed after 3 retries for crew %s", crewID)
}

// ---------------------------------------------------------------------------
// Last-seen tracking
// ---------------------------------------------------------------------------

type crewLastSeen struct {
	CrewID   string `json:"crew_id"`
	LastSeen int64  `json:"last_seen"`
}

func readLastSeen(ctx context.Context, nk runtime.NakamaModule, userID, crewID string) int64 {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: CrewLastSeenCollection, Key: crewID, UserID: userID},
	})
	if err != nil || len(objects) == 0 {
		return 0
	}
	var ls crewLastSeen
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &ls); err != nil {
		return 0
	}
	return ls.LastSeen
}

func updateLastSeen(ctx context.Context, nk runtime.NakamaModule, userID, crewID string) {
	ls := crewLastSeen{CrewID: crewID, LastSeen: time.Now().UnixMilli()}
	data, _ := json.Marshal(ls)
	nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      CrewLastSeenCollection,
			Key:             crewID,
			UserID:          userID,
			Value:           string(data),
			PermissionRead:  1,
			PermissionWrite: 1,
		},
	})
}

// ---------------------------------------------------------------------------
// Catch-up RPC
// ---------------------------------------------------------------------------

type CatchupRequest struct {
	CrewID   string `json:"crew_id"`
	LastSeen int64  `json:"last_seen"`
}

type CatchupResponse struct {
	CrewID      string         `json:"crew_id"`
	CatchupText string        `json:"catchup_text"`
	EventCount  int           `json:"event_count"`
	TopEvents   []CatchupEvent `json:"top_events"`
	HasEvents   bool          `json:"has_events"`
}

type CatchupEvent struct {
	Type    string      `json:"type"`
	ActorID string      `json:"actor_id"`
	Ts      int64       `json:"ts"`
	Data    interface{} `json:"data"`
}

const quietCatchupText = "All quiet, crew's been chilling. Nothing new since you left."

func CrewCatchupRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req CatchupRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}

	lastSeen := req.LastSeen
	if lastSeen == 0 {
		lastSeen = readLastSeen(ctx, nk, userID, req.CrewID)
	}

	// If last_seen is recent (< threshold), return quiet state
	now := time.Now().UnixMilli()
	if lastSeen > 0 && (now-lastSeen) < int64(CatchupThresholdMs) {
		resp := CatchupResponse{
			CrewID:      req.CrewID,
			CatchupText: quietCatchupText,
			TopEvents:   []CatchupEvent{},
			HasEvents:   false,
		}
		data, _ := json.Marshal(resp)
		return string(data), nil
	}

	ledger, _ := readLedger(ctx, nk, req.CrewID)
	resp := buildCatchup(ledger.Events, lastSeen, req.CrewID, ctx, nk)
	data, _ := json.Marshal(resp)
	return string(data), nil
}

func buildCatchup(events []CrewEvent, lastSeen int64, crewID string, ctx context.Context, nk runtime.NakamaModule) CatchupResponse {
	recent := filterAfter(events, lastSeen)
	if len(recent) == 0 {
		return CatchupResponse{
			CrewID:      crewID,
			CatchupText: quietCatchupText,
			TopEvents:   []CatchupEvent{},
			HasEvents:   false,
		}
	}

	// Sort by score desc, then timestamp desc
	sort.Slice(recent, func(i, j int) bool {
		if recent[i].Score != recent[j].Score {
			return recent[i].Score > recent[j].Score
		}
		return recent[i].Timestamp > recent[j].Timestamp
	})

	selected := selectDiverse(recent, CatchupTopN)
	text := renderCatchupText(selected, ctx, nk)

	topEvents := make([]CatchupEvent, 0, len(selected))
	for _, e := range selected {
		topEvents = append(topEvents, CatchupEvent{
			Type:    e.Type,
			ActorID: e.ActorID,
			Ts:      e.Timestamp,
			Data:    e.Data,
		})
	}

	return CatchupResponse{
		CrewID:      crewID,
		CatchupText: text,
		EventCount:  len(recent),
		TopEvents:   topEvents,
		HasEvents:   true,
	}
}

func filterAfter(events []CrewEvent, lastSeen int64) []CrewEvent {
	if lastSeen == 0 {
		return events
	}
	result := make([]CrewEvent, 0)
	for _, e := range events {
		if e.Timestamp > lastSeen {
			result = append(result, e)
		}
	}
	return result
}

// selectDiverse picks top N events preferring type diversity.
func selectDiverse(events []CrewEvent, n int) []CrewEvent {
	if len(events) <= n {
		return events
	}

	selected := make([]CrewEvent, 0, n)
	usedTypes := map[string]bool{}

	for _, e := range events {
		if len(selected) >= n {
			break
		}
		if !usedTypes[e.Type] {
			selected = append(selected, e)
			usedTypes[e.Type] = true
		}
	}

	// Fill remaining slots if not enough unique types
	if len(selected) < n {
		taken := map[string]bool{}
		for _, s := range selected {
			taken[s.ID] = true
		}
		for _, e := range events {
			if len(selected) >= n {
				break
			}
			if !taken[e.ID] {
				selected = append(selected, e)
				taken[e.ID] = true
			}
		}
	}

	return selected
}

// ---------------------------------------------------------------------------
// Template rendering
// ---------------------------------------------------------------------------

func renderCatchupText(events []CrewEvent, ctx context.Context, nk runtime.NakamaModule) string {
	fragments := make([]string, 0, len(events))
	for _, e := range events {
		if f := renderEventFragment(e, ctx, nk); f != "" {
			fragments = append(fragments, f)
		}
	}
	return joinWithAnd(fragments)
}

func renderEventFragment(e CrewEvent, ctx context.Context, nk runtime.NakamaModule) string {
	dataBytes, _ := json.Marshal(e.Data)

	switch e.Type {
	case "moment":
		var d MomentData
		json.Unmarshal(dataBytes, &d)
		name := resolveUsername(ctx, nk, e.ActorID)
		if name == "" {
			name = "someone"
		}
		if d.Text != "" {
			return fmt.Sprintf("%s: %s", name, d.Text)
		}
		switch d.Sentiment {
		case "win":
			return fmt.Sprintf("%s had a win in %s", name, d.GameName)
		case "loss":
			return fmt.Sprintf("%s took an L in %s", name, d.GameName)
		case "highlight":
			return fmt.Sprintf("%s had a moment in %s", name, d.GameName)
		}
		return ""

	case "voice_session":
		var d VoiceSessionData
		json.Unmarshal(dataBytes, &d)
		return fmt.Sprintf("%s hung out in %s for %dm", joinNamesList(d.ParticipantNames, 3), d.ChannelName, d.DurationMin)

	case "stream_session":
		var d StreamSessionData
		json.Unmarshal(dataBytes, &d)
		return fmt.Sprintf("%s streamed %s for %dm", d.StreamerName, d.Title, d.DurationMin)

	case "game_session":
		var d GameSessionData
		json.Unmarshal(dataBytes, &d)
		return fmt.Sprintf("%s played %s", joinNamesList(d.PlayerNames, 3), d.GameName)

	case "member_joined":
		var d MemberJoinedData
		json.Unmarshal(dataBytes, &d)
		return fmt.Sprintf("%s joined the crew", d.DisplayName)

	case "member_left":
		var d MemberLeftData
		json.Unmarshal(dataBytes, &d)
		return fmt.Sprintf("%s left the crew", d.DisplayName)

	case "chat_activity":
		var d ChatActivityData
		json.Unmarshal(dataBytes, &d)
		return fmt.Sprintf("%d messages from %d people in chat", d.MessageCount, d.ActiveUserCount)

	case "clip":
		var d ClipData
		json.Unmarshal(dataBytes, &d)
		name := resolveUsername(ctx, nk, e.ActorID)
		if name == "" {
			name = "someone"
		}
		return fmt.Sprintf("%s clipped that (%.0fs %s)", name, d.DurationSeconds, d.ClipType)

	case "weekly_recap":
		return "weekly recap is ready"
	}

	return ""
}

func joinNamesList(names []string, max int) string {
	if len(names) == 0 {
		return "someone"
	}
	if len(names) <= max {
		return joinWithAnd(names)
	}
	shown := names[:max]
	extra := len(names) - max
	return strings.Join(shown, ", ") + fmt.Sprintf(" and %d others", extra)
}

func joinWithAnd(parts []string) string {
	switch len(parts) {
	case 0:
		return ""
	case 1:
		return parts[0]
	case 2:
		return parts[0] + " and " + parts[1]
	default:
		return strings.Join(parts[:len(parts)-1], ", ") + ", and " + parts[len(parts)-1]
	}
}

// ---------------------------------------------------------------------------
// Post Moment RPC
// ---------------------------------------------------------------------------

type PostMomentRequest struct {
	CrewID    string `json:"crew_id"`
	Sentiment string `json:"sentiment"`
	Text      string `json:"text"`
	GameName  string `json:"game_name"`
}

func PostMomentRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req PostMomentRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	if req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	switch req.Sentiment {
	case "win", "loss", "highlight":
	default:
		return "", runtime.NewError("sentiment must be win, loss, or highlight", 3)
	}
	if len(req.Text) > MomentMaxTextLen {
		return "", runtime.NewError(fmt.Sprintf("text max %d characters", MomentMaxTextLen), 3)
	}
	if len(req.GameName) > MomentMaxGameNameLen {
		return "", runtime.NewError(fmt.Sprintf("game_name max %d characters", MomentMaxGameNameLen), 3)
	}

	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	// Rate limit check
	ledger, _ := readLedger(ctx, nk, req.CrewID)
	dayStart := time.Now().Truncate(24 * time.Hour).UnixMilli()
	momentCount := 0
	for _, e := range ledger.Events {
		if e.Type == "moment" && e.ActorID == userID && e.Timestamp >= dayStart {
			momentCount++
		}
	}
	if momentCount >= MomentMaxPerUserPerDay {
		return "", runtime.NewError("moment rate limit exceeded (max 10 per day)", 8)
	}

	score := 25
	if req.Text != "" {
		score = 40
	}

	eventID := generateEventID()
	event := CrewEvent{
		ID:        eventID,
		CrewID:    req.CrewID,
		Type:      "moment",
		ActorID:   userID,
		Timestamp: time.Now().UnixMilli(),
		Score:     score,
		Data: MomentData{
			Text:      req.Text,
			Sentiment: req.Sentiment,
			GameName:  req.GameName,
		},
	}

	if err := AppendCrewEvent(ctx, nk, req.CrewID, event); err != nil {
		logger.Error("Failed to append moment event: %v", err)
		return "", runtime.NewError("failed to save moment", 13)
	}

	logger.Info("User %s posted moment in crew %s: sentiment=%s", userID, req.CrewID, req.Sentiment)
	resp, _ := json.Marshal(map[string]interface{}{"success": true, "event_id": eventID})
	return string(resp), nil
}

// ---------------------------------------------------------------------------
// Game Session End RPC
// ---------------------------------------------------------------------------

type GameSessionEndRequest struct {
	CrewID      string `json:"crew_id"`
	GameName    string `json:"game_name"`
	DurationMin int    `json:"duration_min"`
}

func GameSessionEndRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req GameSessionEndRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" || req.GameName == "" {
		return "", runtime.NewError("crew_id and game_name required", 3)
	}

	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	username := resolveUsername(ctx, nk, userID)

	event := CrewEvent{
		ID:        generateEventID(),
		CrewID:    req.CrewID,
		Type:      "game_session",
		ActorID:   userID,
		Timestamp: time.Now().UnixMilli(),
		Score:     10,
		Data: GameSessionData{
			GameName:    req.GameName,
			GameIGDBID:  0,
			PlayerIDs:   []string{userID},
			PlayerNames: []string{username},
			DurationMin: req.DurationMin,
		},
	}

	if err := AppendCrewEvent(ctx, nk, req.CrewID, event); err != nil {
		logger.Error("Failed to append game_session event: %v", err)
		return "", runtime.NewError("failed to save game session", 13)
	}

	logger.Info("User %s game session end in crew %s: game=%s duration=%dm", userID, req.CrewID, req.GameName, req.DurationMin)
	resp, _ := json.Marshal(map[string]interface{}{"success": true})
	return string(resp), nil
}

// ---------------------------------------------------------------------------
// Chat activity aggregator
// ---------------------------------------------------------------------------

type chatWindow struct {
	count       int
	userSet     map[string]bool
	windowStart int64
}

var chatCounters = struct {
	sync.Mutex
	counts map[string]*chatWindow
}{counts: make(map[string]*chatWindow)}

func incrementChatCounter(crewID, userID string) {
	chatCounters.Lock()
	defer chatCounters.Unlock()

	w, ok := chatCounters.counts[crewID]
	if !ok {
		w = &chatWindow{
			userSet:     make(map[string]bool),
			windowStart: time.Now().UnixMilli(),
		}
		chatCounters.counts[crewID] = w
	}
	w.count++
	w.userSet[userID] = true
}

func flushChatCounters(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger) {
	chatCounters.Lock()
	snapshot := chatCounters.counts
	chatCounters.counts = make(map[string]*chatWindow)
	chatCounters.Unlock()

	for crewID, w := range snapshot {
		if w.count < ChatActivityThreshold {
			continue
		}
		event := CrewEvent{
			ID:        generateEventID(),
			CrewID:    crewID,
			Type:      "chat_activity",
			ActorID:   "",
			Timestamp: time.Now().UnixMilli(),
			Score:     5,
			Data: ChatActivityData{
				MessageCount:    w.count,
				WindowStart:     w.windowStart,
				WindowEnd:       time.Now().UnixMilli(),
				ActiveUserCount: len(w.userSet),
			},
		}
		if err := AppendCrewEvent(ctx, nk, crewID, event); err != nil {
			logger.Warn("Failed to flush chat_activity for crew %s: %v", crewID, err)
		}
	}
}

func startChatActivityTicker(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, interval time.Duration) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			flushChatCounters(ctx, nk, logger)
		}
	}
}

// ---------------------------------------------------------------------------
// Voice session tracking (in-memory, called from voice_state.go)
// ---------------------------------------------------------------------------

type voiceSessionInfo struct {
	channelName  string
	crewID       string
	startTime    time.Time
	participants map[string]string // userID -> username
	peakCount    int
}

var (
	voiceSessions   = make(map[string]*voiceSessionInfo) // channelID -> session
	voiceSessionsMu sync.Mutex
)

// voiceSessionOnJoin tracks a user joining a voice channel for session event generation.
func voiceSessionOnJoin(channelID, crewID, channelName, userID, username string) {
	voiceSessionsMu.Lock()
	defer voiceSessionsMu.Unlock()

	sess, ok := voiceSessions[channelID]
	if !ok {
		sess = &voiceSessionInfo{
			channelName:  channelName,
			crewID:       crewID,
			startTime:    time.Now(),
			participants: make(map[string]string),
		}
		voiceSessions[channelID] = sess
	}
	sess.participants[userID] = username
	if len(sess.participants) > sess.peakCount {
		sess.peakCount = len(sess.participants)
	}
}

// voiceSessionOnLastLeave returns session info when the last member leaves
// a channel that had 2+ unique participants. Returns nil otherwise.
func voiceSessionOnLastLeave(channelID string) *voiceSessionInfo {
	voiceSessionsMu.Lock()
	defer voiceSessionsMu.Unlock()

	sess, ok := voiceSessions[channelID]
	if !ok {
		return nil
	}
	delete(voiceSessions, channelID)

	if len(sess.participants) < 2 {
		return nil
	}
	return sess
}
