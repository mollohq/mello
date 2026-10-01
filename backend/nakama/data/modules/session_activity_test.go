package main

import (
	"context"
	"os"
	"testing"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

func TestDecidePushFollowsTheSpecTable(t *testing.T) {
	desktopActive := sessionActivity{Platform: "desktop", Active: true}
	desktopAway := sessionActivity{Platform: "desktop", Active: false}
	phoneActive := sessionActivity{Platform: "ios", Active: true}
	phoneAway := sessionActivity{Platform: "ios", Active: false}

	cases := []struct {
		name     string
		sessions int
		reports  []sessionActivity
		want     pushAction
	}{
		{"no session", 0, nil, pushNow},
		{"desktop active", 1, []sessionActivity{desktopActive}, pushSkip},
		{"phone in the foreground", 1, []sessionActivity{phoneActive}, pushSkip},
		{"desktop away", 1, []sessionActivity{desktopAway}, pushAfterGrace},
		{"desktop away, phone away", 2, []sessionActivity{desktopAway, phoneAway}, pushAfterGrace},
		{"desktop away, phone active", 2, []sessionActivity{desktopAway, phoneActive}, pushSkip},
		{"only a backgrounded phone", 1, []sessionActivity{phoneAway}, pushNow},
		{"a session that never reported (old client)", 1, nil, pushSkip},
		{"one of two sessions never reported", 2, []sessionActivity{desktopAway}, pushSkip},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := decidePush(c.sessions, c.reports); got != c.want {
				t.Fatalf("decidePush = %s, want %s", got, c.want)
			}
		})
	}
}

func TestSessionActivityFollowsTheSession(t *testing.T) {
	resetSessionCountsForTests()
	resetSessionActivitiesForTests()
	defer resetSessionCountsForTests()
	defer resetSessionActivitiesForTests()

	registerSessionStart("u1")
	if got := pushDecisionFor("u1"); got != pushSkip {
		t.Fatalf("a connected session without a report must block, got %s", got)
	}
	setSessionActivity("u1", "s1", sessionActivity{Platform: "desktop", Active: false})
	if got := pushDecisionFor("u1"); got != pushAfterGrace {
		t.Fatalf("an inactive desktop must hold the push, got %s", got)
	}
	setSessionActivity("u1", "s1", sessionActivity{Platform: "desktop", Active: true})
	if got := pushDecisionFor("u1"); got != pushSkip {
		t.Fatalf("an active desktop must block, got %s", got)
	}

	clearSessionActivity("u1", "s1")
	registerSessionEnd("u1")
	if got := pushDecisionFor("u1"); got != pushNow {
		t.Fatalf("after the session ends the push goes now, got %s", got)
	}
	if len(userSessionActivities("u1")) != 0 {
		t.Fatal("the report must be removed with the session")
	}
}

func TestDesktopGraceDefaultAndOverride(t *testing.T) {
	old := os.Getenv("PUSH_DESKTOP_GRACE_SECS")
	defer os.Setenv("PUSH_DESKTOP_GRACE_SECS", old)

	os.Unsetenv("PUSH_DESKTOP_GRACE_SECS")
	if desktopGrace() != 120*time.Second {
		t.Fatalf("default = %s", desktopGrace())
	}
	os.Setenv("PUSH_DESKTOP_GRACE_SECS", "30")
	if desktopGrace() != 30*time.Second {
		t.Fatalf("override = %s", desktopGrace())
	}
	os.Setenv("PUSH_DESKTOP_GRACE_SECS", "junk")
	if desktopGrace() != 120*time.Second {
		t.Fatal("a bad value falls back to the default")
	}
}

// The held push is rechecked when the grace period ends: sent while the
// desktop is still inactive, dropped once the user is active again.
func TestHeldPushIsRecheckedAfterTheGracePeriod(t *testing.T) {
	resetSessionCountsForTests()
	resetSessionActivitiesForTests()
	defer resetSessionCountsForTests()
	defer resetSessionActivitiesForTests()

	var pending []func()
	var waited []time.Duration
	oldAfter, oldSend := afterFunc, heldPushSend
	defer func() { afterFunc, heldPushSend = oldAfter, oldSend }()
	afterFunc = func(d time.Duration, f func()) { waited = append(waited, d); pending = append(pending, f) }
	var sent []string
	heldPushSend = func(_ context.Context, _ runtime.Logger, _ runtime.NakamaModule, uid string, n pushAlert) {
		sent = append(sent, uid+"/"+n.MessageID)
	}

	registerSessionStart("u1")
	setSessionActivity("u1", "s1", sessionActivity{Platform: "desktop", Active: false})

	schedulePushAfterGrace(testLogger(), nil, "u1", pushAlert{MessageID: "m1"})
	schedulePushAfterGrace(testLogger(), nil, "u1", pushAlert{MessageID: "m2"})
	if len(sent) != 0 || len(waited) != 2 || waited[0] != desktopGrace() {
		t.Fatalf("nothing is sent before the grace period ends: sent=%v waited=%v", sent, waited)
	}

	pending[0]() // still inactive → send
	setSessionActivity("u1", "s1", sessionActivity{Platform: "desktop", Active: true})
	pending[1]() // active again → drop

	if len(sent) != 1 || sent[0] != "u1/m1" {
		t.Fatalf("sent = %v, want only u1/m1", sent)
	}
}
