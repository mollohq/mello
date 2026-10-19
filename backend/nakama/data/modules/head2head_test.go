package main

import (
	"context"
	"encoding/json"
	"strings"
	"testing"
	"time"

	"github.com/heroiclabs/nakama-common/api"
)

// h2hNow is Monday 2026-10-19 12:00 UTC.
var h2hNow = time.Date(2026, time.October, 19, 12, 0, 0, 0, time.UTC).UnixMilli()

func ip(v int) *int { return &v }

func dayAgo(n int) string {
	return time.UnixMilli(h2hNow).UTC().AddDate(0, 0, -n).Format("2006-01-02")
}

func hgame(id string, playtimeMin int) LibraryGame {
	return LibraryGame{GameID: id, Name: id, Launcher: "steam", PlaytimeMin: ip(playtimeMin)}
}

func hdoc(games ...LibraryGame) *LibraryDoc { return &LibraryDoc{Games: games} }

// played is one stats doc with one day of play, n days ago.
func played(gameID string, daysAgo, wallMin int) UserGameStats {
	return UserGameStats{
		GameID:         gameID,
		PlayedMinTotal: wallMin,
		RecentDays:     []RecentDayEntry{{Date: dayAgo(daysAgo), WallMin: wallMin}},
		LastPlayed:     h2hNow - int64(daysAgo)*86_400_000,
	}
}

func member(id string, lib *LibraryDoc, stats ...UserGameStats) h2hMemberData {
	return h2hMemberData{UserID: id, Library: lib, Stats: stats}
}

func compute(prev *Head2headDoc, now int64, count int, ms ...h2hMemberData) *Head2headDoc {
	return computeHead2head(h2hInput{CrewID: "crew", MemberCount: count, Members: ms, Prev: prev, NowMs: now})
}

func titleOf(t *testing.T, d *Head2headDoc, id string) Head2headTitle {
	t.Helper()
	for _, x := range d.Titles {
		if x.TitleID == id {
			return x
		}
	}
	t.Fatalf("title %s missing, have %+v", id, d.Titles)
	return Head2headTitle{}
}

func noTitle(t *testing.T, d *Head2headDoc, id string) {
	t.Helper()
	for _, x := range d.Titles {
		if x.TitleID == id {
			t.Fatalf("title %s should be omitted, got %+v", id, x)
		}
	}
}

// ---- the_carry ----

func TestTheCarryTwoMembers(t *testing.T) {
	d := compute(nil, h2hNow, 2,
		member("u1", hdoc(hgame("steam-1", 600), hgame("steam-9", 5000))),
		member("u2", hdoc(hgame("steam-1", 1800))),
	)
	c := titleOf(t, d, titleTheCarry)
	if c.HolderUserID != "u2" || c.Value != 30.0 || c.Unit != "h" {
		t.Fatalf("carry: %+v", c)
	}
}

func TestTheCarryThreeMembersUsesMostInstalledGame(t *testing.T) {
	// steam-1 is on three PCs. steam-9 has more minutes but only one owner.
	d := compute(nil, h2hNow, 3,
		member("u1", hdoc(hgame("steam-1", 600), hgame("steam-9", 90000))),
		member("u2", hdoc(hgame("steam-1", 1200))),
		member("u3", hdoc(hgame("steam-1", 900))),
	)
	c := titleOf(t, d, titleTheCarry)
	if c.HolderUserID != "u2" || c.Value != 20.0 {
		t.Fatalf("carry: %+v", c)
	}
}

func TestTheCarryTopGameTieBrokenByTotalPlaytime(t *testing.T) {
	d := compute(nil, h2hNow, 2,
		member("u1", hdoc(hgame("steam-1", 60), hgame("steam-2", 600))),
		member("u2", hdoc(hgame("steam-1", 120), hgame("steam-2", 1200))),
	)
	// Both games have two owners. steam-2 has the larger total.
	c := titleOf(t, d, titleTheCarry)
	if c.HolderUserID != "u2" || c.Value != 20.0 {
		t.Fatalf("carry: %+v", c)
	}
}

