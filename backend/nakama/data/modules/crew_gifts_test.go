package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"strings"
	"testing"
	"time"

	"github.com/heroiclabs/nakama-common/api"
	"github.com/heroiclabs/nakama-common/runtime"
	"google.golang.org/protobuf/types/known/wrapperspb"
)

// Roles for the gift tests: the fake answers the group_edge state, so the
// owner checks see 0 for the superadmin and 2 for a member.
func (f *fakeQuestsNk) setRole(crewID, userID string, state int) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.roles == nil {
		f.roles = map[string]int{}
	}
	f.roles[crewID+"/"+userID] = state
}

func (f *fakeQuestsNk) role(crewID, userID string) int {
	f.mu.Lock()
	defer f.mu.Unlock()
	if s, ok := f.roles[crewID+"/"+userID]; ok {
		return s
	}
	return 2
}

func (f *fakeQuestsNk) UserGroupsList(_ context.Context, userID string, _ int, _ *int, _ string) ([]*api.UserGroupList_UserGroup, string, error) {
	var out []*api.UserGroupList_UserGroup
	for crew, ms := range f.members {
		for _, m := range ms {
			if m.GetUser().GetId() == userID {
				out = append(out, &api.UserGroupList_UserGroup{
					Group: &api.Group{Id: crew},
					State: wrapperspb.Int32(int32(f.role(crew, userID))),
				})
			}
		}
	}
	return out, "", nil
}

func (f *fakeQuestsNk) GroupUsersList(_ context.Context, id string, _ int, _ *int, _ string) ([]*api.GroupUserList_GroupUser, string, error) {
	var out []*api.GroupUserList_GroupUser
	for _, m := range f.members[id] {
		out = append(out, &api.GroupUserList_GroupUser{User: m.GetUser(), State: wrapperspb.Int32(int32(f.role(id, m.GetUser().GetId())))})
	}
	return out, "", nil
}

// giftsCrew is a crew "gc" with the owner "own" and the member "ida", a quest
// record made now, and a media bucket at media.test.
func giftsCrew(t *testing.T) *fakeQuestsNk {
	t.Helper()
	old := mediaURLFor
	mediaURLFor = func(k string) string { return "https://media.test/" + k }
	t.Cleanup(func() { mediaURLFor = old })
	nk := newFakeQuestsNk()
	nk.addMember("gc", "own", "b0bben")
	nk.addMember("gc", "ida", "Ida")
	nk.setRole("gc", "own", 0)
	questsCreate(context.Background(), testLogger(), nk, "gc", "own")
	return nk
}

func rpcT(t *testing.T, fn func(context.Context, runtime.Logger, *sql.DB, runtime.NakamaModule, string) (string, error), nk runtime.NakamaModule, user, payload string) (string, error) {
	t.Helper()
	return fn(ctxWithUser(user), testLogger(), nil, nk, payload)
}

func markStep(t *testing.T, nk *fakeQuestsNk, crewID string, set func(*CrewQuests)) {
	t.Helper()
	ctx := context.Background()
	q, version := readCrewQuests(ctx, nk, crewID)
	if q == nil {
		t.Fatalf("crew %s has no record", crewID)
	}
	set(q)
	if err := writeCrewQuests(ctx, nk, q, version); err != nil {
		t.Fatal(err)
	}
}

func TestGiftOpenRules(t *testing.T) {
	none := (*CrewQuests)(nil)
	fresh := newCrewQuests("c", "own", 1)
	done := newCrewQuests("c", "own", 1)
	done.Steps = CrewQuestsSteps{Friend: &QuestFriend{}, Call: &QuestCall{}, Clip: &QuestClip{}, Session: &QuestSession{}}
	cases := []struct {
		q    *CrewQuests
		gift string
		want bool
	}{
		{none, giftBanner, true}, {none, giftLink, true}, {none, giftEmoji, true}, {none, giftWeek, false}, {none, giftPlus, false},
		{fresh, giftBanner, true}, {fresh, giftLink, false}, {fresh, giftEmoji, false}, {fresh, giftWeek, false}, {fresh, giftPlus, false},
		{done, giftLink, true}, {done, giftEmoji, true}, {done, giftWeek, true}, {done, giftPlus, true},
	}
	for _, c := range cases {
		if got := giftOpen(c.q, c.gift); got != c.want {
			t.Errorf("giftOpen(record=%v, %s) = %v, want %v", c.q != nil, c.gift, got, c.want)
		}
	}
}

