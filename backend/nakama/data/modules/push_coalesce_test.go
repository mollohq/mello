package main

import (
	"testing"
	"time"
)

type limiterHarness struct {
	clock   time.Time
	timers  []func()
	waits   []time.Duration
	sent    []pushAlert
	limiter *pushLimiter
}

func newLimiterHarness() *limiterHarness {
	h := &limiterHarness{clock: time.Unix(1_790_000_000, 0)}
	h.limiter = newPushLimiter(pushWindow,
		func() time.Time { return h.clock },
		func(d time.Duration, f func()) { h.waits = append(h.waits, d); h.timers = append(h.timers, f) },
		func(_ string, a pushAlert) { h.sent = append(h.sent, a) })
	return h
}

func (h *limiterHarness) fireTimers() {
	timers := h.timers
	h.timers = nil
	for _, f := range timers {
		f()
	}
}

func mention(crewID, crewName, messageID, body string) pushAlert {
	return pushAlert{Type: "mention", CrewID: crewID, ChannelID: "ch-" + crewID, MessageID: messageID, Title: crewName, Body: body}
}

func TestLimiterSendsTheFirstPushAtOnce(t *testing.T) {
	h := newLimiterHarness()
	h.limiter.submit("u1", mention("c1", "Retro", "m1", "bob: @alice hi"))
	if len(h.sent) != 1 || h.sent[0].MessageID != "m1" || len(h.timers) != 0 {
		t.Fatalf("sent=%v timers=%d", h.sent, len(h.timers))
	}
}

func TestLimiterCoalescesMentionsInOneCrew(t *testing.T) {
	h := newLimiterHarness()
	h.limiter.submit("u1", mention("c1", "Retro", "m1", "bob: one"))
	h.clock = h.clock.Add(2 * time.Second)
	h.limiter.submit("u1", mention("c1", "Retro", "m2", "bob: two"))
	h.limiter.submit("u1", mention("c1", "Retro", "m3", "kim: three"))
	h.limiter.submit("u1", mention("c1", "Retro", "m4", "kim: four"))

	if len(h.sent) != 1 {
		t.Fatalf("only the first push goes out inside the window, sent=%d", len(h.sent))
	}
	if len(h.waits) != 1 || h.waits[0] != 3*time.Second {
		t.Fatalf("one flush at the end of the window, waits=%v", h.waits)
	}
	h.clock = h.clock.Add(3 * time.Second)
	h.fireTimers()

	if len(h.sent) != 2 {
		t.Fatalf("one coalesced push after the window, sent=%d", len(h.sent))
	}
	got := h.sent[1]
	if got.Title != "Retro" || got.Body != "3 new mentions" || got.MessageID != "m4" || got.CrewID != "c1" {
		t.Fatalf("coalesced push = %+v", got)
	}
}

func TestLimiterSendsASingleHeldMentionUnchanged(t *testing.T) {
	h := newLimiterHarness()
	h.limiter.submit("u1", mention("c1", "Retro", "m1", "bob: one"))
	held := mention("c2", "Devz", "m2", "kim: @alice look")
	h.limiter.submit("u1", held)
	h.fireTimers()
	if len(h.sent) != 2 || h.sent[1] != held {
		t.Fatalf("a single held mention is sent as is, got %+v", h.sent)
	}
}

func TestLimiterSummarisesSeveralCrewsAndOpensTheNewest(t *testing.T) {
	h := newLimiterHarness()
	h.limiter.submit("u1", mention("c0", "Ops", "m0", "first"))
	h.limiter.submit("u1", mention("c1", "Retro", "m1", "a"))
	h.limiter.submit("u1", mention("c2", "Devz", "m2", "b"))
	h.limiter.submit("u1", mention("c1", "Retro", "m3", "c"))
	h.fireTimers()

	got := h.sent[len(h.sent)-1]
	if got.Title != "Mello" || got.Body != "3 new mentions in Retro, Devz" {
		t.Fatalf("summary = %q / %q", got.Title, got.Body)
	}
	if got.CrewID != "c1" || got.MessageID != "m3" {
		t.Fatalf("the tap opens the crew of the newest mention, got %+v", got)
	}
}

func TestLimiterLimitsEachUserSeparately(t *testing.T) {
	h := newLimiterHarness()
	h.limiter.submit("u1", mention("c1", "Retro", "m1", "x"))
	h.limiter.submit("u2", mention("c1", "Retro", "m1", "x"))
	if len(h.sent) != 2 {
		t.Fatalf("two users each get their first push, sent=%d", len(h.sent))
	}
}

func TestLimiterSendsAtOnceAgainAfterAQuietWindow(t *testing.T) {
	h := newLimiterHarness()
	h.limiter.submit("u1", mention("c1", "Retro", "m1", "x"))
	h.clock = h.clock.Add(pushWindow)
	h.limiter.submit("u1", mention("c1", "Retro", "m2", "y"))
	if len(h.sent) != 2 || len(h.timers) != 0 {
		t.Fatalf("after a quiet window the next push goes at once, sent=%d timers=%d", len(h.sent), len(h.timers))
	}
}

func TestLimiterHoldsAMentionThatArrivesAfterAFlush(t *testing.T) {
	h := newLimiterHarness()
	h.limiter.submit("u1", mention("c1", "Retro", "m1", "x"))
	h.limiter.submit("u1", mention("c1", "Retro", "m2", "y"))
	h.clock = h.clock.Add(pushWindow)
	h.fireTimers() // the flush counts as a push
	h.clock = h.clock.Add(time.Second)
	h.limiter.submit("u1", mention("c1", "Retro", "m3", "z"))
	if len(h.sent) != 2 || len(h.timers) != 1 {
		t.Fatalf("a mention right after a flush waits for the window, sent=%d timers=%d", len(h.sent), len(h.timers))
	}
}
