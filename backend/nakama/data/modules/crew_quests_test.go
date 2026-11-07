package main

import (
	"context"
	"encoding/json"
	"fmt"
	"strconv"
	"testing"
	"time"

	"github.com/heroiclabs/nakama-common/api"
	"github.com/heroiclabs/nakama-common/runtime"
)

// fakeQuestsNk is fakeGuestNk with storage versions: a read returns the
// version, a write with "*" fails when the object exists, and a write with a
// version fails when the object moved on. The quest writers depend on both.
type fakeQuestsNk struct {
	*fakeGuestNk
	versions map[string]int
	roles    map[string]int // crew/user -> group_edge state, 2 when unset
}

func newFakeQuestsNk() *fakeQuestsNk {
	return &fakeQuestsNk{fakeGuestNk: newFakeGuestNk(), versions: map[string]int{}}
}

func (f *fakeQuestsNk) StorageRead(ctx context.Context, reads []*runtime.StorageRead) ([]*api.StorageObject, error) {
	out, err := f.fakeGuestNk.StorageRead(ctx, reads)
	f.mu.Lock()
	defer f.mu.Unlock()
	for _, o := range out {
		o.Version = strconv.Itoa(f.versions[fakeStorageKey(o.Collection, o.Key, o.UserId)])
	}
	return out, err
}

func (f *fakeQuestsNk) StorageWrite(ctx context.Context, writes []*runtime.StorageWrite) ([]*api.StorageObjectAck, error) {
	f.mu.Lock()
	for _, w := range writes {
		k := fakeStorageKey(w.Collection, w.Key, w.UserID)
		_, exists := f.storage[k]
		if w.Version == "*" && exists {
			f.mu.Unlock()
			return nil, fmt.Errorf("storage write rejected: %s exists", k)
		}
		if w.Version != "" && w.Version != "*" && w.Version != strconv.Itoa(f.versions[k]) {
			f.mu.Unlock()
			return nil, fmt.Errorf("storage write rejected: version check failed for %s", k)
		}
		f.versions[k]++
	}
	f.mu.Unlock()
	return f.fakeGuestNk.StorageWrite(ctx, writes)
}

func readQuestsT(t *testing.T, nk runtime.NakamaModule, crewID string) *CrewQuests {
	t.Helper()
	q, _ := readCrewQuests(context.Background(), nk, crewID)
	if q == nil {
		t.Fatalf("crew %s has no quest record", crewID)
	}
	return q
}

// joinT seats a participant with the real join path.
func joinT(t *testing.T, nk runtime.NakamaModule, crewID, channelID, userID string, guest bool) {
	t.Helper()
	_, err := joinVoiceRoom(context.Background(), testLogger(), nk, voiceJoinParams{
		CrewID: crewID, ChannelID: channelID, ChannelName: "General",
		UserID: userID, Username: userID, MaxMembers: 10, IsGuest: guest,
	})
	if err != nil {
		t.Fatalf("join %s: %v", userID, err)
	}
}

