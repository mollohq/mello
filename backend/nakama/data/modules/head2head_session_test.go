package main

import (
	"context"
	"testing"
)

// act builds a member with only an activity doc.
func act(id string, lifetime UserActivityStats, days ...ActivityDay) h2hMemberData {
	lifetime.Days = days
	return h2hMemberData{UserID: id, Activity: &lifetime}
}

func day(daysAgo int, f func(*ActivityDay)) ActivityDay {
	d := ActivityDay{Date: dayAgo(daysAgo)}
	f(&d)
	return d
}

func localMin(h, m int) *int { v := h*60 + m; return &v }

func holder(t *testing.T, d *Head2headDoc, id string) Head2headTitle {
	t.Helper()
	return titleOf(t, d, id)
}

// ---- mouth ----

func TestMouthTwoMembers(t *testing.T) {
	d := compute(nil, h2hNow, 2,
		act("u1", UserActivityStats{}, day(1, func(d *ActivityDay) { d.VoiceMin = 100 })),
		act("u2", UserActivityStats{}, day(2, func(d *ActivityDay) { d.VoiceMin = 40 })))
	m := holder(t, d, titleMouth)
	if m.HolderUserID != "u1" || m.Value != 100 || m.Unit != "min" {
		t.Fatalf("mouth: %+v", m)
	}
}

func TestMouthThreeMembersSumsDaysAndUsesFourteenDayWindow(t *testing.T) {
	d := compute(nil, h2hNow, 3,
		act("u1", UserActivityStats{}, day(13, func(d *ActivityDay) { d.VoiceMin = 60 }), day(0, func(d *ActivityDay) { d.VoiceMin = 50 })),
		act("u2", UserActivityStats{}, day(3, func(d *ActivityDay) { d.VoiceMin = 90 })),
		// 500 minutes, but 14 days ago is outside the window (days 0 to 13).
		act("u3", UserActivityStats{}, day(14, func(d *ActivityDay) { d.VoiceMin = 500 })))
	m := holder(t, d, titleMouth)
	if m.HolderUserID != "u1" || m.Value != 110 {
		t.Fatalf("mouth: %+v", m)
	}
	// u3 has no minutes in the window, so only u1 and u2 contend.
	for _, p := range d.Pairs {
		if p.A == "u3" || p.B == "u3" {
			if len(p.Values) != 0 {
				t.Fatalf("u3 must not contend for mouth: %+v", p)
			}
		}
	}
}

func TestMouthOneContenderIsNotContested(t *testing.T) {
	d := compute(nil, h2hNow, 2,
		act("u1", UserActivityStats{}, day(1, func(d *ActivityDay) { d.VoiceMin = 100 })),
		act("u2", UserActivityStats{Clips: 1}))
	noTitle(t, d, titleMouth)
}

// ---- marathon ----

func TestMarathonTwoMembersLifetime(t *testing.T) {
	d := compute(nil, h2hNow, 2,
		act("u1", UserActivityStats{LongestSessionMin: 90}),
		act("u2", UserActivityStats{LongestSessionMin: 45}))
	m := holder(t, d, titleMarathon)
	if m.HolderUserID != "u1" || m.Value != 90 || m.Unit != "min" {
		t.Fatalf("marathon: %+v", m)
	}
}

func TestMarathonThreeMembersIgnoresTheWindow(t *testing.T) {
	// The longest session is lifetime: u3's 200 minutes sits in no bucket.
	d := compute(nil, h2hNow, 3,
		act("u1", UserActivityStats{LongestSessionMin: 90}),
		act("u2", UserActivityStats{LongestSessionMin: 45}),
		act("u3", UserActivityStats{LongestSessionMin: 200}, day(30, func(d *ActivityDay) { d.LongestSessionMin = 200 })))
	if m := holder(t, d, titleMarathon); m.HolderUserID != "u3" || m.Value != 200 {
		t.Fatalf("marathon: %+v", m)
	}
}

// ---- last_one_up ----

func TestLastOneUpTwoMembersRanksFromEighteen(t *testing.T) {
	d := compute(nil, h2hNow, 2,
		act("u1", UserActivityStats{}, day(1, func(d *ActivityDay) { d.LatestEndLocalMin = localMin(23, 30) })),
		act("u2", UserActivityStats{}, day(2, func(d *ActivityDay) { d.LatestEndLocalMin = localMin(1, 15) })))
	m := holder(t, d, titleLastOneUp)
	if m.HolderUserID != "u2" || m.Value != "01:15" || m.Unit != "" {
		t.Fatalf("last_one_up: %+v", m)
	}
}