func TestTheCarryFallsBackToObservedMinutes(t *testing.T) {
	noFigure := LibraryGame{GameID: "steam-1", Name: "x", Launcher: "steam"}
	d := compute(nil, h2hNow, 2,
		member("u1", hdoc(hgame("steam-1", 60))),
		member("u2", hdoc(noFigure), played("steam-1", 1, 300)),
	)
	if c := titleOf(t, d, titleTheCarry); c.HolderUserID != "u2" || c.Value != 5.0 {
		t.Fatalf("carry: %+v", c)
	}
}

// ---- locked_in and has_a_life ----

func TestLockedInAndHasALifeTwoMembers(t *testing.T) {
	d := compute(nil, h2hNow, 2,
		member("u1", nil, played("steam-1", 1, 120), played("steam-2", 3, 60)),
		member("u2", nil, played("steam-1", 2, 30)),
	)
	if l := titleOf(t, d, titleLockedIn); l.HolderUserID != "u1" || l.Value != 3.0 {
		t.Fatalf("locked_in: %+v", l)
	}
	if h := titleOf(t, d, titleHasALife); h.HolderUserID != "u2" || h.Value != 0.5 {
		t.Fatalf("has_a_life: %+v", h)
	}
}

func TestHasALifeNeedsASessionInTheWeek(t *testing.T) {
	// u3 played only 10 days ago. u3 is not a contender for either week title.
	d := compute(nil, h2hNow, 3,
		member("u1", nil, played("steam-1", 1, 300)),
		member("u2", nil, played("steam-1", 1, 60)),
		member("u3", nil, played("steam-1", 10, 5)),
	)
	if h := titleOf(t, d, titleHasALife); h.HolderUserID != "u2" {
		t.Fatalf("has_a_life: %+v", h)
	}
	if l := titleOf(t, d, titleLockedIn); l.HolderUserID != "u1" {
		t.Fatalf("locked_in: %+v", l)
	}
}

func TestWeekWindowIsSevenDaysInclusive(t *testing.T) {
	d := compute(nil, h2hNow, 2,
		member("u1", nil, played("steam-1", 6, 100)),
		member("u2", nil, played("steam-1", 7, 500)),
	)
	// u2's day is outside the window, so locked_in has one contender.
	noTitle(t, d, titleLockedIn)
}

// ---- the_collector ----

func TestCollector(t *testing.T) {
	d := compute(nil, h2hNow, 3,
		member("u1", hdoc(hgame("steam-1", 1), hgame("steam-2", 1))),
		member("u2", hdoc(hgame("steam-1", 1), hgame("steam-2", 1), hgame("steam-3", 1))),
		member("u3", hdoc(hgame("steam-1", 1))),
	)
	c := titleOf(t, d, titleTheCollect)
	if c.HolderUserID != "u2" || c.Value != 3 || c.Unit != "installed" {
		t.Fatalf("collector: %+v", c)
	}
}

// ---- sampler ----

func TestSamplerCountsDistinctGamesInFourteenDays(t *testing.T) {
	d := compute(nil, h2hNow, 3,
		member("u1", nil, played("steam-1", 1, 10), played("steam-2", 13, 10)),
		member("u2", nil, played("steam-1", 1, 10), played("steam-3", 14, 10)), // 14 days ago is out
		member("u3", nil, played("steam-1", 2, 10), played("steam-2", 2, 10), played("steam-3", 20, 10)),
	)
	s := titleOf(t, d, titleSampler)
	if s.HolderUserID != "u1" && s.HolderUserID != "u3" {
		t.Fatalf("sampler: %+v", s)
	}
	if s.Value != 2 || s.Unit != "games" {
		t.Fatalf("sampler: %+v", s)
	}
}

func TestSamplerSeesTwoWeekOldDaysAfterCapRaise(t *testing.T) {
	if recentDaysCap != 14 {
		t.Fatalf("recentDaysCap = %d, want 14", recentDaysCap)
	}
}

// ---- one_trick ----

