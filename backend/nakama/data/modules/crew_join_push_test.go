package main

import (
	"context"
	"strings"
	"testing"

	"github.com/heroiclabs/nakama-common/api"
	"google.golang.org/protobuf/types/known/wrapperspb"
)

func TestCrewJoinPushBodyCopy(t *testing.T) {
	cases := []struct {
		names []string
		hi    bool
		want  string
	}{
		{[]string{"Jonas"}, true, "Jonas joined Night Owls. Say hi."},
		{[]string{"Jonas"}, false, "Jonas joined Night Owls."},
		{[]string{"Max", "Jonas"}, false, "Jonas and 1 other joined Night Owls."},
		{[]string{"Sara", "Max", "Jonas"}, false, "Jonas and 2 others joined Night Owls."},
		{nil, false, ""},
	}
	for _, c := range cases {
		if got := crewJoinPushBody(c.names, "Night Owls", c.hi); got != c.want {
			t.Errorf("got %q want %q", got, c.want)
		}
	}
	if strings.ContainsAny(crewJoinPushBody([]string{cleanPushName("Jo!nas")}, "Crew", true), "!—") {
		t.Fatal("copy rules")
	}
}

func TestCrewJoinMessageIDIsOnePerCrewAndDayUnder64Bytes(t *testing.T) {
	id := crewJoinMessageID("0f6d2b8e-1c3a-4d5e-9f70-aabbccddeeff", utc(20, 23, 59))
	if len(id) > 64 || !strings.HasPrefix(id, "join-") || !strings.HasSuffix(id, "-20261020") {
		t.Fatalf("id %q", id)
	}
	if id == crewJoinMessageID("0f6d2b8e-1c3a-4d5e-9f70-aabbccddee00", utc(20, 1, 0)) {
		t.Fatal("two crews share an id")
	}
}

// joinRig runs queueCrewJoinPush on the fake module.
type joinRig struct {
	*beltRig
}

func newJoinRig(t *testing.T) *joinRig {
	r := &joinRig{newBeltRig(t)}
	r.nk.members["crew"] = nil // the belt rig seeds three members; start empty
	r.nk.groups["crew"] = &api.Group{Id: "crew", Name: "Night Owls"}
	return r
}

// join adds a member and runs the after-join step.
func (r *joinRig) join(id, name string) {
	r.nk.addMember("crew", id, name)
	queueCrewJoinPush(context.Background(), testLogger(), r.nk, "crew", id)
}

func (r *joinRig) tick() {
	flushPushQueue(context.Background(), r.nk, testLogger(), crewJoinPushKey+"crew")
}

func TestCrewJoinPushSaysHiFromTheSecondToTheFifthMember(t *testing.T) {
	r := newJoinRig(t)
	r.join("u1", "Ann")
	if len(r.sent) != 0 {
		t.Fatalf("the creator is alone: %v", r.sent)
	}
	r.join("u2", "Bo")
	if len(r.sent) != 1 || !strings.HasPrefix(r.sent[0], "u1|crew_join|join-") ||
		!strings.HasSuffix(r.sent[0], "|Bo joined Night Owls. Say hi.") {
		t.Fatalf("second member: %v", r.sent)
	}
	r.sent = nil
	r.join("u3", "Cy")
	if len(r.sent) != 2 {
		t.Fatalf("third member: both others are told: %v", r.sent)
	}
	for _, s := range r.sent {
		if strings.HasPrefix(s, "u3|") {
			t.Fatalf("the joiner gets no push: %v", r.sent)
		}
		if !strings.HasSuffix(s, "|Cy joined Night Owls. Say hi.") {
			t.Fatalf("copy: %q", s)
		}
	}
	r.sent = nil
	r.join("u4", "Di")
	r.join("u5", "Ed")
	if len(r.sent) != 3+4 {
		t.Fatalf("fourth and fifth, one push per joiner and member: %d", len(r.sent))
	}
}

func TestCrewJoinPushFromTheSixthMemberHasNoSayHiAndCoalesces(t *testing.T) {
	r := newJoinRig(t)
	for i, n := range []string{"Ann", "Bo", "Cy", "Di", "Ed"} {
		r.nk.addMember("crew", "u"+string(rune('1'+i)), n)
	}
	r.join("u6", "Flo")
	if len(r.sent) != 5 {
		t.Fatalf("sixth member: %v", r.sent)
	}
	for _, s := range r.sent {
		if strings.HasPrefix(s, "u6|") || !strings.HasSuffix(s, "|Flo joined Night Owls.") {
			t.Fatalf("no Say hi from the sixth member: %q", s)
		}
	}
	// Gus, Hal and Ida join in the next 20 minutes. The five old members had
	// a push at 12:00, so theirs are held. Flo, Gus and Hal had none yet.
	r.sent = nil
	r.now = utc(20, 12, 10)
	r.join("u7", "Gus")
	r.now = utc(20, 12, 20)
	r.join("u8", "Hal")
	r.join("u9", "Ida")
	if len(r.sent) != 3 || !strings.HasPrefix(r.sent[0], "u6|") || !strings.HasSuffix(r.sent[0], "|Gus joined Night Owls.") {
		t.Fatalf("only the members without a recent push: %v", r.sent)
	}
	r.sent = nil
	r.now = utc(20, 12, 59)
	r.tick()
	if len(r.sent) != 0 {
		t.Fatalf("the hour has not passed: %v", r.sent)
	}
	r.now = utc(20, 13, 1)
	r.tick()
	if len(r.sent) != 5 {
		t.Fatalf("one push for each of the five: %v", r.sent)
	}
	for _, s := range r.sent {
		if !strings.HasSuffix(s, "|Ida and 2 others joined Night Owls.") {
			t.Fatalf("Gus, Hal and Ida in one push: %q", s)
		}
	}
	r.sent = nil
	r.tick()
	if len(r.sent) != 0 {
		t.Fatalf("each push goes once: %v", r.sent)
	}
	r.now = utc(20, 13, 30)
	r.tick()
	// Flo: Gus was sent at 12:10, Hal and Ida wait. Gus: Hal was sent at 12:20, Ida waits.
	if len(r.sent) != 2 || !strings.HasSuffix(r.sent[0], "|Ida and 1 other joined Night Owls.") ||
		!strings.HasSuffix(r.sent[1], "|Ida joined Night Owls.") {
		t.Fatalf("later windows: %v", r.sent)
	}
}

func TestCrewJoinPushWaitsOutQuietHours(t *testing.T) {
	r := newJoinRig(t)
	r.now = utc(20, 22, 30)
	r.join("u1", "Ann")
	r.join("u2", "Bo")
	if len(r.sent) != 0 {
		t.Fatalf("22:30: %v", r.sent)
	}
	r.now = utc(21, 9, 10)
	r.tick()
	if len(r.sent) != 1 || !strings.HasPrefix(r.sent[0], "u1|") {
		t.Fatalf("09:10: %v", r.sent)
	}
}

func TestCrewJoinPushIgnoresAPendingJoinRequest(t *testing.T) {
	r := newJoinRig(t)
	r.join("u1", "Ann")
	r.join("u2", "Bo")
	r.sent = nil
	// The user is in the list with state 3, a join request.
	r.nk.addMember("crew", "u3", "Cy")
	r.nk.members["crew"][2].State = wrapperspb.Int32(3)
	queueCrewJoinPush(context.Background(), testLogger(), r.nk, "crew", "u3")
	if len(r.sent) != 0 {
		t.Fatalf("a request is no join: %v", r.sent)
	}
}