func TestLastOneUpThreeMembersTakesTheLatestDayAndSkipsOldOnes(t *testing.T) {
	d := compute(nil, h2hNow, 3,
		act("u1", UserActivityStats{},
			day(5, func(d *ActivityDay) { d.LatestEndLocalMin = localMin(19, 0) }),
			day(1, func(d *ActivityDay) { d.LatestEndLocalMin = localMin(22, 45) })),
		act("u2", UserActivityStats{}, day(2, func(d *ActivityDay) { d.LatestEndLocalMin = localMin(21, 0) })),
		// 03:00 is the latest of all, but 14 days ago is outside the window.
		act("u3", UserActivityStats{}, day(14, func(d *ActivityDay) { d.LatestEndLocalMin = localMin(3, 0) })))
	m := holder(t, d, titleLastOneUp)
	if m.HolderUserID != "u1" || m.Value != "22:45" {
		t.Fatalf("last_one_up: %+v", m)
	}
}

func TestLastOneUpMemberWithoutAnOffsetDoesNotContend(t *testing.T) {
	d := compute(nil, h2hNow, 2,
		act("u1", UserActivityStats{}, day(1, func(d *ActivityDay) { d.LatestEndLocalMin = localMin(23, 0) })),
		act("u2", UserActivityStats{}, day(1, func(d *ActivityDay) { d.GameMin = 30 })))
	noTitle(t, d, titleLastOneUp)
}

// ---- clipper ----

func TestClipperTwoMembers(t *testing.T) {
	d := compute(nil, h2hNow, 2,
		act("u1", UserActivityStats{Clips: 1}),
		act("u2", UserActivityStats{Clips: 3}))
	m := holder(t, d, titleClipper)
	if m.HolderUserID != "u2" || m.Value != 3 || m.Unit != "clips" {
		t.Fatalf("clipper: %+v", m)
	}
}

func TestClipperThreeMembersIsLifetime(t *testing.T) {
	d := compute(nil, h2hNow, 3,
		act("u1", UserActivityStats{Clips: 4}, day(40, func(d *ActivityDay) { d.Clips = 4 })),
		act("u2", UserActivityStats{Clips: 2}, day(0, func(d *ActivityDay) { d.Clips = 2 })),
		act("u3", UserActivityStats{}))
	if m := holder(t, d, titleClipper); m.HolderUserID != "u1" || m.Value != 4 {
		t.Fatalf("clipper: %+v", m)
	}
}

// ---- rage_quit ----

func TestRageQuitTwoMembers(t *testing.T) {
	d := compute(nil, h2hNow, 2,
		act("u1", UserActivityStats{}, day(1, func(d *ActivityDay) { d.ShortSessions = 2 })),
		act("u2", UserActivityStats{}, day(1, func(d *ActivityDay) { d.ShortSessions = 1 })))
	m := holder(t, d, titleRageQuit)
	if m.HolderUserID != "u1" || m.Value != 2 || m.Unit != "sessions" {
		t.Fatalf("rage_quit: %+v", m)
	}
}

func TestRageQuitThreeMembersUsesTheFourteenDayWindow(t *testing.T) {
	d := compute(nil, h2hNow, 3,
		act("u1", UserActivityStats{ShortSessions: 9}, day(20, func(d *ActivityDay) { d.ShortSessions = 9 })),
		act("u2", UserActivityStats{}, day(4, func(d *ActivityDay) { d.ShortSessions = 1 }), day(6, func(d *ActivityDay) { d.ShortSessions = 2 })),
		act("u3", UserActivityStats{}, day(2, func(d *ActivityDay) { d.ShortSessions = 2 })))
	if m := holder(t, d, titleRageQuit); m.HolderUserID != "u2" || m.Value != 3 {
		t.Fatalf("rage_quit: %+v", m)
	}
}

// ---- winner ----

func TestWinnerTwoMembers(t *testing.T) {
	d := compute(nil, h2hNow, 2,
		act("u1", UserActivityStats{}, day(1, func(d *ActivityDay) { d.WinMoments = 1 })),
		act("u2", UserActivityStats{}, day(1, func(d *ActivityDay) { d.WinMoments = 2 })))
	m := holder(t, d, titleWinner)
	if m.HolderUserID != "u2" || m.Value != 2 || m.Unit != "wins" {
		t.Fatalf("winner: %+v", m)
	}
}

func TestWinnerThreeMembersUsesTheFourteenDayWindow(t *testing.T) {
	d := compute(nil, h2hNow, 3,
		act("u1", UserActivityStats{}, day(13, func(d *ActivityDay) { d.WinMoments = 2 })),
		act("u2", UserActivityStats{}, day(14, func(d *ActivityDay) { d.WinMoments = 8 })),
		act("u3", UserActivityStats{}, day(0, func(d *ActivityDay) { d.WinMoments = 1 })))
	if m := holder(t, d, titleWinner); m.HolderUserID != "u1" || m.Value != 2 {
		t.Fatalf("winner: %+v", m)
	}
}