func TestOneTrick(t *testing.T) {
	d := compute(nil, h2hNow, 3,
		member("u1", hdoc(hgame("steam-1", 900), hgame("steam-2", 100))), // 90%
		member("u2", hdoc(hgame("steam-1", 500), hgame("steam-2", 500))), // 50%
		member("u3", hdoc(hgame("steam-1", 100), hgame("steam-2", 100))), // 50%
	)
	o := titleOf(t, d, titleOneTrick)
	if o.HolderUserID != "u1" || o.Value != 90 || o.Unit != "%" {
		t.Fatalf("one_trick: %+v", o)
	}
}

// ---- night_owl ----

func sessionAt(gameID string, utc time.Time, tz *int) UserGameStats {
	s := played(gameID, 1, 10)
	s.LastPlayed = utc.UnixMilli()
	s.LastTzOffsetMin = tz
	return s
}

func TestNightOwlLatestLocalClock(t *testing.T) {
	day := time.Date(2026, time.October, 18, 0, 0, 0, 0, time.UTC)
	d := compute(nil, h2hNow, 3,
		// 23:30 local (UTC+2 from 21:30 UTC).
		member("u1", nil, sessionAt("steam-1", day.Add(21*time.Hour+30*time.Minute), ip(120))),
		// 01:15 local (UTC-5 from 06:15 UTC). After midnight is later than 23:30.
		member("u2", nil, sessionAt("steam-1", day.Add(6*time.Hour+15*time.Minute), ip(-300))),
		// 22:00 UTC with no offset: skipped.
		member("u3", nil, sessionAt("steam-1", day.Add(22*time.Hour), nil)),
	)
	n := titleOf(t, d, titleNightOwl)
	if n.HolderUserID != "u2" || n.Value != "01:15" {
		t.Fatalf("night_owl: %+v", n)
	}
}

func TestNightOwlSkipsMembersWithoutOffset(t *testing.T) {
	day := time.Date(2026, time.October, 18, 22, 0, 0, 0, time.UTC)
	d := compute(nil, h2hNow, 2,
		member("u1", nil, sessionAt("steam-1", day, ip(0))),
		member("u2", nil, sessionAt("steam-1", day, nil)),
	)
	noTitle(t, d, titleNightOwl)
}

func TestNightOwlIgnoresSessionsOlderThanFourteenDays(t *testing.T) {
	old := time.UnixMilli(h2hNow).UTC().AddDate(0, 0, -20)
	d := compute(nil, h2hNow, 2,
		member("u1", nil, sessionAt("steam-1", old, ip(0))),
		member("u2", nil, sessionAt("steam-1", old, ip(0))),
	)
	noTitle(t, d, titleNightOwl)
}

// ---- contested and pairs ----

func TestOneMemberHoldsNothing(t *testing.T) {
	d := compute(nil, h2hNow, 1,
		member("u1", hdoc(hgame("steam-1", 600)), played("steam-1", 1, 100)),
	)
	if len(d.Titles) != 0 || len(d.Pairs) != 0 {
		t.Fatalf("titles=%+v pairs=%+v", d.Titles, d.Pairs)
	}
	if len(d.Members) != 1 {
		t.Fatalf("members: %v", d.Members)
	}
}

func TestMemberWithoutDataIsNotAMember(t *testing.T) {
	d := compute(nil, h2hNow, 2,
		member("u1", hdoc(hgame("steam-1", 600))),
		member("u2", nil),
	)
	if len(d.Members) != 1 || d.MemberCount != 2 {
		t.Fatalf("members=%v count=%d", d.Members, d.MemberCount)
	}
}

func TestEmptyLibraryIsNoData(t *testing.T) {
	d := compute(nil, h2hNow, 2, member("u1", hdoc()), member("u2", hdoc(hgame("steam-1", 1))))
	noTitle(t, d, titleTheCollect)
}