// ---------------------------------------------------------------------------
// Gift 1: banner
// ---------------------------------------------------------------------------

func TestBannerGameArtIsTheWorkerHero(t *testing.T) {
	nk := giftsCrew(t)
	if _, err := rpcT(t, SetCrewBannerRPC, nk, "own", `{"crew_id":"gc","kind":"game","game_id":"steam-291550"}`); err != nil {
		t.Fatal(err)
	}
	p, _ := readCrewProfile(context.Background(), nk, "gc")
	if p.Banner == nil || p.Banner.URL != "https://art.m3llo.app/steam-291550/hero" || p.Banner.Version == 0 {
		t.Fatalf("banner = %+v", p.Banner)
	}
}

func TestBannerIsOwnerOnly(t *testing.T) {
	nk := giftsCrew(t)
	_, err := rpcT(t, SetCrewBannerRPC, nk, "ida", `{"crew_id":"gc","kind":"game","game_id":"steam-291550"}`)
	if errCode(err) != 7 {
		t.Fatalf("a member set the banner: %v", err)
	}
}

func TestBannerUploadTakesOnlyThisCrewsKey(t *testing.T) {
	nk := giftsCrew(t)
	for _, key := range []string{"crew-banners/other/abc.jpg", "crew-banners/gc/x/abc.jpg", "crew-banners/gc/../abc.jpg", "crew-banners/gc/abc.png", "gc/own/abc.jpg"} {
		_, err := rpcT(t, SetCrewBannerRPC, nk, "own", `{"crew_id":"gc","kind":"upload","key":"`+key+`"}`)
		if errCode(err) != 3 {
			t.Errorf("key %q: err = %v, want invalid", key, err)
		}
	}
	if _, err := rpcT(t, SetCrewBannerRPC, nk, "own", `{"crew_id":"gc","kind":"upload","key":"crew-banners/gc/abc.jpg"}`); err != nil {
		t.Fatal(err)
	}
	p, _ := readCrewProfile(context.Background(), nk, "gc")
	if p.Banner == nil || p.Banner.URL != "https://media.test/crew-banners/gc/abc.jpg" {
		t.Fatalf("banner = %+v", p.Banner)
	}
}

func TestBannerShowsInCrewStateAndTheInvite(t *testing.T) {
	nk := giftsCrew(t)
	if _, err := rpcT(t, SetCrewBannerRPC, nk, "own", `{"crew_id":"gc","kind":"game","game_id":"igdb-1020"}`); err != nil {
		t.Fatal(err)
	}
	if _, err := rpcT(t, ClearCrewBannerRPC, nk, "ida", `{"crew_id":"gc"}`); errCode(err) != 7 {
		t.Fatalf("a member cleared the banner: %v", err)
	}
	p, _ := readCrewProfile(context.Background(), nk, "gc")
	if p.Banner == nil {
		t.Fatal("the banner is gone")
	}
	if _, err := rpcT(t, ClearCrewBannerRPC, nk, "own", `{"crew_id":"gc"}`); err != nil {
		t.Fatal(err)
	}
	if p, _ := readCrewProfile(context.Background(), nk, "gc"); p.Banner != nil {
		t.Fatalf("banner = %+v after clear", p.Banner)
	}
}

// ---------------------------------------------------------------------------
// Gift 2: invite link
// ---------------------------------------------------------------------------

