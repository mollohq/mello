package main

import (
	"context"
	"encoding/json"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// Crew quests: the first steps of a new crew. One system-owned record per
// crew, made with the crew. A step is written once, by the server, when it
// happens. The quest card shows on the feed for questsWindowDays; the steps
// have no end, so a late step still counts.

const (
	CrewQuestsCollection = "crew_quests"
	// questsWindowDays is how long the quest card shows on the feed.
	questsWindowDays = 14
	// questsNamesMax caps the names a step keeps.
	questsNamesMax = 8
)

// Step keys, in order. Crew made is the record itself (MadeMs).
const (
	questStepFriend  = "friend"
	questStepCall    = "call"
	questStepClip    = "clip"
	questStepSession = "session"
)

// CrewQuests is the record of crew_quests/{crew_id}.
type CrewQuests struct {
	CrewID  string          `json:"crew_id"`
	OwnerID string          `json:"owner_id"`
	MadeMs  int64           `json:"made_ms"`
	EndsMs  int64           `json:"ends_ms"`
	Steps   CrewQuestsSteps `json:"steps"`
	// PlusGranted lists the members who got m3llo+ from this crew's step 5.
	PlusGranted []string `json:"plus_granted,omitempty"`
	// InviteCopiedMs is the first copy of an invite link, for the funnel.
	InviteCopiedMs int64 `json:"invite_copied_ms,omitempty"`
}

// CrewQuestsSteps holds the four steps after crew made. A nil step is not done.
type CrewQuestsSteps struct {
	Friend  *QuestFriend  `json:"friend,omitempty"`
	Call    *QuestCall    `json:"call,omitempty"`
	Clip    *QuestClip    `json:"clip,omitempty"`
	Session *QuestSession `json:"session,omitempty"`
}

// QuestFriend is the first member to join who is not the owner.
type QuestFriend struct {
	DoneMs        int64  `json:"done_ms"`
	UserID        string `json:"user_id"`
	Name          string `json:"name"`
	InvitedBy     string `json:"invited_by,omitempty"`
	InvitedByName string `json:"invited_by_name,omitempty"`
}

// QuestCall is the first time two or more people were in one voice room
// together, marked on the join of the second. A browser guest counts. Guests
// have no name here, only a count.
type QuestCall struct {
	DoneMs      int64    `json:"done_ms"`
	UserIDs     []string `json:"user_ids"`
	Names       []string `json:"names"`
	Guests      int      `json:"guests,omitempty"`
	ChannelID   string   `json:"channel_id"`
	ChannelName string   `json:"channel_name,omitempty"`
}

// QuestClip is the first clip posted to the crew, voice or game. The poster
// and the media are in crew_clips, found by ClipID.
type QuestClip struct {
	DoneMs      int64  `json:"done_ms"`
	ClipID      string `json:"clip_id"`
	ClipType    string `json:"clip_type"`
	ClipperID   string `json:"clipper_id"`
	ClipperName string `json:"clipper_name"`
	Game        string `json:"game,omitempty"`
}

// QuestSession is the first game session of the crew.
type QuestSession struct {
	DoneMs     int64    `json:"done_ms"`
	GameID     string   `json:"game_id,omitempty"`
	GameName   string   `json:"game_name,omitempty"`
	GameIGDBID int      `json:"game_igdb_id,omitempty"`
	Minutes    int      `json:"minutes"`
	UserIDs    []string `json:"user_ids"`
	Names      []string `json:"names"`
}

// ZoneQuests is the quests part of the zones payload. The client shows the
// card ahead of the server's Stage, as it does a live stream, and decides when
// the card goes: the opened state of the gifts is on the device.
type ZoneQuests struct {
	OwnerID string          `json:"owner_id"`
	MadeMs  int64           `json:"made_ms"`
	EndsMs  int64           `json:"ends_ms"`
	Steps   CrewQuestsSteps `json:"steps"`
	Gifts   ZoneQuestGifts  `json:"gifts"`
}

// zoneQuests is the payload for a record, or nil when the crew has no record
// or the window of the card has passed.
func zoneQuests(q *CrewQuests, nowMs int64, gifts ZoneQuestGifts) *ZoneQuests {
	if q == nil || nowMs >= q.EndsMs {
		return nil
	}
	return &ZoneQuests{OwnerID: q.OwnerID, MadeMs: q.MadeMs, EndsMs: q.EndsMs, Steps: q.Steps, Gifts: gifts}
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

// readCrewQuests returns the record and its version, or nil when the crew has
// none (a crew made before quests shipped).
func readCrewQuests(ctx context.Context, nk runtime.NakamaModule, crewID string) (*CrewQuests, string) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: CrewQuestsCollection, Key: crewID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return nil, ""
	}
	var q CrewQuests
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &q); err != nil {
		return nil, ""
	}
	return &q, objects[0].GetVersion()
}

// writeCrewQuests writes the record. Version "*" writes only when there is no
// record yet; a read version writes only over that version.
func writeCrewQuests(ctx context.Context, nk runtime.NakamaModule, q *CrewQuests, version string) error {
	data, err := json.Marshal(q)
	if err != nil {
		return err
	}
	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{{
		Collection:      CrewQuestsCollection,
		Key:             q.CrewID,
		UserID:          SystemUserID,
		Value:           string(data),
		Version:         version,
		PermissionRead:  0,
		PermissionWrite: 0,
	}})
	return err
}