func TestPairCounts(t *testing.T) {
	// u1 holds carry, collector. u2 holds locked_in. Third member is in the mix.
	ms := []h2hMemberData{
		member("u1", hdoc(hgame("steam-1", 3000), hgame("steam-2", 10), hgame("steam-3", 10)), played("steam-1", 1, 60)),
		member("u2", hdoc(hgame("steam-1", 1000)), played("steam-1", 1, 400)),
		member("u3", hdoc(hgame("steam-1", 2000), hgame("steam-2", 10)), played("steam-1", 1, 200)),
	}
	d := compute(nil, h2hNow, 3, ms...)
	if len(d.Pairs) != 3 {
		t.Fatalf("pairs: %+v", d.Pairs)
	}
	get := func(a, b string) Head2headPair {
		for _, p := range d.Pairs {
			if p.A == a && p.B == b {
				return p
			}
		}
		t.Fatalf("pair %s/%s missing", a, b)
		return Head2headPair{}
	}
	// u1 v u2: carry u1, locked_in u2, has_a_life u1, collector u1, sampler u1, one_trick:
	// u1 3000/3020 = 99%, u2 100%, so u2. sampler is a tie at one game, the lower id wins.
	p := get("u1", "u2")
	if p.ACount != 4 || p.BCount != 2 {
		t.Fatalf("u1/u2: %+v", p)
	}
	// Every pair counts each contested title with both members as contenders once.
	for _, p := range d.Pairs {
		if p.ACount+p.BCount == 0 {
			t.Fatalf("empty pair %+v", p)
		}
	}
}

func TestPairOnlyCountsTitlesBothContend(t *testing.T) {
	// u3 has no library, so u3 contends only for the titles built from play time.
	d := compute(nil, h2hNow, 3,
		member("u1", hdoc(hgame("steam-1", 600), hgame("steam-2", 1))),
		member("u2", hdoc(hgame("steam-1", 60))),
		member("u3", nil, played("steam-1", 1, 100)),
	)
	for _, p := range d.Pairs {
		// u1 and u3 share one_trick only. u1/u2 share carry, collector, one_trick.
		if p.A == "u1" && p.B == "u3" && p.ACount+p.BCount != 1 {
			t.Fatalf("u1/u3 share one contested title: %+v", p)
		}
		if p.A == "u1" && p.B == "u2" && p.ACount+p.BCount != 3 {
			t.Fatalf("u1/u2 share three contested titles: %+v", p)
		}
	}
}

func TestPairTieKeepsPreviousHolder(t *testing.T) {
	ms := []h2hMemberData{
		member("u1", hdoc(hgame("steam-1", 1))),
		member("u2", hdoc(hgame("steam-1", 1))),
	}
	first := compute(nil, h2hNow, 2, ms...)
	if first.Titles[0].HolderUserID != "u1" {
		t.Fatalf("tie with no history goes to the lower id: %+v", first.Titles)
	}
	prev := &Head2headDoc{Titles: []Head2headTitle{{TitleID: titleTheCollect, HolderUserID: "u2", SinceMs: 5}}}
	d := compute(prev, h2hNow, 2, ms...)
	if c := titleOf(t, d, titleTheCollect); c.HolderUserID != "u2" || c.SinceMs != 5 {
		t.Fatalf("tie keeps the holder: %+v", c)
	}
}

// ---- open seats ----

func TestOpenSeats(t *testing.T) {
	for count, want := range map[int]int{0: 5, 1: 4, 2: 3, 5: 0, 7: 0} {
		if got := head2headOpenSeats(count); got != want {
			t.Errorf("seats(%d) = %d want %d", count, got, want)
		}
		d := compute(nil, h2hNow, count)
		if d.OpenSeats != want || d.MemberCount != count {
			t.Errorf("doc(%d): seats=%d count=%d", count, d.OpenSeats, d.MemberCount)
		}
	}
}

// ---- bookkeeping ----