func TestInviteNameRules(t *testing.T) {
	for name, want := range map[string]bool{
		"brawlbros": true, "bob-test-26": true, "abc": true, strings.Repeat("a", 24): true,
		"ab": false, "-abc": false, "abc-": false, "a--b": false, "admin": false, "m3llo": false,
		strings.Repeat("a", 25): false, "a b c": false, "abc_d": false,
		"brawlbro": false, "abcd-1234": false, "ab-cd1234": true,
	} {
		if got := inviteNameAllowed(name); got != want {
			t.Errorf("inviteNameAllowed(%q) = %v, want %v", name, got, want)
		}
	}
	if normalizeInviteName("  BrawlBros ") != "brawlbros" {
		t.Fatal("a name is lower case")
	}
}

func TestInviteNameNeedsAFriendFirst(t *testing.T) {
	nk := giftsCrew(t)
	_, err := rpcT(t, SetInviteNameRPC, nk, "own", `{"crew_id":"gc","name":"brawlbros"}`)
	if errCode(err) != 9 {
		t.Fatalf("set before step 2: %v, want gift not unlocked", err)
	}
	markStep(t, nk, "gc", func(q *CrewQuests) { q.Steps.Friend = &QuestFriend{UserID: "ida"} })
	if _, err := rpcT(t, SetInviteNameRPC, nk, "ida", `{"crew_id":"gc","name":"brawlbros"}`); errCode(err) != 7 {
		t.Fatalf("a member set the link: %v", err)
	}
	if _, err := rpcT(t, SetInviteNameRPC, nk, "own", `{"crew_id":"gc","name":"BrawlBros"}`); err != nil {
		t.Fatal(err)
	}
	crewID, _, err := lookupInviteCode(context.Background(), nk, normalizeInviteCode("brawlbros"))
	if err != nil || crewID != "gc" {
		t.Fatalf("the name resolves to %q, %v", crewID, err)
	}
	if p, _ := readCrewProfile(context.Background(), nk, "gc"); p.InviteName != "brawlbros" {
		t.Fatalf("profile name = %q", p.InviteName)
	}
}

func TestInviteNameTakenByAnotherCrew(t *testing.T) {
	nk := giftsCrew(t)
	markStep(t, nk, "gc", func(q *CrewQuests) { q.Steps.Friend = &QuestFriend{UserID: "ida"} })
	if _, err := rpcT(t, SetInviteNameRPC, nk, "own", `{"crew_id":"gc","name":"squad"}`); err != nil {
		t.Fatal(err)
	}
	nk.addMember("other", "max", "Max")
	nk.setRole("other", "max", 0)
	out, _ := rpcT(t, CheckInviteNameRPC, nk, "max", `{"crew_id":"other","name":"SQUAD"}`)
	if !strings.Contains(out, `"taken"`) {
		t.Fatalf("check = %s, want taken", out)
	}
	// A crew with no record (made before quests) has the link gift open.
	if _, err := rpcT(t, SetInviteNameRPC, nk, "max", `{"crew_id":"other","name":"squad"}`); errCode(err) != 6 {
		t.Fatalf("another crew took the name: %v", err)
	}
	if _, err := rpcT(t, SetInviteNameRPC, nk, "max", `{"crew_id":"other","name":"maxcrew"}`); err != nil {
		t.Fatalf("a crew with no record gets the link: %v", err)
	}
}

func TestInviteNameOldNameIsHeldThenFreed(t *testing.T) {
	nk := giftsCrew(t)
	ctx := context.Background()
	markStep(t, nk, "gc", func(q *CrewQuests) { q.Steps.Friend = &QuestFriend{UserID: "ida"} })
	for _, n := range []string{"first", "second"} {
		if _, err := rpcT(t, SetInviteNameRPC, nk, "own", `{"crew_id":"gc","name":"`+n+`"}`); err != nil {
			t.Fatal(err)
		}
	}
	if crewID, _, _ := lookupInviteCode(ctx, nk, "FIRST"); crewID != "gc" {
		t.Fatalf("the old name leads to %q, want the crew while held", crewID)
	}
	nk.addMember("other", "max", "Max")
	nk.setRole("other", "max", 0)
	if state, _ := inviteNameState(ctx, nk, "other", "first", time.Now().UnixMilli()); state != "taken" {
		t.Fatalf("held name state = %s, want taken", state)
	}
	if state, _ := inviteNameState(ctx, nk, "other", "first", time.Now().UnixMilli()+inviteNameHoldMs+1); state != "free" {
		t.Fatalf("name after the hold = %s, want free", state)
	}
	if state, _ := inviteNameState(ctx, nk, "other", "second", time.Now().UnixMilli()+inviteNameHoldMs+1); state != "taken" {
		t.Fatalf("the crew's current name = %s, want taken", state)
	}
}

