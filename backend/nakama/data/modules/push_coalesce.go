package main

// Per-user rate limit and coalescing for remote push (spec 23 §6.3).
//
// The first push to a user goes out at once. Pushes that arrive inside the
// window after it are held, and one push replaces them when the window ends:
// a single held mention unchanged, several mentions as "N new mentions". A busy
// crew therefore buzzes a phone at most once per window.
//
// State lives in memory on the single Nakama instance; a restart drops held
// pushes (best-effort, spec §6.4).

import (
	"context"
	"fmt"
	"sort"
	"strings"
	"sync"
	"time"
)

const (
	pushWindow = 5 * time.Second
	// Users with nothing held and a last push older than the window are
	// forgotten once the map grows past this size.
	pushLimiterSweepAt = 1000
)

type heldMention struct {
	alert pushAlert // the newest mention in this crew
	count int
	seq   uint64 // order of arrival, to find the newest crew
}

type userPushState struct {
	lastSent  time.Time
	held      map[string]*heldMention // crew id -> held mentions
	flushDue  bool
	heldTotal int
}

type pushLimiter struct {
	mu     sync.Mutex
	window time.Duration
	now    func() time.Time
	after  func(time.Duration, func())
	send   func(userID string, a pushAlert)
	users  map[string]*userPushState
	seq    uint64
}

func newPushLimiter(window time.Duration, now func() time.Time, after func(time.Duration, func()), send func(string, pushAlert)) *pushLimiter {
	return &pushLimiter{
		window: window,
		now:    now,
		after:  after,
		send:   send,
		users:  map[string]*userPushState{},
	}
}

// submit sends the alert now, or holds it when the user had a push inside the
// window.
func (l *pushLimiter) submit(userID string, a pushAlert) {
	l.mu.Lock()
	now := l.now()
	l.sweepLocked(now)
	st := l.users[userID]
	if st == nil {
		st = &userPushState{held: map[string]*heldMention{}}
		l.users[userID] = st
	}
	if !st.flushDue && now.Sub(st.lastSent) >= l.window {
		st.lastSent = now
		l.mu.Unlock()
		l.send(userID, a)
		return
	}

	l.seq++
	h := st.held[a.CrewID]
	if h == nil {
		h = &heldMention{}
		st.held[a.CrewID] = h
	}
	h.alert, h.seq = a, l.seq
	h.count++
	st.heldTotal++
	if !st.flushDue {
		st.flushDue = true
		wait := l.window - now.Sub(st.lastSent)
		l.after(wait, func() { l.flush(userID) })
	}
	l.mu.Unlock()
}

// flush sends one push for everything held for the user.
func (l *pushLimiter) flush(userID string) {
	l.mu.Lock()
	st := l.users[userID]
	if st == nil || !st.flushDue {
		l.mu.Unlock()
		return
	}
	alert, ok := coalesceHeld(st.held)
	st.held = map[string]*heldMention{}
	st.heldTotal = 0
	st.flushDue = false
	if ok {
		st.lastSent = l.now()
	}
	l.mu.Unlock()
	if ok {
		l.send(userID, alert)
	}
}

func (l *pushLimiter) sweepLocked(now time.Time) {
	if len(l.users) < pushLimiterSweepAt {
		return
	}
	for id, st := range l.users {
		if !st.flushDue && now.Sub(st.lastSent) >= l.window {
			delete(l.users, id)
		}
	}
}

// coalesceHeld builds the one push that replaces the held mentions. The tap
// target is the crew with the newest mention.
func coalesceHeld(held map[string]*heldMention) (pushAlert, bool) {
	if len(held) == 0 {
		return pushAlert{}, false
	}
	var newest *heldMention
	total := 0
	for _, h := range held {
		total += h.count
		if newest == nil || h.seq > newest.seq {
			newest = h
		}
	}
	if total == 1 {
		return newest.alert, true
	}

	out := newest.alert
	if len(held) == 1 {
		// Title stays the crew name.
		out.Body = fmt.Sprintf("%d new mentions", total)
		return out, true
	}
	// Crew names in arrival order of their newest mention, newest first.
	crews := make([]*heldMention, 0, len(held))
	for _, h := range held {
		crews = append(crews, h)
	}
	sort.Slice(crews, func(i, j int) bool { return crews[i].seq > crews[j].seq })
	names := make([]string, 0, len(crews))
	for _, h := range crews {
		names = append(names, h.alert.Title)
	}
	out.Title = "Mello"
	out.Body = truncateRunes(fmt.Sprintf("%d new mentions in %s", total, strings.Join(names, ", ")), pushBodyMaxRunes)
	return out, true
}

// mentionPushes is the limiter in front of every mention push. Its send runs on
// its own context: a flush fires after the fan-out that held it has returned.
var mentionPushes = newPushLimiter(
	pushWindow,
	time.Now,
	func(d time.Duration, f func()) { afterFunc(d, f) },
	func(userID string, a pushAlert) {
		defer func() {
			if r := recover(); r != nil && globalLogger != nil {
				globalLogger.Error("push: send panic for user %s: %v", userID, r)
			}
		}()
		ctx, cancel := context.WithTimeout(context.Background(), pushFanoutTimeout)
		defer cancel()
		sendPushToUser(ctx, globalLogger, globalNk, userID, a)
	},
)