func newVoiceTest(t *testing.T) (*fakeQuestsNk, context.Context, runtime.Logger) {
	t.Helper()
	resetVoiceState()
	resetLedgerSessions()
	t.Cleanup(resetVoiceState)
	nk := newFakeQuestsNk()
	ctx, lg := context.Background(), testLogger()
	questsCreate(ctx, lg, nk, "crew", "own")
	return nk, ctx, lg
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

func TestQuestsCreateWritesTheRecordWithItsWindow(t *testing.T) {
	nk := newFakeQuestsNk()
	before := time.Now().UnixMilli()
	questsCreate(context.Background(), testLogger(), nk, "crew", "own")
	q := readQuestsT(t, nk, "crew")
	if q.OwnerID != "own" || q.MadeMs < before || q.EndsMs != q.MadeMs+14*dayMs {
		t.Fatalf("record = %+v", q)
	}
	if q.Steps.Friend != nil || q.Steps.Call != nil || q.Steps.Clip != nil || q.Steps.Session != nil {
		t.Fatalf("a new record has no step done: %+v", q.Steps)
	}
	if p := nk.perms[fakeStorageKey(CrewQuestsCollection, "crew", SystemUserID)]; p != [2]int{0, 0} {
		t.Fatalf("permissions = %v, want read 0 write 0", p)
	}
}

func TestQuestsCreateDoesNotOverwriteARecord(t *testing.T) {
	nk := newFakeQuestsNk()
	ctx, lg := context.Background(), testLogger()
	questsCreate(ctx, lg, nk, "crew", "own")
	questsMarkFriend(ctx, lg, nk, "crew", QuestFriend{UserID: "ida", Name: "Ida"})
	questsCreate(ctx, lg, nk, "crew", "someone-else")
	q := readQuestsT(t, nk, "crew")
	if q.OwnerID != "own" || q.Steps.Friend == nil {
		t.Fatalf("a second create changed the record: %+v", q)
	}
}

func TestQuestsWriteNothingForACrewWithNoRecord(t *testing.T) {
	nk := newFakeQuestsNk()
	ctx, lg := context.Background(), testLogger()
	questsMarkFriend(ctx, lg, nk, "old-crew", QuestFriend{UserID: "ida"})
	questsMarkClip(ctx, lg, nk, "old-crew", StoredClip{ClipID: "c1", ActorID: "ida"})
	questsMarkSession(ctx, lg, nk, "old-crew", 1, GameSessionData{GameID: "g"})
	if _, ok := nk.storage[fakeStorageKey(CrewQuestsCollection, "old-crew", SystemUserID)]; ok {
		t.Fatal("a crew made before quests shipped gets no record")
	}
}

func TestQuestsLateStepStillCounts(t *testing.T) {
	nk := newFakeQuestsNk()
	ctx, lg := context.Background(), testLogger()
	questsCreate(ctx, lg, nk, "crew", "own")
	q, version := readCrewQuests(ctx, nk, "crew")
	q.EndsMs = time.Now().UnixMilli() - dayMs
	if err := writeCrewQuests(ctx, nk, q, version); err != nil {
		t.Fatal(err)
	}
	questsMarkClip(ctx, lg, nk, "crew", StoredClip{ClipID: "c1", ActorID: "own", Ts: 5})
	if readQuestsT(t, nk, "crew").Steps.Clip == nil {
		t.Fatal("a step after the window still counts: its gift unlocks")
	}
}

// ---------------------------------------------------------------------------
// Step 2: a friend joins
// ---------------------------------------------------------------------------

func TestQuestsFriendIsTheFirstMemberWhoIsNotTheOwner(t *testing.T) {
	nk := newFakeQuestsNk()
	ctx, lg := context.Background(), testLogger()
	questsCreate(ctx, lg, nk, "crew", "own")
	questsMarkFriend(ctx, lg, nk, "crew", QuestFriend{UserID: "own", Name: "Owner"})
	if readQuestsT(t, nk, "crew").Steps.Friend != nil {
		t.Fatal("the owner is not a friend")
	}
	questsMarkFriend(ctx, lg, nk, "crew", QuestFriend{UserID: "ida", Name: "Ida"})
	questsMarkFriend(ctx, lg, nk, "crew", QuestFriend{UserID: "elin", Name: "Elin"})
	f := readQuestsT(t, nk, "crew").Steps.Friend
	if f == nil || f.UserID != "ida" || f.DoneMs == 0 {
		t.Fatalf("friend = %+v, want Ida, the first", f)
	}
}

func TestQuestsMemberJoinedMarksTheFriendWithTheInviter(t *testing.T) {
	nk := newFakeQuestsNk()
	nk.addMember("crew", "own", "b0bben")
	nk.addMember("crew", "ida", "Ida")
	ctx, lg := context.Background(), testLogger()
	questsCreate(ctx, lg, nk, "crew", "own")
	recordMemberJoined(ctx, lg, nk, "crew", "ida", "own")
	f := readQuestsT(t, nk, "crew").Steps.Friend
	if f == nil || f.UserID != "ida" || f.Name != "Ida" || f.InvitedBy != "own" || f.InvitedByName != "b0bben" {
		t.Fatalf("friend = %+v", f)
	}
}

// The owner has the crew open when a friend joins. A join sends no event that
// loads the feed again, so without this push the step shows as done only
// after the next crew event (seen in a local test, 2026-11-07).
func TestQuestsStepWriteTellsTheOpenCrewTheFeedChanged(t *testing.T) {
	nk := newFakeQuestsNk()
	nk.addMember("qfc-crew", "qfc-own", "b0bben")
	nk.addMember("qfc-crew", "qfc-ida", "Ida")
	openCrew("qfc-own", "qfc-crew")
	feedNotesMu.Lock()
	feedNotes = nil
	feedNotesMu.Unlock()
	ctx, lg := context.Background(), testLogger()
	questsCreate(ctx, lg, nk, "qfc-crew", "qfc-own")
	recordMemberJoined(ctx, lg, nk, "qfc-crew", "qfc-ida", "qfc-own")
	got := feedChangedTo("qfc-own")
	if len(got) != 1 || got[0].content["crew_id"] != "qfc-crew" || got[0].content["reason"] != "quest" {
		t.Fatalf("feed_changed to the owner = %+v, want one for the quest", got)
	}
	// A step that is done writes nothing and sends nothing.
	recordMemberJoined(ctx, lg, nk, "qfc-crew", "qfc-ida", "qfc-own")
	if n := len(feedChangedTo("qfc-own")); n != 1 {
		t.Fatalf("feed_changed count = %d after a second join, want 1", n)
	}
}

// ---------------------------------------------------------------------------
// Step 3: first call, two people in one voice room
// ---------------------------------------------------------------------------

func TestQuestsCallCountsOnTheJoinOfTheSecondPerson(t *testing.T) {
	nk, _, _ := newVoiceTest(t)
	joinT(t, nk, "crew", "ch", "own", false)
	if c := readQuestsT(t, nk, "crew").Steps.Call; c != nil {
		t.Fatalf("one person in voice is no call: %+v", c)
	}
	joinT(t, nk, "crew", "ch", "ida", false)
	c := readQuestsT(t, nk, "crew").Steps.Call
	if c == nil || c.DoneMs == 0 || c.ChannelID != "ch" || c.ChannelName != "General" || c.Guests != 0 {
		t.Fatalf("call = %+v, want own and ida in General", c)
	}
	if len(c.UserIDs) != 2 || c.UserIDs[0] != "ida" || c.UserIDs[1] != "own" || c.Names[0] != "ida" {
		t.Fatalf("call people = %v %v, want ida and own", c.UserIDs, c.Names)
	}
}

func TestQuestsCallAloneDoesNotCount(t *testing.T) {
	nk, ctx, lg := newVoiceTest(t)
	joinT(t, nk, "crew", "ch", "own", false)
	if !backdateVoiceJoin("own", 30*time.Minute) {
		t.Fatal("own is not in voice")
	}
	voiceLeaveInternal(ctx, lg, nk, "own")
	joinT(t, nk, "crew", "ch", "own", false)
	if c := readQuestsT(t, nk, "crew").Steps.Call; c != nil {
		t.Fatalf("time alone counted as a call: %+v", c)
	}
}

func TestQuestsCallInTwoRoomsDoesNotCount(t *testing.T) {
	nk, _, _ := newVoiceTest(t)
	joinT(t, nk, "crew", "ch", "own", false)
	joinT(t, nk, "crew", "ch2", "ida", false)
	if c := readQuestsT(t, nk, "crew").Steps.Call; c != nil {
		t.Fatalf("two rooms with one person each counted: %+v", c)
	}
}

func TestQuestsCallWithAGuestCountsAndTheGuestHasNoName(t *testing.T) {
	// The owner and one browser guest: no voice_session is written for this
	// call (guests are not ledger participants), but the step counts.
	nk, _, _ := newVoiceTest(t)
	joinT(t, nk, "crew", "ch", "own", false)
	joinT(t, nk, "crew", "ch", "guest:abc", true)
	c := readQuestsT(t, nk, "crew").Steps.Call
	if c == nil || c.Guests != 1 || len(c.UserIDs) != 1 || c.UserIDs[0] != "own" {
		t.Fatalf("call = %+v, want own and 1 guest", c)
	}
	for _, n := range c.Names {
		if n == "guest:abc" {
			t.Fatal("a guest has no name in the record")
		}
	}
}

func TestQuestsCallIsWrittenOnce(t *testing.T) {
	nk, ctx, lg := newVoiceTest(t)
	joinT(t, nk, "crew", "ch", "own", false)
	joinT(t, nk, "crew", "ch", "ida", false)
	voiceLeaveInternal(ctx, lg, nk, "ida")
	joinT(t, nk, "crew", "ch", "elin", false)
	c := readQuestsT(t, nk, "crew").Steps.Call
	if c == nil || len(c.UserIDs) != 2 || c.UserIDs[0] != "ida" {
		t.Fatalf("a done step changed: %+v", c)
	}
}

// ---------------------------------------------------------------------------
// Steps 4 and 5
// ---------------------------------------------------------------------------

func TestQuestsClipIsTheFirstClip(t *testing.T) {
	nk := newFakeQuestsNk()
	ctx, lg := context.Background(), testLogger()
	questsCreate(ctx, lg, nk, "crew", "own")
	questsMarkClip(ctx, lg, nk, "crew", StoredClip{ClipID: "c1", ActorID: "own", ClipperName: "b0bben", ClipType: clipTypeGame, Game: "Brawlhalla", Ts: 10})
	questsMarkClip(ctx, lg, nk, "crew", StoredClip{ClipID: "c2", ActorID: "ida", ClipType: clipTypeVoice, Ts: 20})
	c := readQuestsT(t, nk, "crew").Steps.Clip
	if c == nil || c.ClipID != "c1" || c.ClipperID != "own" || c.Game != "Brawlhalla" || c.DoneMs != 10 {
		t.Fatalf("clip = %+v, want the first clip", c)
	}
}

func TestQuestsSessionIsTheFirstGameSession(t *testing.T) {
	nk := newFakeQuestsNk()
	ctx, lg := context.Background(), testLogger()
	questsCreate(ctx, lg, nk, "crew", "own")
	ids := []string{"own", "ida", "a", "b", "c", "d", "e", "f", "g", "h"}
	questsMarkSession(ctx, lg, nk, "crew", 30, GameSessionData{
		GameID: "brawlhalla", GameName: "Brawlhalla", GameIGDBID: 7, DurationMin: 125,
		PlayerIDs: ids, PlayerNames: ids,
	})
	questsMarkSession(ctx, lg, nk, "crew", 40, GameSessionData{GameID: "cs2", DurationMin: 5})
	s := readQuestsT(t, nk, "crew").Steps.Session
	if s == nil || s.GameID != "brawlhalla" || s.Minutes != 125 || s.DoneMs != 30 || s.GameIGDBID != 7 {
		t.Fatalf("session = %+v, want the first session", s)
	}
	if len(s.UserIDs) != questsNamesMax || len(s.Names) != questsNamesMax {
		t.Fatalf("names = %d, want the cap %d", len(s.Names), questsNamesMax)
	}
}

// ---------------------------------------------------------------------------
// The zones payload
// ---------------------------------------------------------------------------

func TestZonesQuestsShowWhileTheWindowRuns(t *testing.T) {
	in := zInput()
	if z := buildFeedZones(in); z.Quests != nil {
		t.Fatal("a crew with no record has no quests")
	}
	in.Quests = newCrewQuests("crew", "own", zonesNow-13*dayMs)
	in.Quests.Steps.Friend = &QuestFriend{DoneMs: zonesNow - dayMs, UserID: "ida", Name: "Ida"}
	z := buildFeedZones(in)
	if z.Quests == nil || z.Quests.OwnerID != "own" || z.Quests.Steps.Friend == nil {
		t.Fatalf("quests = %+v", z.Quests)
	}
	in.Quests = newCrewQuests("crew", "own", zonesNow-14*dayMs)
	if z := buildFeedZones(in); z.Quests != nil {
		t.Fatal("the card goes when the window ends")
	}
}

func TestZonesQuestsKeepTheStageClipInClips(t *testing.T) {
	// The client shows the quest card ahead of the Stage, so the Stage clip
	// must also be in Clips while quests show. The client takes it out of
	// Clips when it puts the Stage back.
	in := zInput()
	in.Clips = []StoredClip{zClip("hero", clipTypeGame, 1, true)}
	in.Quests = newCrewQuests("crew", "own", zonesNow-dayMs)
	z := buildFeedZones(in)
	if z.Stage.Kind != "game_clip" || z.Stage.Entry.ID != "hero" {
		t.Fatalf("stage = %+v, want the game clip for when the card goes", z.Stage)
	}
	found := false
	for _, id := range entryIDs(z.Clips) {
		found = found || id == "hero"
	}
	if !found {
		t.Fatal("the stage clip is in Clips while quests show")
	}
}

func TestZonesQuestsWireForm(t *testing.T) {
	in := zInput()
	in.Quests = newCrewQuests("crew", "own", zonesNow-dayMs)
	in.Quests.Steps.Call = &QuestCall{DoneMs: 1, UserIDs: []string{"own"}, Names: []string{"b0bben"}, Guests: 1, ChannelID: "ch"}
	data, err := json.Marshal(buildFeedZones(in))
	if err != nil {
		t.Fatal(err)
	}
	var wire struct {
		Quests map[string]json.RawMessage `json:"quests"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		t.Fatal(err)
	}
	for _, k := range []string{"owner_id", "made_ms", "ends_ms", "steps"} {
		if _, ok := wire.Quests[k]; !ok {
			t.Fatalf("quests has no %q: %s", k, data)
		}
	}
	var steps map[string]json.RawMessage
	if err := json.Unmarshal(wire.Quests["steps"], &steps); err != nil {
		t.Fatal(err)
	}
	if _, ok := steps["call"]; !ok || len(steps) != 1 {
		t.Fatalf("steps = %s, want only the done step", wire.Quests["steps"])
	}
}

func TestQuestsPostClipMarksTheClip(t *testing.T) {
	nk := newFakeQuestsNk()
	nk.addMember("q-crew", "q-actor", "actor")
	questsCreate(context.Background(), testLogger(), nk, "q-crew", "q-actor")
	payload := `{"crew_id":"q-crew","clip_id":"c1","clip_type":"voice","duration_seconds":10}`
	if _, err := PostClipRPC(ctxWithUser("q-actor"), testLogger(), nil, nk, payload); err != nil {
		t.Fatal(err)
	}
	c := readQuestsT(t, nk, "q-crew").Steps.Clip
	if c == nil || c.ClipID != "c1" || c.ClipperID != "q-actor" || c.ClipType != clipTypeVoice {
		t.Fatalf("clip = %+v", c)
	}
}

func TestQuestsGameSessionEndMarksTheSession(t *testing.T) {
	nk := newFakeQuestsNk()
	nk.addMember("q-crew", "q-actor", "actor")
	questsCreate(context.Background(), testLogger(), nk, "q-crew", "q-actor")
	payload := `{"crew_id":"q-crew","game_name":"Brawlhalla","game_id":"steam-291550","duration_min":125,"active_min":120}`
	if _, err := GameSessionEndRPC(ctxWithUser("q-actor"), testLogger(), nil, nk, payload); err != nil {
		t.Fatal(err)
	}
	s := readQuestsT(t, nk, "q-crew").Steps.Session
	if s == nil || s.GameName != "Brawlhalla" || s.Minutes != 125 || len(s.UserIDs) != 1 || s.UserIDs[0] != "q-actor" {
		t.Fatalf("session = %+v", s)
	}
}

// GroupCreate makes the group in the fake, so CreateCrewRPC runs.
func (f *fakeQuestsNk) GroupCreate(_ context.Context, userID, name, creatorID, _, description, avatarURL string, open bool, _ map[string]interface{}, maxCount int) (*api.Group, error) {
	g := &api.Group{Id: "g-" + name, Name: name, CreatorId: creatorID, Description: description, AvatarUrl: avatarURL, MaxCount: int32(maxCount)}
	f.groups[g.Id] = g
	f.addMember(g.Id, userID, userID)
	return g, nil
}

func TestQuestsCreateCrewMakesTheRecord(t *testing.T) {
	nk := newFakeQuestsNk()
	out, err := CreateCrewRPC(ctxWithUser("own"), testLogger(), nil, nk, `{"name":"bob_test_26"}`)
	if err != nil {
		t.Fatal(err)
	}
	var resp CreateCrewResponse
	if err := json.Unmarshal([]byte(out), &resp); err != nil {
		t.Fatal(err)
	}
	if q := readQuestsT(t, nk, resp.CrewID); q.OwnerID != "own" {
		t.Fatalf("record = %+v, want owner own", q)
	}
}