func TestRecomputeKeepsSinceWhenHolderUnchanged(t *testing.T) {
	ms := []h2hMemberData{
		member("u1", hdoc(hgame("steam-1", 1), hgame("steam-2", 1))),
		member("u2", hdoc(hgame("steam-1", 1))),
	}
	first := compute(nil, 1_000, 2, ms...)
	second := compute(first, 9_000, 2, ms...)
	c := titleOf(t, second, titleTheCollect)
	if c.SinceMs != 1_000 || c.MovedAtMs != 0 || c.PreviousHolderUserID != "" {
		t.Fatalf("unchanged holder: %+v", c)
	}
	if second.UpdatedAt != 9_000 {
		t.Fatalf("updated_at: %d", second.UpdatedAt)
	}
}

func TestRecomputeSetsMovedAtOnChange(t *testing.T) {
	before := []h2hMemberData{
		member("u1", hdoc(hgame("steam-1", 1), hgame("steam-2", 1))),
		member("u2", hdoc(hgame("steam-1", 1))),
	}
	after := []h2hMemberData{
		before[0],
		member("u2", hdoc(hgame("steam-1", 1), hgame("steam-2", 1), hgame("steam-3", 1))),
	}
	first := compute(nil, 1_000, 2, before...)
	second := compute(first, 9_000, 2, after...)
	c := titleOf(t, second, titleTheCollect)
	if c.HolderUserID != "u2" || c.PreviousHolderUserID != "u1" || c.MovedAtMs != 9_000 || c.SinceMs != 9_000 {
		t.Fatalf("moved: %+v", c)
	}
	// A third run with the same data keeps the move.
	third := compute(second, 12_000, 2, after...)
	c = titleOf(t, third, titleTheCollect)
	if c.SinceMs != 9_000 || c.MovedAtMs != 9_000 || c.PreviousHolderUserID != "u1" {
		t.Fatalf("kept move: %+v", c)
	}
}

func TestNewTitleHasNoMove(t *testing.T) {
	d := compute(nil, 5_000, 2,
		member("u1", hdoc(hgame("steam-1", 1))),
		member("u2", hdoc(hgame("steam-1", 1))),
	)
	for _, tt := range d.Titles {
		if tt.SinceMs != 5_000 || tt.MovedAtMs != 0 || tt.PreviousHolderUserID != "" {
			t.Fatalf("new title: %+v", tt)
		}
	}
}

// ---- stats: cap and tz ----

func TestRecentDaysCapKeepsFourteen(t *testing.T) {
	s := &UserGameStats{GameID: "steam-1"}
	base := time.Date(2026, time.October, 1, 12, 0, 0, 0, time.UTC)
	for i := 0; i < 20; i++ {
		applySessionPlayTime(s, 10, 5, base.AddDate(0, 0, i))
	}
	if len(s.RecentDays) != 14 {
		t.Fatalf("days kept: %d", len(s.RecentDays))
	}
	if s.RecentDays[0].Date != "2026-10-07" || s.RecentDays[13].Date != "2026-10-20" {
		t.Fatalf("window: %s .. %s", s.RecentDays[0].Date, s.RecentDays[13].Date)
	}
}

func TestSessionStoresTzOffsetAndKeepsItWhenAbsent(t *testing.T) {
	nk := newFakeGuestNk()
	ctx := context.Background()
	if _, _, err := UpdateUserGameStatsForSession(ctx, nk, "u1", "steam-1", 10, 5, 0, 0, 0, ip(120)); err != nil {
		t.Fatal(err)
	}
	s, _ := readUserGameStats(ctx, nk, "u1", "steam-1")
	if s.LastTzOffsetMin == nil || *s.LastTzOffsetMin != 120 {
		t.Fatalf("tz: %v", s.LastTzOffsetMin)
	}
	if _, _, err := UpdateUserGameStatsForSession(ctx, nk, "u1", "steam-1", 10, 5, 0, 0, 0, nil); err != nil {
		t.Fatal(err)
	}
	s, _ = readUserGameStats(ctx, nk, "u1", "steam-1")
	if s.LastTzOffsetMin == nil || *s.LastTzOffsetMin != 120 {
		t.Fatalf("tz after a session without offset: %v", s.LastTzOffsetMin)
	}
}