func TestRandomCodesSkipACustomName(t *testing.T) {
	nk := giftsCrew(t)
	markStep(t, nk, "gc", func(q *CrewQuests) { q.Steps.Friend = &QuestFriend{UserID: "ida"} })
	if _, err := rpcT(t, SetInviteNameRPC, nk, "own", `{"crew_id":"gc","name":"abcd-efgh"}`); errCode(err) != 3 {
		t.Fatalf("a name in the shape of a code: %v", err)
	}
	// A taken key of any kind is skipped: put one in the way.
	nk.put(InviteCodeCollection, "ABCD-EFGH", SystemUserID, inviteCodeObject{CrewID: "gc", Custom: true})
	for i := 0; i < 50; i++ {
		code, err := generateCode(context.Background(), nk)
		if err != nil || code == "ABCD-EFGH" {
			t.Fatalf("generateCode = %q, %v", code, err)
		}
	}
}

// ---------------------------------------------------------------------------
// Gift 3: crew emoji
// ---------------------------------------------------------------------------

func addEmoji(t *testing.T, nk *fakeQuestsNk, user, name string) (string, error) {
	t.Helper()
	return rpcT(t, CrewEmojiAddRPC, nk, user, `{"crew_id":"gc","name":"`+name+`","key":"crew-emoji/gc/`+name+`.png"}`)
}

func TestCrewEmojiNeedsTheFirstCall(t *testing.T) {
	nk := giftsCrew(t)
	if _, err := addEmoji(t, nk, "ida", "gg"); errCode(err) != 9 {
		t.Fatalf("emoji before step 3: %v", err)
	}
	markStep(t, nk, "gc", func(q *CrewQuests) { q.Steps.Call = &QuestCall{} })
	if _, err := addEmoji(t, nk, "ida", "gg"); err != nil {
		t.Fatalf("any member adds an emoji: %v", err)
	}
	d, _ := readCrewEmoji(context.Background(), nk, "gc")
	if len(d.Emoji) != 1 || d.Emoji[0].AddedBy != "ida" || d.Emoji[0].URL != "https://media.test/crew-emoji/gc/gg.png" {
		t.Fatalf("emoji = %+v", d.Emoji)
	}
}

