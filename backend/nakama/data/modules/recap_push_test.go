package main

import (
	"context"
	"strings"
	"testing"
	"time"

	"github.com/heroiclabs/nakama-common/api"
	"github.com/heroiclabs/nakama-common/runtime"
)

func pushRecap() WeeklyRecapData {
	// Week 43 of 2026 starts Monday 19 October.
	start := time.Date(2026, 10, 19, 0, 0, 0, 0, time.UTC)
	return WeeklyRecapData{
		CrewID:    "crew",
		WeekStart: start.UnixMilli(),
		WeekEnd:   start.Add(7 * 24 * time.Hour).UnixMilli(),
		BeltsMoved: []RecapBeltMoved{
			{TitleID: titleTheCarry, HolderUserID: "jonas", HolderName: "Jonas", PreviousUserID: "bob", PreviousName: "Bob", MovedAtMs: 10},
			{TitleID: titleMarathon, HolderUserID: "max", HolderName: "Max", PreviousUserID: "sara", PreviousName: "Sara", MovedAtMs: 20},
		},
		BeltsHeld: []RecapBeltHeld{
			{UserID: "max", DisplayName: "Max", Count: 5},
			{UserID: "bob", DisplayName: "Bob", Count: 4},
			{UserID: "jonas", DisplayName: "Jonas", Count: 3},
			{UserID: "sara", DisplayName: "Sara", Count: 3},
			{UserID: "ann", DisplayName: "Ann", Count: 0},
		},
	}
}

func TestRecapPushBodyForTheMemberWhoLostABelt(t *testing.T) {
	if got := recapPushBody(pushRecap(), "bob"); got != "Jonas took The Carry. You hold 4." {
		t.Fatalf("got %q", got)
	}
}

func TestRecapPushBodyForTheMemberWhoTookABelt(t *testing.T) {
	if got := recapPushBody(pushRecap(), "max"); got != "You took Marathon from Sara. You hold 5." {
		t.Fatalf("got %q", got)
	}
}

func TestRecapPushBodyLostComesBeforeTook(t *testing.T) {
	r := pushRecap()
	// Bob also takes a belt from Max, newer than the one he lost.
	r.BeltsMoved = append(r.BeltsMoved, RecapBeltMoved{TitleID: titleClipper, HolderUserID: "bob", HolderName: "Bob", PreviousUserID: "max", PreviousName: "Max", MovedAtMs: 30})
	if got := recapPushBody(r, "bob"); got != "Jonas took The Carry. You hold 4." {
		t.Fatalf("got %q", got)
	}
}

func TestRecapPushBodyForAMemberWhoNothingMovedFor(t *testing.T) {
	if got := recapPushBody(pushRecap(), "ann"); got != "Week 43 recap. Max leads with 5." {
		t.Fatalf("got %q", got)
	}
	r := pushRecap()
	r.BeltsMoved = nil
	if got := recapPushBody(r, "bob"); got != "Week 43 recap. Max leads with 5." {
		t.Fatalf("got %q", got)
	}
}

func TestRecapPushBodyIsEmptyWithoutBelts(t *testing.T) {
	r := pushRecap()
	r.BeltsMoved, r.BeltsHeld = nil, nil
	if got := recapPushBody(r, "bob"); got != "" {
		t.Fatalf("got %q", got)
	}
	if items := buildRecapPushItems(r, []string{"bob", "max"}, nil); len(items) != 0 {
		t.Fatalf("no belts, no push: %+v", items)
	}
}

func TestRecapPushBodyFollowsTheCopyRules(t *testing.T) {
	r := pushRecap()
	r.BeltsMoved[0].HolderName = "Jo!nas — the great"
	got := recapPushBody(r, "bob")
	if strings.ContainsAny(got, "!—–") {
		t.Fatalf("forbidden mark in %q", got)
	}
	for _, uid := range []string{"bob", "max", "ann"} {
		b := recapPushBody(pushRecap(), uid)
		if strings.Contains(b, "!") || strings.Contains(b, "—") {
			t.Fatalf("%s: %q", uid, b)
		}
	}
}