func TestValidTzOffset(t *testing.T) {
	if validTzOffset(nil) != nil || validTzOffset(ip(900)) != nil || validTzOffset(ip(-841)) != nil {
		t.Fatal("bad offsets must be dropped")
	}
	if v := validTzOffset(ip(-300)); v == nil || *v != -300 {
		t.Fatal("good offset must pass")
	}
}

// ---- recompute through storage ----

func (f *fakeGuestNk) UserGroupsList(_ context.Context, userID string, _ int, _ *int, _ string) ([]*api.UserGroupList_UserGroup, string, error) {
	var out []*api.UserGroupList_UserGroup
	for crew, ms := range f.members {
		for _, m := range ms {
			if m.GetUser().GetId() == userID {
				out = append(out, &api.UserGroupList_UserGroup{Group: &api.Group{Id: crew}})
			}
		}
	}
	return out, "", nil
}

func (f *fakeGuestNk) StorageList(_ context.Context, _, userID, collection string, _ int, _ string) ([]*api.StorageObject, string, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	prefix := collection + "/"
	suffix := "/" + userID
	var out []*api.StorageObject
	for k, v := range f.storage {
		if strings.HasPrefix(k, prefix) && strings.HasSuffix(k, suffix) {
			out = append(out, &api.StorageObject{Collection: collection, UserId: userID, Value: v})
		}
	}
	return out, "", nil
}

func h2hFixture() *fakeGuestNk {
	nk := newFakeGuestNk()
	nk.addMember("crew", "u1", "a")
	nk.addMember("crew", "u2", "b")
	nk.put(UserLibraryCollection, UserLibraryKey, "u1", hdoc(hgame("steam-1", 600), hgame("steam-2", 5)))
	nk.put(UserLibraryCollection, UserLibraryKey, "u2", hdoc(hgame("steam-1", 60)))
	return nk
}

func TestRecomputeWritesDocForEveryCrewOfTheUser(t *testing.T) {
	nk := h2hFixture()
	nk.addMember("crew2", "u1", "a")
	nk.addMember("crew2", "u3", "c")
	recomputeHead2headForUser(context.Background(), testLogger(), nk, "u1", head2headTriggerLibrary)
	for _, c := range []string{"crew", "crew2"} {
		if doc, _ := readHead2headDoc(context.Background(), nk, c); doc == nil || doc.CrewID != c {
			t.Fatalf("no doc for %s", c)
		}
	}
	raw := nk.storage[fakeStorageKey(CrewHead2headCollection, head2headKey("crew"), SystemUserID)]
	var doc Head2headDoc
	if err := json.Unmarshal([]byte(raw), &doc); err != nil || doc.MemberCount != 2 || doc.OpenSeats != 3 {
		t.Fatalf("doc: %v %+v", err, doc)
	}
}

func TestSessionTriggerDebounce(t *testing.T) {
	nk := h2hFixture()
	ctx := context.Background()
	lg := testLogger()
	if recomputeHead2head(ctx, lg, nk, "crew", head2headTriggerLibrary, h2hNow) == nil {
		t.Fatal("first compute must write")
	}
	if recomputeHead2head(ctx, lg, nk, "crew", head2headTriggerSession, h2hNow+30_000) != nil {
		t.Fatal("a session end under 60 s after the doc must skip")
	}
	if recomputeHead2head(ctx, lg, nk, "crew", head2headTriggerLibrary, h2hNow+30_000) == nil {
		t.Fatal("a library report never skips")
	}
	if recomputeHead2head(ctx, lg, nk, "crew", head2headTriggerSession, h2hNow+30_000+61_000) == nil {
		t.Fatal("a session end after 60 s must write")
	}
}

func TestLibraryReportRecomputesTheDoc(t *testing.T) {
	nk := newFakeGuestNk()
	nk.addMember("crew", "u1", "a")
	nk.addMember("crew", "u2", "b")
	nk.put(UserLibraryCollection, UserLibraryKey, "u2", hdoc(hgame("steam-1", 60)))
	if err := libReport(t, nk, "u1", `{"games":[{"game_id":"steam-1","name":"G","launcher":"steam","playtime_min":600}]}`); err != nil {
		t.Fatal(err)
	}
	doc, _ := readHead2headDoc(context.Background(), nk, "crew")
	if doc == nil || titleOf(t, doc, titleTheCarry).HolderUserID != "u1" {
		t.Fatalf("doc after report: %+v", doc)
	}
}