func TestCrewEmojiFiveSlotsAndUniqueNames(t *testing.T) {
	nk := giftsCrew(t)
	markStep(t, nk, "gc", func(q *CrewQuests) { q.Steps.Call = &QuestCall{} })
	if _, err := addEmoji(t, nk, "own", "gg"); err != nil {
		t.Fatal(err)
	}
	if _, err := addEmoji(t, nk, "ida", "GG"); errCode(err) != 6 {
		t.Fatalf("a second gg: %v", err)
	}
	for _, n := range []string{"a1", "a2", "a3", "a4"} {
		if _, err := addEmoji(t, nk, "ida", n); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := addEmoji(t, nk, "ida", "a5"); errCode(err) != 9 {
		t.Fatalf("a sixth emoji: %v", err)
	}
	if _, err := rpcT(t, CrewEmojiAddRPC, nk, "own", `{"crew_id":"gc","name":"x!","key":"crew-emoji/gc/x.png"}`); errCode(err) != 3 {
		t.Fatalf("a bad name: %v", err)
	}
}

func TestCrewEmojiRemoveByTheAdderOrTheOwner(t *testing.T) {
	nk := giftsCrew(t)
	nk.addMember("gc", "elin", "Elin")
	markStep(t, nk, "gc", func(q *CrewQuests) { q.Steps.Call = &QuestCall{} })
	for _, n := range []string{"one", "two"} {
		if _, err := addEmoji(t, nk, "ida", n); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := rpcT(t, CrewEmojiRemoveRPC, nk, "elin", `{"crew_id":"gc","name":"one"}`); errCode(err) != 7 {
		t.Fatalf("another member removed it: %v", err)
	}
	if _, err := rpcT(t, CrewEmojiRemoveRPC, nk, "ida", `{"crew_id":"gc","name":"one"}`); err != nil {
		t.Fatal(err)
	}
	if _, err := rpcT(t, CrewEmojiRemoveRPC, nk, "own", `{"crew_id":"gc","name":"two"}`); err != nil {
		t.Fatal(err)
	}
	if d, _ := readCrewEmoji(context.Background(), nk, "gc"); len(d.Emoji) != 0 {
		t.Fatalf("emoji = %+v", d.Emoji)
	}
}

func TestReactionTakesACrewEmoji(t *testing.T) {
	nk := giftsCrew(t)
	markStep(t, nk, "gc", func(q *CrewQuests) { q.Steps.Call = &QuestCall{} })
	react := func(emoji, op string) error {
		_, err := rpcT(t, ChatReactRPC, nk, "ida", `{"crew_id":"gc","message_id":"m1","emoji":"`+emoji+`","op":"`+op+`"}`)
		return err
	}
	if err := react(":gg:", "add"); errCode(err) != 5 {
		t.Fatalf("a crew emoji that does not exist: %v", err)
	}
	if _, err := addEmoji(t, nk, "own", "gg"); err != nil {
		t.Fatal(err)
	}
	if err := react(":gg:", "add"); err != nil {
		t.Fatalf("react with a crew emoji: %v", err)
	}
	if err := react(":gg", "add"); errCode(err) != 3 {
		t.Fatalf("text is still refused: %v", err)
	}
	doc, _ := readReactionsDoc(context.Background(), nk, "m1")
	if len(doc.Reactions[":gg:"]) != 1 {
		t.Fatalf("reactions = %+v", doc.Reactions)
	}
	if _, err := rpcT(t, CrewEmojiRemoveRPC, nk, "own", `{"crew_id":"gc","name":"gg"}`); err != nil {
		t.Fatal(err)
	}
	if err := react(":gg:", "remove"); err != nil {
		t.Fatalf("a reaction of a removed emoji can be taken back: %v", err)
	}
}

// ---------------------------------------------------------------------------
// Gift 4: founding week
// ---------------------------------------------------------------------------

func TestFoundingWeekOpensAfterTheFirstClip(t *testing.T) {
	in := zInput()
	// The crew was made on Thursday 22 October, two weeks before zonesNow.
	made := zonesNow - 14*dayMs
	in.Quests = newCrewQuests("crew", "own", made)
	in.Clips = []StoredClip{zClip("old", clipTypeGame, 13, true)}
	week := weekStartOf(made)
	find := func(z FeedZones) *ZoneHistoryWeek {
		for i := range z.History {
			if z.History[i].WeekStart == week {
				return &z.History[i]
			}
		}
		t.Fatalf("no history week %d in %+v", week, z.History)
		return nil
	}
	if w := find(buildFeedZones(in)); !w.Locked || w.Founding {
		t.Fatalf("before the clip step: %+v", w)
	}
	in.Quests.Steps.Clip = &QuestClip{ClipID: "old"}
	w := find(buildFeedZones(in))
	if w.Locked || !w.Founding || len(w.Items) == 0 || len(w.Teasers) != 0 {
		t.Fatalf("founding week = %+v, want open with its items", w)
	}
}

func TestFoundingWeeksSpanTwoWeeksLateInAWeek(t *testing.T) {
	// Made on a Saturday: the first 7 days end the next Saturday.
	saturday := weekStartOf(zonesNow) + 5*dayMs
	q := newCrewQuests("c", "own", saturday)
	q.Steps.Clip = &QuestClip{ClipID: "c"}
	if got := foundingWeeks(q); len(got) != 2 || got[1]-got[0] != 7*dayMs {
		t.Fatalf("founding weeks = %v, want two", got)
	}
	monday := weekStartOf(zonesNow)
	q = newCrewQuests("c", "own", monday)
	q.Steps.Clip = &QuestClip{ClipID: "c"}
	if got := foundingWeeks(q); len(got) != 1 {
		t.Fatalf("made on a Monday: %v, want one week", got)
	}
}

// ---------------------------------------------------------------------------
// Gift 5: m3llo+
// ---------------------------------------------------------------------------

func TestFirstSessionGrantsPlusToEveryMember(t *testing.T) {
	nk := giftsCrew(t)
	ctx := context.Background()
	if IsUserPremium(ctx, nk, "ida") {
		t.Fatal("no m3llo+ before step 5")
	}
	questsMarkSession(ctx, testLogger(), nk, "gc", 1, GameSessionData{GameID: "g", PlayerIDs: []string{"own"}, PlayerNames: []string{"b0bben"}})
	for _, u := range []string{"own", "ida"} {
		if !IsUserPremium(ctx, nk, u) {
			t.Fatalf("%s has no m3llo+ after step 5", u)
		}
		p, _ := readUserPlus(ctx, nk, u)
		if days := (p.UntilMs - p.QuestGrantMs) / dayMs; days != questPlusDays || p.Source != "quest" || p.QuestCrewID != "gc" {
			t.Fatalf("%s plus = %+v", u, p)
		}
	}
	if q := readQuestsT(t, nk, "gc"); len(q.PlusGranted) != 2 {
		t.Fatalf("plus_granted = %v", q.PlusGranted)
	}
}

func TestPlusFromQuestsOncePerUser(t *testing.T) {
	nk := giftsCrew(t)
	ctx := context.Background()
	questsMarkSession(ctx, testLogger(), nk, "gc", 1, GameSessionData{GameID: "g"})
	first, _ := readUserPlus(ctx, nk, "ida")
	// Ida is in a second new crew that finishes its quests later.
	nk.addMember("gc2", "ida", "Ida")
	nk.addMember("gc2", "max", "Max")
	questsCreate(ctx, testLogger(), nk, "gc2", "max")
	questsMarkSession(ctx, testLogger(), nk, "gc2", 2, GameSessionData{GameID: "g"})
	again, _ := readUserPlus(ctx, nk, "ida")
	if again.UntilMs != first.UntilMs || again.QuestCrewID != "gc" {
		t.Fatalf("a second quest grant: %+v, was %+v", again, first)
	}
	if q := readQuestsT(t, nk, "gc2"); len(q.PlusGranted) != 1 || q.PlusGranted[0] != "max" {
		t.Fatalf("gc2 plus_granted = %v, want only max", q.PlusGranted)
	}
}

func TestPlusOpensCrewHistoryInTheFeed(t *testing.T) {
	nk := giftsCrew(t)
	ctx := context.Background()
	questsMarkSession(ctx, testLogger(), nk, "gc", 1, GameSessionData{GameID: "g"})
	z := crewFeedZones(ctx, testLogger(), nk, FeedRequest{CrewID: "gc"}, "ida", nil, nil, nil, time.Now().UnixMilli())
	if !z.Plus {
		t.Fatal("the feed reads m3llo+ from user_plus")
	}
}

// ---------------------------------------------------------------------------
// The payload, reports, invite copied
// ---------------------------------------------------------------------------

func TestZonesQuestsCarryTheGifts(t *testing.T) {
	nk := giftsCrew(t)
	ctx := context.Background()
	markStep(t, nk, "gc", func(q *CrewQuests) {
		q.Steps.Friend = &QuestFriend{UserID: "ida"}
		q.Steps.Call = &QuestCall{}
	})
	for _, p := range []struct {
		fn      func(context.Context, runtime.Logger, *sql.DB, runtime.NakamaModule, string) (string, error)
		payload string
	}{
		{SetCrewBannerRPC, `{"crew_id":"gc","kind":"game","game_id":"steam-291550"}`},
		{SetInviteNameRPC, `{"crew_id":"gc","name":"brawlbros"}`},
		{CrewEmojiAddRPC, `{"crew_id":"gc","name":"gg","key":"crew-emoji/gc/gg.png"}`},
	} {
		if _, err := rpcT(t, p.fn, nk, "own", p.payload); err != nil {
			t.Fatal(err)
		}
	}
	z := crewFeedZones(ctx, testLogger(), nk, FeedRequest{CrewID: "gc"}, "own", nil, nil, nil, time.Now().UnixMilli())
	g := z.Quests.Gifts
	if g.BannerURL == "" || g.BannerGameID != "steam-291550" || g.InviteName != "brawlbros" || g.EmojiCount != 1 || g.FirstEmojiURL == "" {
		t.Fatalf("gifts = %+v", g)
	}
	data, _ := json.Marshal(z.Quests)
	for _, k := range []string{`"gifts"`, `"banner_url"`, `"invite_name"`, `"emoji_count"`} {
		if !strings.Contains(string(data), k) {
			t.Fatalf("quests json has no %s: %s", k, data)
		}
	}
}

func TestReportCrewMediaStoresAReport(t *testing.T) {
	nk := giftsCrew(t)
	if _, err := rpcT(t, ReportCrewMediaRPC, nk, "max", `{"crew_id":"gc","kind":"banner"}`); errCode(err) != 5 {
		t.Fatalf("a report with no uploaded banner: %v", err)
	}
	if _, err := rpcT(t, SetCrewBannerRPC, nk, "own", `{"crew_id":"gc","kind":"upload","key":"crew-banners/gc/b.jpg"}`); err != nil {
		t.Fatal(err)
	}
	// The join page reports through the http key, with no user.
	if _, err := ReportCrewMediaRPC(context.Background(), testLogger(), nil, nk, `{"crew_id":"gc","kind":"banner"}`); err != nil {
		t.Fatal(err)
	}
	n := 0
	for k, v := range nk.storage {
		if strings.HasPrefix(k, MediaReportsCollection+"/") {
			n++
			if !strings.Contains(v, "crew-banners/gc/b.jpg") {
				t.Fatalf("report = %s", v)
			}
		}
	}
	if n != 1 {
		t.Fatalf("reports = %d, want 1", n)
	}
}

func TestInviteCopiedIsSetOnce(t *testing.T) {
	nk := giftsCrew(t)
	if _, err := rpcT(t, ReportInviteCopiedRPC, nk, "max", `{"crew_id":"gc"}`); errCode(err) != 7 {
		t.Fatalf("a stranger: %v", err)
	}
	if _, err := rpcT(t, ReportInviteCopiedRPC, nk, "own", `{"crew_id":"gc"}`); err != nil {
		t.Fatal(err)
	}
	first := readQuestsT(t, nk, "gc").InviteCopiedMs
	time.Sleep(2 * time.Millisecond)
	if _, err := rpcT(t, ReportInviteCopiedRPC, nk, "ida", `{"crew_id":"gc"}`); err != nil {
		t.Fatal(err)
	}
	if first == 0 || readQuestsT(t, nk, "gc").InviteCopiedMs != first {
		t.Fatalf("invite_copied_ms = %d then %d", first, readQuestsT(t, nk, "gc").InviteCopiedMs)
	}
}

func TestPlusSkipsAJoinRequest(t *testing.T) {
	nk := giftsCrew(t)
	nk.addMember("gc", "asker", "Asker")
	nk.setRole("gc", "asker", 3) // a join request, not a member yet
	questsMarkSession(context.Background(), testLogger(), nk, "gc", 1, GameSessionData{GameID: "g"})
	if IsUserPremium(context.Background(), nk, "asker") {
		t.Fatal("a join request is not a member: no m3llo+")
	}
}

func TestCrewStateAndInviteCarryTheGifts(t *testing.T) {
	nk := giftsCrew(t)
	ctx := context.Background()
	nk.groups["gc"] = &api.Group{Id: "gc", Name: "bob_test_26"}
	markStep(t, nk, "gc", func(q *CrewQuests) { q.Steps.Call = &QuestCall{} })
	if _, err := rpcT(t, SetCrewBannerRPC, nk, "own", `{"crew_id":"gc","kind":"game","game_id":"steam-291550"}`); err != nil {
		t.Fatal(err)
	}
	if _, err := addEmoji(t, nk, "ida", "gg"); err != nil {
		t.Fatal(err)
	}
	InvalidateCrewState("gc")
	st, err := ComputeCrewState(ctx, testLogger(), nk, "gc", true, "own")
	if err != nil {
		t.Fatal(err)
	}
	if st.Banner == nil || st.Banner.GameID != "steam-291550" || len(st.Emoji) != 1 || st.Emoji[0].Name != "gg" {
		t.Fatalf("crew_state banner %+v emoji %+v", st.Banner, st.Emoji)
	}
	code, err := GenerateInviteCode(ctx, nk, testLogger(), "gc", "own")
	if err != nil {
		t.Fatal(err)
	}
	out, err := ResolveCrewInviteRPC(ctx, testLogger(), nil, nk, `{"code":"`+code+`"}`)
	if err != nil {
		t.Fatal(err)
	}
	var resp ResolveCrewInviteResponse
	if err := json.Unmarshal([]byte(out), &resp); err != nil {
		t.Fatal(err)
	}
	if resp.BannerURL != "https://art.m3llo.app/steam-291550/hero" || resp.BannerVersion == 0 {
		t.Fatalf("invite banner = %q %d", resp.BannerURL, resp.BannerVersion)
	}
}

// ChannelMessagesList has no chat in the gift tests.
func (f *fakeQuestsNk) ChannelMessagesList(_ context.Context, _ string, _ int, _ bool, _ string) ([]*api.ChannelMessage, string, string, error) {
	return nil, "", "", nil
}

// A member adds an emoji. The owner has the crew open and gets a crew event,
// so the client loads crew state with the new emoji (seen in a local journey,
// 2026-11-07). A member with the crew only in the sidebar gets none: the
// gifts draw only in the open crew.
func TestAGiftChangeTellsTheOpenCrewOnly(t *testing.T) {
	nk := giftsCrew(t)
	markStep(t, nk, "gc", func(q *CrewQuests) { q.Steps.Call = &QuestCall{DoneMs: 1} })
	openCrew("own", "gc")
	getOrCreateSubscription("side") // a client with gc in its sidebar only
	addCrewSubscriber("gc", "side")
	t.Cleanup(func() { removeCrewSubscriber("gc", "own"); removeCrewSubscriber("gc", "side") })
	feedNotesMu.Lock()
	feedNotes = nil
	feedNotesMu.Unlock()
	if _, err := addEmoji(t, nk, "ida", "gg"); err != nil {
		t.Fatal(err)
	}
	events := func(user string) []sentNote {
		feedNotesMu.Lock()
		defer feedNotesMu.Unlock()
		var out []sentNote
		for _, n := range feedNotes {
			if n.userID == user && n.code == NotifyCrewEvent {
				out = append(out, n)
			}
		}
		return out
	}
	got := events("own")
	if len(got) != 1 || got[0].content["event"] != "crew_gifts" || got[0].content["crew_id"] != "gc" {
		t.Fatalf("crew events to the owner = %+v, want one crew_gifts", got)
	}
	if n := len(events("side")); n != 0 {
		t.Fatalf("a sidebar subscriber got %d crew events", n)
	}
}