func TestRecapPushAtIsMonday1800LocalOrUTC(t *testing.T) {
	monday := time.Date(2026, 10, 19, 0, 30, 0, 0, time.UTC)
	at := func(tz *int) time.Time { return time.UnixMilli(recapPushAt(monday, tz)).UTC() }
	if got := at(nil); !got.Equal(time.Date(2026, 10, 19, 18, 0, 0, 0, time.UTC)) {
		t.Fatalf("no tz: %v", got)
	}
	plus2, minus5, half := 120, -300, 330
	if got := at(&plus2); !got.Equal(time.Date(2026, 10, 19, 16, 0, 0, 0, time.UTC)) {
		t.Fatalf("+02:00: %v", got)
	}
	if got := at(&minus5); !got.Equal(time.Date(2026, 10, 19, 23, 0, 0, 0, time.UTC)) {
		t.Fatalf("-05:00: %v", got)
	}
	if got := at(&half); !got.Equal(time.Date(2026, 10, 19, 12, 30, 0, 0, time.UTC)) {
		t.Fatalf("+05:30: %v", got)
	}
	bad := 5000
	if got := at(&bad); !got.Equal(time.Date(2026, 10, 19, 18, 0, 0, 0, time.UTC)) {
		t.Fatalf("an offset out of range counts as unknown: %v", got)
	}
}

func TestBuildRecapPushItemsUseEachMembersOffset(t *testing.T) {
	r := pushRecap()
	r.WeekEnd = time.Date(2026, 10, 26, 0, 0, 0, 0, time.UTC).UnixMilli()
	plus2 := 120
	items := buildRecapPushItems(r, []string{"bob", "max"}, map[string]*int{"bob": &plus2})
	if len(items) != 2 {
		t.Fatalf("items: %+v", items)
	}
	want := time.Date(2026, 10, 26, 16, 0, 0, 0, time.UTC).UnixMilli()
	if items[0].UserID != "bob" || items[0].SendAtMs != want {
		t.Fatalf("bob: %+v", items[0])
	}
	want = time.Date(2026, 10, 26, 18, 0, 0, 0, time.UTC).UnixMilli()
	if items[1].UserID != "max" || items[1].SendAtMs != want {
		t.Fatalf("max has no offset, 18:00 UTC: %+v", items[1])
	}
}

func TestSplitDueRecapPushesSendsOnceAndDropsStale(t *testing.T) {
	base := time.Date(2026, 10, 19, 18, 0, 0, 0, time.UTC)
	doc := &RecapPushDoc{Items: []RecapPushItem{
		{UserID: "early", SendAtMs: base.UnixMilli()},
		{UserID: "later", SendAtMs: base.Add(5 * time.Hour).UnixMilli()},
		{UserID: "stale", SendAtMs: base.Add(-30 * time.Hour).UnixMilli()},
	}}
	due, changed := splitDueRecapPushes(doc, base.Add(30*time.Minute))
	if !changed || len(due) != 1 || due[0].UserID != "early" {
		t.Fatalf("due: %+v", due)
	}
	if !doc.Items[0].Sent || doc.Items[1].Sent || !doc.Items[2].Sent {
		t.Fatalf("sent flags: %+v", doc.Items)
	}
	if due, changed := splitDueRecapPushes(doc, base.Add(30*time.Minute)); changed || len(due) != 0 {
		t.Fatal("a second pass sends nothing")
	}
	if due, _ := splitDueRecapPushes(doc, base.Add(6*time.Hour)); len(due) != 1 || due[0].UserID != "later" {
		t.Fatalf("later: %+v", due)
	}
}

// ---- the job ----

// jobNk adds the group list the job reads.
type jobNk struct {
	*fakeGuestNk
	crews []string
}

func (j *jobNk) GroupsList(_ context.Context, _, _ string, _ *int, _ *bool, _ int, _ string) ([]*api.Group, string, error) {
	var out []*api.Group
	for _, c := range j.crews {
		out = append(out, &api.Group{Id: c})
	}
	return out, "", nil
}

func (j *jobNk) StorageList(ctx context.Context, caller, userID, collection string, limit int, cursor string) ([]*api.StorageObject, string, error) {
	objs, next, err := j.fakeGuestNk.StorageList(ctx, caller, userID, collection, limit, cursor)
	for _, o := range objs {
		// The fake keeps no key on a listed object; recover it from the crew doc.
		o.Key = "crew"
	}
	return objs, next, err
}

func newJobNk() *jobNk {
	return &jobNk{fakeGuestNk: recapFixture(), crews: []string{"crew"}}
}

// seedWeek gives the crew one voice session in the week before monday.
// upcomingMonday is the next Monday 00:00 UTC. The event ledger drops events
// older than its rolling window by the real clock, so a fixed date in a seeded
// test goes stale. A Monday ahead of now keeps the seeded Sunday in the window.
func upcomingMonday() time.Time {
	return mondayStartUTC(time.Now()).Add(7 * 24 * time.Hour)
}

