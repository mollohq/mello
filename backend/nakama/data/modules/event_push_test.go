package main

import (
	"context"
	"strings"
	"testing"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

func utc(d, h, m int) time.Time { return time.Date(2026, 10, d, h, m, 0, 0, time.UTC) }

func TestBeltPushBodyCopy(t *testing.T) {
	one := []pushEvent{{Name: "Jonas", Title: "The Carry"}}
	if got := beltPushBody(one, 4); got != "Jonas took The Carry from you. You hold 4." {
		t.Fatalf("one: %q", got)
	}
	many := []pushEvent{{Name: "Max", Title: "Marathon"}, {Name: "Max", Title: "Clipper"}, {Name: "Jonas", Title: "The Carry"}}
	if got := beltPushBody(many, 2); got != "Jonas took The Carry and 2 more. You hold 2." {
		t.Fatalf("many: %q", got)
	}
	took := []pushEvent{{Name: "Sara", Title: "Night Owl", Took: true}}
	if got := beltPushBody(took, 3); got != "You took Night Owl from Sara. You hold 3." {
		t.Fatalf("took: %q", got)
	}
	if got := beltPushBody(nil, 1); got != "" {
		t.Fatalf("empty: %q", got)
	}
}

func TestBeltPushBodyFollowsTheCopyRules(t *testing.T) {
	got := beltPushBody([]pushEvent{{Name: cleanPushName("Jo!nas — the great"), Title: "The Carry"}}, 1)
	if strings.ContainsAny(got, "!—–") {
		t.Fatalf("%q", got)
	}
}

func TestPushQuietAdjust(t *testing.T) {
	plus2, minus5, half := 120, -300, 330
	cases := []struct {
		name string
		at   time.Time
		tz   *int
		want time.Time
	}{
		{"noon UTC is fine", utc(20, 12, 0), nil, utc(20, 12, 0)},
		{"09:00 is the first minute", utc(20, 9, 0), nil, utc(20, 9, 0)},
		{"08:59 waits for 09:00", utc(20, 8, 59), nil, utc(20, 9, 0)},
		{"21:59 is the last minute", utc(20, 21, 59), nil, utc(20, 21, 59)},
		{"22:00 waits for tomorrow", utc(20, 22, 0), nil, utc(21, 9, 0)},
		{"23:30 waits for tomorrow", utc(20, 23, 30), nil, utc(21, 9, 0)},
		{"03:00 waits for today", utc(20, 3, 0), nil, utc(20, 9, 0)},
		{"+02:00 at 20:30 UTC is 22:30 local", utc(20, 20, 30), &plus2, utc(21, 7, 0)},
		{"+02:00 at 06:30 UTC is 08:30 local", utc(20, 6, 30), &plus2, utc(20, 7, 0)},
		{"-05:00 at 12:00 UTC is 07:00 local", utc(20, 12, 0), &minus5, utc(20, 14, 0)},
		{"-05:00 at 03:00 UTC is 22:00 local the day before", utc(20, 3, 0), &minus5, utc(20, 14, 0)},
		{"+05:30 at 04:00 UTC is 09:30 local", utc(20, 4, 0), &half, utc(20, 4, 0)},
		{"an offset out of range counts as UTC", utc(20, 23, 0), intp(5000), utc(21, 9, 0)},
	}
	for _, c := range cases {
		if got := pushQuietAdjust(c.at, c.tz); !got.Equal(c.want) {
			t.Errorf("%s: got %v want %v", c.name, got, c.want)
		}
	}
}

func TestPushSendAtHoldsForTheWindowThenQuietHours(t *testing.T) {
	now := utc(20, 12, 0)
	if got := pushSendAt(now, 0, beltPushWindow, nil); !got.Equal(now) {
		t.Fatalf("first push goes at once: %v", got)
	}
	last := now.Add(-time.Hour).UnixMilli()
	if got := pushSendAt(now, last, beltPushWindow, nil); !got.Equal(now.Add(5 * time.Hour)) {
		t.Fatalf("held to the end of the window: %v", got)
	}
	// The window ends at 22:00 local: the push moves to the next 09:00.
	last = utc(20, 16, 0).UnixMilli()
	if got := pushSendAt(now, last, beltPushWindow, nil); !got.Equal(utc(21, 9, 0)) {
		t.Fatalf("window end in quiet hours: %v", got)
	}
	if got := pushSendAt(now, utc(19, 12, 0).UnixMilli(), beltPushWindow, nil); !got.Equal(now) {
		t.Fatalf("an old send holds nothing: %v", got)
	}
}

func TestBeltPushEventsSkipTheActor(t *testing.T) {
	names := map[string]string{"u1": "Ann", "u2": "Bo", "u3": "Cy"}
	members := map[string]bool{"u1": true, "u2": true, "u3": true}
	moves := []beltMove{{TitleID: titleTheCarry, HolderID: "u2", PreviousID: "u1"}}

	got := beltPushEvents(moves, names, members, "u2")
	if len(got) != 1 || len(got["u1"]) != 1 || got["u1"][0].Name != "Bo" || got["u1"][0].Title != "The Carry" {
		t.Fatalf("actor takes the belt: %+v", got)
	}
	if _, ok := got["u2"]; ok {
		t.Fatal("no push to the actor")
	}

	got = beltPushEvents(moves, names, members, "")
	if len(got["u2"]) != 1 || !got["u2"][0].Took || got["u2"][0].Name != "Ann" {
		t.Fatalf("no actor, the new holder is told too: %+v", got)
	}
	got = beltPushEvents(moves, names, members, "u3")
	if len(got["u2"]) != 1 {
		t.Fatalf("another member is the actor: %+v", got)
	}
	delete(members, "u1")
	got = beltPushEvents(moves, names, members, "")
	if _, ok := got["u1"]; ok {
		t.Fatal("a member who left gets no push")
	}
}

func TestBeltMovesOfReadsOnlyThisRecompute(t *testing.T) {
	doc := &Head2headDoc{UpdatedAt: 100, Titles: []Head2headTitle{
		{TitleID: titleTheCarry, HolderUserID: "b", PreviousHolderUserID: "a", MovedAtMs: 100},
		{TitleID: titleClipper, HolderUserID: "b", PreviousHolderUserID: "a", MovedAtMs: 40},
		{TitleID: titleMouth, HolderUserID: "a", MovedAtMs: 0},
	}}
	moves := beltMovesOf(doc)
	if len(moves) != 1 || moves[0].TitleID != titleTheCarry {
		t.Fatalf("moves: %+v", moves)
	}
}

// beltRig runs queueBeltPushes against the fake module, with the clock and the
// sender under the test's control.
type beltRig struct {
	t    *testing.T
	nk   *fakeGuestNk
	now  time.Time
	sent []string
}

func newBeltRig(t *testing.T) *beltRig {
	r := &beltRig{t: t, nk: newFakeGuestNk(), now: utc(20, 12, 0)}
	r.nk.addMember("crew", "u1", "Ann")
	r.nk.addMember("crew", "u2", "Bo")
	r.nk.addMember("crew", "u3", "Cy")
	oldClock, oldGo, oldSend := eventPushClock, eventPushGo, recapPushSend
	eventPushClock = func() time.Time { return r.now }
	eventPushGo = func(f func()) { f() }
	recapPushSend = func(_ context.Context, _ runtime.Logger, _ runtime.NakamaModule, crewID, _ string, it RecapPushItem) {
		r.sent = append(r.sent, it.UserID+"|"+it.Type+"|"+it.MessageID+"|"+it.Body)
	}
	t.Cleanup(func() { eventPushClock, eventPushGo, recapPushSend = oldClock, oldGo, oldSend })
	return r
}

// move writes a recompute where holder takes the titles from prev.
func (r *beltRig) move(actor, holder, prev string, titles ...string) {
	doc := &Head2headDoc{CrewID: "crew", UpdatedAt: r.now.UnixMilli()}
	for _, id := range titles {
		doc.Titles = append(doc.Titles, Head2headTitle{TitleID: id, HolderUserID: holder, PreviousHolderUserID: prev, MovedAtMs: doc.UpdatedAt})
	}
	ctx := context.Background()
	if actor != "" {
		ctx = withHead2headActor(ctx, actor)
	}
	queueBeltPushes(ctx, testLogger(), r.nk, doc)
}

// tick sends what is due at the rig clock, as the hourly job does.
func (r *beltRig) tick() {
	flushPushQueue(context.Background(), r.nk, testLogger(), beltPushKey+"crew")
}

func TestBeltPushGoesAtOnceToThePreviousHolderOnly(t *testing.T) {
	r := newBeltRig(t)
	r.move("u2", "u2", "u1", titleTheCarry)
	if len(r.sent) != 1 {
		t.Fatalf("sent: %v", r.sent)
	}
	want := "u1|belt|belt-" + shortPushID("crew") + "-" + shortPushID("u1") + "|Bo took The Carry from you. You hold 0."
	if r.sent[0] != want {
		t.Fatalf("got %q\nwant %q", r.sent[0], want)
	}
	if len(strings.SplitN(r.sent[0], "|", 4)[2]) > 64 {
		t.Fatal("the collapse id must fit 64 bytes")
	}
}

func TestBeltPushTellsTheNewHolderWhenSomeoneElseCausedTheMove(t *testing.T) {
	r := newBeltRig(t)
	r.move("", "u2", "u1", titleTheCarry) // the dirty loop knows no actor
	if len(r.sent) != 2 {
		t.Fatalf("sent: %v", r.sent)
	}
	got := strings.Join(r.sent, "\n")
	if !strings.Contains(got, "u1|belt|") || !strings.Contains(got, "u2|belt|") ||
		!strings.Contains(got, "You took The Carry from Ann. You hold 1.") {
		t.Fatalf("sent: %v", r.sent)
	}
}

func TestBeltPushCoalescesInsideTheWindow(t *testing.T) {
	r := newBeltRig(t)
	r.move("u2", "u2", "u1", titleTheCarry)
	if len(r.sent) != 1 {
		t.Fatalf("first: %v", r.sent)
	}
	r.now = r.now.Add(time.Hour)
	r.move("u2", "u2", "u1", titleClipper)
	r.now = r.now.Add(time.Hour)
	r.move("u3", "u3", "u1", titleMouth, titleMarathon)
	if len(r.sent) != 1 {
		t.Fatalf("moves inside the window are held: %v", r.sent)
	}
	r.now = utc(20, 17, 59)
	r.tick()
	if len(r.sent) != 1 {
		t.Fatalf("the window ends at 18:00: %v", r.sent)
	}
	r.now = utc(20, 18, 1)
	r.tick()
	if len(r.sent) != 2 || !strings.HasSuffix(r.sent[1], "Cy took Marathon and 2 more. You hold 0.") {
		t.Fatalf("one push for three moves: %v", r.sent)
	}
	r.tick()
	if len(r.sent) != 2 {
		t.Fatalf("each push goes once: %v", r.sent)
	}
	// A new move 6 h after the last send goes at once again.
	r.now = utc(21, 10, 0)
	r.move("u2", "u2", "u1", titleLockedIn)
	if len(r.sent) != 3 || !strings.HasSuffix(r.sent[2], "Bo took Locked In from you. You hold 0.") {
		t.Fatalf("after the window: %v", r.sent)
	}
}

func TestBeltPushWaitsForNineInTheRecipientsMorning(t *testing.T) {
	r := newBeltRig(t)
	r.now = utc(20, 23, 30)
	r.move("u2", "u2", "u1", titleTheCarry)
	if len(r.sent) != 0 {
		t.Fatalf("quiet hours: %v", r.sent)
	}
	r.now = utc(21, 8, 30)
	r.tick()
	if len(r.sent) != 0 {
		t.Fatalf("08:30 is still quiet: %v", r.sent)
	}
	r.now = utc(21, 9, 5)
	r.tick()
	if len(r.sent) != 1 || !strings.HasPrefix(r.sent[0], "u1|belt|") {
		t.Fatalf("09:05: %v", r.sent)
	}
}

func TestBeltPushUsesTheOffsetOnTheActivityDoc(t *testing.T) {
	r := newBeltRig(t)
	// 20:30 UTC is 22:30 at +02:00 for u1 and 20:30 for u2, who has no offset.
	r.nk.put(UserActivityStatsCollection, userActivityStatsKey, "u1", UserActivityStats{TzOffsetMin: intp(120)})
	r.now = utc(20, 20, 30)
	r.move("u3", "u3", "u1", titleTheCarry)
	r.move("u3", "u3", "u2", titleClipper)
	if len(r.sent) != 1 || !strings.HasPrefix(r.sent[0], "u2|") {
		t.Fatalf("u1 is in quiet hours, u2 is not: %v", r.sent)
	}
	r.now = utc(21, 7, 5) // 09:05 at +02:00
	r.tick()
	if len(r.sent) != 2 || !strings.HasPrefix(r.sent[1], "u1|") {
		t.Fatalf("u1 at 09:05 local: %v", r.sent)
	}
}