// ---- feed payload ----

func h2hDocForFeed() *Head2headDoc {
	return &Head2headDoc{
		CrewID:  "crew",
		Members: []string{"u1", "u2", "u3"},
		Titles: []Head2headTitle{
			{TitleID: titleTheCarry, HolderUserID: "u1", Value: 5.0, Unit: "h", SinceMs: 1},
			{TitleID: titleLockedIn, HolderUserID: "u2", Value: 2.0, Unit: "h", SinceMs: 2, PreviousHolderUserID: "u1", MovedAtMs: h2hNow - 3_600_000},
			{TitleID: titleSampler, HolderUserID: "u3", Value: 4, Unit: "games", SinceMs: 3, PreviousHolderUserID: "u2", MovedAtMs: h2hNow - 7_200_000},
			{TitleID: titleOneTrick, HolderUserID: "u1", Value: 80, Unit: "%", SinceMs: 4, PreviousHolderUserID: "u2", MovedAtMs: h2hNow - 48*3_600_000},
		},
		Pairs: []Head2headPair{
			{A: "u1", B: "u2", ACount: 3, BCount: 1},
			{A: "u1", B: "u3", ACount: 2, BCount: 2},
			{A: "u2", B: "u3", ACount: 1, BCount: 3},
		},
		MemberCount: 3,
		OpenSeats:   2,
		UpdatedAt:   h2hNow - 1000,
	}
}

func TestFeedPayloadTrimsPairsForViewer(t *testing.T) {
	f := trimHead2headForViewer(h2hDocForFeed(), "crew", "u2", 3, h2hNow)
	if len(f.Pairs) != 2 {
		t.Fatalf("viewer pairs: %+v", f.Pairs)
	}
	for _, p := range f.Pairs {
		if p.A != "u2" {
			t.Fatalf("viewer must be A: %+v", p)
		}
	}
	// u1/u2 was 3-1 with u1 as A. For the viewer u2 it is 1-3.
	if f.Pairs[0].B != "u1" || f.Pairs[0].ACount != 1 || f.Pairs[0].BCount != 3 {
		t.Fatalf("swap: %+v", f.Pairs[0])
	}
	if len(f.Titles) != 4 {
		t.Fatalf("full titles list: %d", len(f.Titles))
	}
	if f.MemberCount != 3 || f.OpenSeats != 2 || f.UpdatedAt != h2hNow-1000 {
		t.Fatalf("counts: %+v", f)
	}
}

func TestFeedPayloadBeltMovedRecentIsNewestWithin24h(t *testing.T) {
	f := trimHead2headForViewer(h2hDocForFeed(), "crew", "u1", 3, h2hNow)
	if f.BeltMovedRecent == nil || f.BeltMovedRecent.TitleID != titleLockedIn {
		t.Fatalf("belt: %+v", f.BeltMovedRecent)
	}
}

func TestFeedPayloadNoBeltWhenMovesAreOld(t *testing.T) {
	f := trimHead2headForViewer(h2hDocForFeed(), "crew", "u1", 3, h2hNow+30*3_600_000)
	if f.BeltMovedRecent != nil {
		t.Fatalf("belt: %+v", f.BeltMovedRecent)
	}
}

func TestFeedPayloadForCrewWithoutDoc(t *testing.T) {
	f := trimHead2headForViewer(nil, "crew", "u1", 2, h2hNow)
	if f.OpenSeats != 3 || f.MemberCount != 2 || f.Titles == nil || f.Pairs == nil || len(f.Titles) != 0 {
		t.Fatalf("empty payload: %+v", f)
	}
	raw, _ := json.Marshal(f)
	if !strings.Contains(string(raw), `"titles":[]`) || !strings.Contains(string(raw), `"pairs":[]`) {
		t.Fatalf("lists must encode as []: %s", raw)
	}
}