func seedWeek(t *testing.T, nk runtime.NakamaModule, monday time.Time) {
	t.Helper()
	if err := AppendCrewEvent(context.Background(), nk, "crew", CrewEvent{
		ID: "e1", CrewID: "crew", Type: "voice_session", Timestamp: monday.Add(-24 * time.Hour).UnixMilli(),
		Data: VoiceSessionData{ChannelName: "General", ParticipantNames: []string{"Ann", "Bo"}, DurationMin: 45},
	}); err != nil {
		t.Fatal(err)
	}
}

func recapCount(nk runtime.NakamaModule) int {
	doc, _ := readRecapsDoc(context.Background(), nk, "crew")
	return len(doc.Recaps)
}

func TestRecapDueLookback(t *testing.T) {
	monday := time.Date(2026, 10, 19, 0, 0, 0, 0, time.UTC)
	empty := &CrewRecapsDoc{}
	cases := []struct {
		name string
		now  time.Time
		doc  *CrewRecapsDoc
		want bool
	}{
		{"monday 00:05", monday.Add(5 * time.Minute), empty, true},
		{"monday missed, tuesday noon", monday.Add(36 * time.Hour), empty, true},
		{"last hour of the lookback", monday.Add(recapLookback - time.Minute), empty, true},
		{"after the lookback", monday.Add(recapLookback), empty, false},
		{"sunday before monday", monday.Add(-time.Hour), empty, false},
		{"already written this week", monday.Add(36 * time.Hour), &CrewRecapsDoc{LastRecapWeek: "2026-W43"}, false},
		{"written last week only", monday.Add(5 * time.Minute), &CrewRecapsDoc{LastRecapWeek: "2026-W42"}, true},
		{"recap from before the marker existed", monday.Add(36 * time.Hour),
			&CrewRecapsDoc{Recaps: []WeeklyRecapData{{WeekEnd: monday.Add(10 * time.Minute).UnixMilli()}}}, false},
	}
	for _, c := range cases {
		if got := recapDue(c.doc, c.now); got != c.want {
			t.Errorf("%s: got %v want %v", c.name, got, c.want)
		}
	}
}

func TestWeeklyRecapJobRunsLateAfterAMissedMondayAndOnlyOnce(t *testing.T) {
	ctx := context.Background()
	monday := upcomingMonday()
	nk := newJobNk()
	seedWeek(t, nk, monday)
	// Wrong day of the job's own week: nothing runs on the Sunday before.
	generateDueRecaps(ctx, nk, testLogger(), monday.Add(-time.Hour))
	if recapCount(nk) != 0 {
		t.Fatal("no recap before Monday")
	}
	// The Monday tick was missed. Tuesday 09:00 runs it.
	generateDueRecaps(ctx, nk, testLogger(), monday.Add(33*time.Hour))
	if recapCount(nk) != 1 {
		t.Fatalf("late run: %d recaps", recapCount(nk))
	}
	doc, _ := readRecapsDoc(ctx, nk, "crew")
	if doc.LastRecapWeek != isoWeekKey(monday) {
		t.Fatalf("last_recap_week: %q", doc.LastRecapWeek)
	}
	r := doc.Recaps[0]
	if r.WeekEnd != monday.UnixMilli() || r.WeekStart != monday.Add(-7*24*time.Hour).UnixMilli() {
		t.Fatalf("the late recap covers the week that ended Monday 00:00: %+v", r)
	}
	// The next hour's tick does not write a second one.
	generateDueRecaps(ctx, nk, testLogger(), monday.Add(34*time.Hour))
	if recapCount(nk) != 1 {
		t.Fatalf("second run: %d recaps", recapCount(nk))
	}
}

func TestWeeklyRecapJobSkipsAWeekPastTheLookback(t *testing.T) {
	monday := upcomingMonday()
	nk := newJobNk()
	seedWeek(t, nk, monday)
	generateDueRecaps(context.Background(), nk, testLogger(), monday.Add(5*24*time.Hour))
	if recapCount(nk) != 0 {
		t.Fatal("a Saturday run is past the lookback")
	}
}