// ---- shared rules ----

func TestSessionTitleTieKeepsThePreviousHolderThenLowestId(t *testing.T) {
	members := func() []h2hMemberData {
		return []h2hMemberData{
			act("u1", UserActivityStats{Clips: 2}),
			act("u2", UserActivityStats{Clips: 2}),
		}
	}
	first := compute(nil, h2hNow, 2, members()...)
	if holder(t, first, titleClipper).HolderUserID != "u1" {
		t.Fatal("no previous holder: the lowest id wins a tie")
	}
	prev := &Head2headDoc{Titles: []Head2headTitle{{TitleID: titleClipper, HolderUserID: "u2", SinceMs: 1}}}
	again := compute(prev, h2hNow, 2, members()...)
	if holder(t, again, titleClipper).HolderUserID != "u2" {
		t.Fatal("a tie keeps the previous holder")
	}
}

func TestSessionTitleMoveSetsPreviousHolder(t *testing.T) {
	prev := compute(nil, h2hNow, 2,
		act("u1", UserActivityStats{Clips: 2}),
		act("u2", UserActivityStats{Clips: 1}))
	next := compute(prev, h2hNow+1000, 2,
		act("u1", UserActivityStats{Clips: 2}),
		act("u2", UserActivityStats{Clips: 3}))
	c := holder(t, next, titleClipper)
	if c.HolderUserID != "u2" || c.PreviousHolderUserID != "u1" || c.MovedAtMs != h2hNow+1000 {
		t.Fatalf("clipper: %+v", c)
	}
}

func TestSessionTitlesCountInPairTallies(t *testing.T) {
	d := compute(nil, h2hNow, 2,
		act("u1", UserActivityStats{Clips: 2, LongestSessionMin: 90}),
		act("u2", UserActivityStats{Clips: 1, LongestSessionMin: 30}))
	p := d.Pairs[0]
	if p.ACount != 2 || p.BCount != 0 || len(p.Values) != 2 {
		t.Fatalf("pair: %+v", p)
	}
}

// ---- recompute through storage and triggers ----

func TestRecomputeReadsActivityDocs(t *testing.T) {
	nk := newFakeGuestNk()
	nk.addMember("crew", "u1", "a")
	nk.addMember("crew", "u2", "b")
	nk.put(UserActivityStatsCollection, userActivityStatsKey, "u1", UserActivityStats{Clips: 1})
	nk.put(UserActivityStatsCollection, userActivityStatsKey, "u2", UserActivityStats{Clips: 5})
	doc := recomputeHead2head(context.Background(), testLogger(), nk, "crew", head2headTriggerLibrary, h2hNow)
	if doc == nil || holder(t, doc, titleClipper).HolderUserID != "u2" {
		t.Fatalf("doc: %+v", doc)
	}
}

func TestClipPostRecomputesTheHolders(t *testing.T) {
	nk := newFakeGuestNk()
	nk.addMember("crew", "u1", "a")
	nk.addMember("crew", "u2", "b")
	nk.put(UserActivityStatsCollection, userActivityStatsKey, "u1", UserActivityStats{Clips: 1})
	p := `{"crew_id":"crew","clip_id":"c1","clip_type":"voice","duration_seconds":10}`
	if _, err := PostClipRPC(ctxWithUser("u2"), testLogger(), nil, nk, p); err != nil {
		t.Fatal(err)
	}
	doc, _ := readHead2headDoc(context.Background(), nk, "crew")
	if doc == nil {
		t.Fatal("a clip post must recompute the doc")
	}
	// u1 has 1 clip and u2 has 1: a tie, lowest id.
	if h := holder(t, doc, titleClipper); h.HolderUserID != "u1" {
		t.Fatalf("clipper: %+v", h)
	}
}

func TestWinMomentRecomputesAndOtherMomentsDoNot(t *testing.T) {
	nk := newFakeGuestNk()
	nk.addMember("crew", "u1", "a")
	nk.addMember("crew", "u2", "b")
	nk.put(UserActivityStatsCollection, userActivityStatsKey, "u1", UserActivityStats{Days: []ActivityDay{{Date: dayAgo(0), WinMoments: 1}}})
	post := func(sentiment string) {
		t.Helper()
		if _, err := PostMomentRPC(ctxWithUser("u2"), testLogger(), nil, nk, `{"crew_id":"crew","sentiment":"`+sentiment+`"}`); err != nil {
			t.Fatal(err)
		}
	}
	post("loss")
	if doc, _ := readHead2headDoc(context.Background(), nk, "crew"); doc != nil {
		t.Fatal("a loss moment must not recompute")
	}
	post("win")
	if doc, _ := readHead2headDoc(context.Background(), nk, "crew"); doc == nil {
		t.Fatal("a win moment must recompute")
	}
}