func TestFeedPayloadSeatsFollowLiveMemberCount(t *testing.T) {
	f := trimHead2headForViewer(h2hDocForFeed(), "crew", "u1", 6, h2hNow)
	if f.OpenSeats != 0 || f.MemberCount != 6 {
		t.Fatalf("seats: %+v", f)
	}
}

// ---- hero ----

func TestBeltMovedHeroRanksAfterNothingAboveAndBeforePreview(t *testing.T) {
	now := h2hNow
	h2h := trimHead2headForViewer(h2hDocForFeed(), "crew", "u1", 3, now)
	cards := []feedCard{freshPreview("prev", now, 3_600_000), recapAt("recap", now-1000)}
	entries := buildThisWeekFull(cards, now, nil, h2h)
	if entries[0].Type != head2headFeedID || entries[0].Role != "hero" {
		t.Fatalf("hero: %+v", entries[0])
	}
}

func TestStalePreviewDoesNotBeatBelt(t *testing.T) {
	now := h2hNow
	h2h := trimHead2headForViewer(h2hDocForFeed(), "crew", "u1", 3, now)
	entries := buildThisWeekFull([]feedCard{freshPreview("prev", now, 1000)}, now, nil, h2h)
	if entries[0].Type != head2headFeedID {
		t.Fatalf("belt must lead a fresh preview: %+v", entries[0])
	}
	foundPreview := false
	for _, e := range entries[1:] {
		if e.ID == "prev" {
			foundPreview = true
		}
	}
	if !foundPreview {
		t.Fatal("the preview must still appear")
	}
}

func TestNoBeltMovedKeepsPreviewAsHeroAndStillEmitsHead2head(t *testing.T) {
	now := h2hNow
	doc := h2hDocForFeed()
	for i := range doc.Titles {
		doc.Titles[i].MovedAtMs = 0
	}
	h2h := trimHead2headForViewer(doc, "crew", "u1", 3, now)
	entries := buildThisWeekFull([]feedCard{freshPreview("prev", now, 1000)}, now, nil, h2h)
	if entries[0].ID != "prev" || entries[0].Role != "hero" {
		t.Fatalf("hero: %+v", entries[0])
	}
	n := 0
	for _, e := range entries {
		if e.Type == head2headFeedID {
			n++
			if e.Role == "hero" {
				t.Fatal("head2head must not be hero")
			}
		}
	}
	if n != 1 {
		t.Fatalf("head2head entries: %d", n)
	}
}

func TestHead2headEntryForEmptyCrew(t *testing.T) {
	h2h := trimHead2headForViewer(nil, "crew", "u1", 1, h2hNow)
	entries := buildThisWeekFull(nil, h2hNow, nil, h2h)
	if len(entries) != 1 || entries[0].Type != head2headFeedID {
		t.Fatalf("entries: %+v", entries)
	}
	if d, ok := entries[0].Data.(*Head2headFeedData); !ok || d.OpenSeats != 4 {
		t.Fatalf("data: %+v", entries[0].Data)
	}
}

func TestNilHead2headAddsNoEntry(t *testing.T) {
	for _, e := range buildThisWeekFull([]feedCard{clipCard("c", h2hNow-1)}, h2hNow, nil, nil) {
		if e.Type == head2headFeedID {
			t.Fatal("unexpected head2head entry")
		}
	}
}

func TestFeedRPCIncludesHead2head(t *testing.T) {
	nk := h2hFixture()
	ctx := ctxWithUser("u1")
	crewHead2headForViewer(ctx, testLogger(), nk, "crew", "u1", h2hNow)
	f := crewHead2headForViewer(ctx, testLogger(), nk, "crew", "u1", h2hNow)
	if f == nil || f.MemberCount != 2 || f.OpenSeats != 3 || len(f.Pairs) != 1 || f.Pairs[0].A != "u1" {
		t.Fatalf("payload: %+v", f)
	}
}