func TestLateRecapLeavesOutEventsAfterTheWeek(t *testing.T) {
	ctx := context.Background()
	monday := upcomingMonday()
	nk := newJobNk()
	seedWeek(t, nk, monday)
	if err := AppendCrewEvent(ctx, nk, "crew", CrewEvent{
		ID: "e2", CrewID: "crew", Type: "voice_session", Timestamp: monday.Add(10 * time.Hour).UnixMilli(),
		Data: VoiceSessionData{ChannelName: "General", DurationMin: 100},
	}); err != nil {
		t.Fatal(err)
	}
	generateDueRecaps(ctx, nk, testLogger(), monday.Add(30*time.Hour))
	doc, _ := readRecapsDoc(ctx, nk, "crew")
	if got := doc.Recaps[0].TotalHangoutMin; got != 45 {
		t.Fatalf("hangout %d, want 45: Monday's session belongs to the next week", got)
	}
}

func TestRecapJobQueuesAndSendsTheMondayPush(t *testing.T) {
	ctx := context.Background()
	monday := upcomingMonday()
	nk := newJobNk()
	seedWeek(t, nk, monday)
	// u2 takes the clipper title from u1 during the recap. u1 sits at +02:00.
	nk.put(UserActivityStatsCollection, userActivityStatsKey, "u1", UserActivityStats{Clips: 1, TzOffsetMin: intp(120)})
	nk.put(UserActivityStatsCollection, userActivityStatsKey, "u2", UserActivityStats{Clips: 4})
	if err := writeHead2headDoc(ctx, nk, &Head2headDoc{CrewID: "crew", Titles: []Head2headTitle{
		{TitleID: titleClipper, HolderUserID: "u1", Value: 1, Unit: "clips", SinceMs: 1},
	}, UpdatedAt: 1}, ""); err != nil {
		t.Fatal(err)
	}

	var sent []string
	old := recapPushSend
	recapPushSend = func(_ context.Context, _ runtime.Logger, _ runtime.NakamaModule, _, _ string, it RecapPushItem) {
		// The recap job recomputes head2head, which can queue belt pushes
		// for the moves it finds. This test checks the recap push only.
		if it.Type != "" && it.Type != recapPushType {
			return
		}
		sent = append(sent, it.UserID+": "+it.Body)
	}
	defer func() { recapPushSend = old }()

	runWeeklyRecapTick(ctx, nk, testLogger(), monday.Add(15*time.Minute))
	if len(sent) != 0 {
		t.Fatalf("nothing is due at 00:15: %v", sent)
	}
	// 16:30 UTC: 18:30 at u1, so u1 is due. u2 has no offset and waits for 18:00 UTC.
	runWeeklyRecapTick(ctx, nk, testLogger(), monday.Add(16*time.Hour+30*time.Minute))
	if len(sent) != 1 || !strings.HasPrefix(sent[0], "u1: ") {
		t.Fatalf("sent at 16:30 UTC: %v", sent)
	}
	runWeeklyRecapTick(ctx, nk, testLogger(), monday.Add(18*time.Hour+10*time.Minute))
	if len(sent) != 2 || !strings.HasPrefix(sent[1], "u2: ") {
		t.Fatalf("sent at 18:10 UTC: %v", sent)
	}
	runWeeklyRecapTick(ctx, nk, testLogger(), monday.Add(19*time.Hour))
	if len(sent) != 2 {
		t.Fatalf("each push goes once: %v", sent)
	}
	if want := "Bo took Clipper. You hold 0."; !strings.Contains(sent[0], want) {
		t.Fatalf("u1 lost the belt: %q", sent[0])
	}
	if want := "You took Clipper from Ann. You hold 1."; !strings.Contains(sent[1], want) {
		t.Fatalf("u2 took the belt: %q", sent[1])
	}
}

func intp(v int) *int { return &v }

func TestActivityDeltaKeepsTheLatestOffset(t *testing.T) {
	s := &UserActivityStats{Days: []ActivityDay{}}
	applyActivityDelta(s, activityDelta{GameMin: 5}, time.Now())
	if s.TzOffsetMin != nil {
		t.Fatal("no offset sent, none stored")
	}
	applyActivityDelta(s, activityDelta{GameMin: 5, TzOffsetMin: intp(60)}, time.Now())
	applyActivityDelta(s, activityDelta{VoiceMin: 5}, time.Now())
	if s.TzOffsetMin == nil || *s.TzOffsetMin != 60 {
		t.Fatalf("offset: %v", s.TzOffsetMin)
	}
	applyActivityDelta(s, activityDelta{GameMin: 5, TzOffsetMin: intp(-300)}, time.Now())
	if *s.TzOffsetMin != -300 {
		t.Fatalf("offset: %v", *s.TzOffsetMin)
	}
}