// newCrewQuests is the record of a crew made at madeMs.
func newCrewQuests(crewID, ownerID string, madeMs int64) *CrewQuests {
	return &CrewQuests{
		CrewID:  crewID,
		OwnerID: ownerID,
		MadeMs:  madeMs,
		EndsMs:  madeMs + int64(questsWindowDays)*dayMs,
	}
}

// questsCreate makes the record of a new crew. Crew made is done by the
// record itself.
func questsCreate(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID, ownerID string) {
	q := newCrewQuests(crewID, ownerID, time.Now().UnixMilli())
	if err := writeCrewQuests(ctx, nk, q, "*"); err != nil {
		logger.Warn("crew_quests create failed for crew %s: %v", crewID, err)
		return
	}
	logger.Info("crew_quests created for crew %s", crewID)
}

// questsMark sets one step when the crew has a record and the step is not
// done. set returns false when it has nothing to write (the step is done).
// It retries on a version conflict, as the ledger does. It reports whether
// it wrote. After a write it tells the members with the crew open that the
// feed changed: the quest card is in the feed, and no other event of a step
// reloads it (a member join sends none).
func questsMark(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID, step string, set func(*CrewQuests) bool) bool {
	for attempt := 0; attempt < 3; attempt++ {
		q, version := readCrewQuests(ctx, nk, crewID)
		if q == nil || !set(q) {
			return false
		}
		err := writeCrewQuests(ctx, nk, q, version)
		if err == nil {
			logger.Info("crew_quests %s written for crew %s", step, crewID)
			PushFeedChanged(ctx, logger, nk, crewID, "quest")
			return true
		}
		retryWait(attempt)
	}
	logger.Warn("crew_quests %s write failed after 3 retries for crew %s", step, crewID)
	return false
}

// ---------------------------------------------------------------------------
// The step setters. Each one is pure, so every rule is testable without
// Nakama. Each returns false when the step is done or the event does not count.
// ---------------------------------------------------------------------------

// setQuestFriend marks the first join of a member who is not the owner.
func setQuestFriend(s *CrewQuestsSteps, ownerID string, f QuestFriend) bool {
	if s.Friend != nil || f.UserID == "" || f.UserID == ownerID {
		return false
	}
	s.Friend = &f
	return true
}

// setQuestCall marks the first call.
func setQuestCall(s *CrewQuestsSteps, c QuestCall) bool {
	if s.Call != nil {
		return false
	}
	s.Call = &c
	return true
}

// setQuestClip marks the first clip.
func setQuestClip(s *CrewQuestsSteps, c QuestClip) bool {
	if s.Clip != nil || c.ClipID == "" {
		return false
	}
	s.Clip = &c
	return true
}

// setQuestSession marks the first game session.
func setQuestSession(s *CrewQuestsSteps, gs QuestSession) bool {
	if s.Session != nil {
		return false
	}
	s.Session = &gs
	return true
}

// capNames keeps the first questsNamesMax ids and names, in step.
func capNames(ids, names []string) ([]string, []string) {
	if len(ids) > questsNamesMax {
		ids = ids[:questsNamesMax]
	}
	if len(names) > questsNamesMax {
		names = names[:questsNamesMax]
	}
	return ids, names
}

// ---------------------------------------------------------------------------
// The writers, called where the events happen.
// ---------------------------------------------------------------------------

// questsMarkFriend is called on every member_joined.
func questsMarkFriend(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID string, f QuestFriend) {
	f.DoneMs = time.Now().UnixMilli()
	questsMark(ctx, logger, nk, crewID, questStepFriend, func(q *CrewQuests) bool {
		return setQuestFriend(&q.Steps, q.OwnerID, f)
	})
}

// questsMarkCall is called on a voice join that puts a second person in a
// room (VoiceRoom.questCall).
func questsMarkCall(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID string, call QuestCall) {
	if crewID == "" {
		return
	}
	call.UserIDs, call.Names = capNames(call.UserIDs, call.Names)
	questsMark(ctx, logger, nk, crewID, questStepCall, func(q *CrewQuests) bool {
		return setQuestCall(&q.Steps, call)
	})
}

// questsMarkClip is called when a new clip is stored.
func questsMarkClip(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID string, clip StoredClip) {
	c := QuestClip{
		DoneMs:      clip.Ts,
		ClipID:      clip.ClipID,
		ClipType:    clip.ClipType,
		ClipperID:   clip.ActorID,
		ClipperName: clip.ClipperName,
		Game:        clip.Game,
	}
	questsMark(ctx, logger, nk, crewID, questStepClip, func(q *CrewQuests) bool {
		return setQuestClip(&q.Steps, c)
	})
}

// questsMarkSession is called when a game_session event is stored.
func questsMarkSession(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID string, doneMs int64, data GameSessionData) {
	ids, names := capNames(data.PlayerIDs, data.PlayerNames)
	gs := QuestSession{
		DoneMs:     doneMs,
		GameID:     data.GameID,
		GameName:   data.GameName,
		GameIGDBID: data.GameIGDBID,
		Minutes:    data.DurationMin,
		UserIDs:    ids,
		Names:      names,
	}
	if questsMark(ctx, logger, nk, crewID, questStepSession, func(q *CrewQuests) bool {
		return setQuestSession(&q.Steps, gs)
	}) {
		// Gift 5: m3llo+ for every member at this moment.
		questsGrantPlus(ctx, logger, nk, crewID)
	}
}
