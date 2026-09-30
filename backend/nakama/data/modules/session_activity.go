package main

// Per-session activity for remote push (spec 23 §6.2).
//
// Each client reports whether its user is active on it, over the realtime
// socket, so the report is tied to one session and dies with it. The push
// fan-out reads these reports to decide between no push, a push now, and a
// push after a grace period (desktop open but the user is away).

import (
	"context"
	"database/sql"
	"encoding/json"
	"os"
	"strconv"
	"sync"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

const defaultDesktopGrace = 120 * time.Second

type sessionActivity struct {
	Platform string // "desktop" or "ios"
	Active   bool
}

var (
	sessionActivityMu sync.Mutex
	// userID -> sessionID -> latest report. Entries are removed in OnSessionEnd.
	sessionActivities = map[string]map[string]sessionActivity{}
)

func setSessionActivity(userID, sessionID string, a sessionActivity) {
	sessionActivityMu.Lock()
	defer sessionActivityMu.Unlock()
	m := sessionActivities[userID]
	if m == nil {
		m = map[string]sessionActivity{}
		sessionActivities[userID] = m
	}
	m[sessionID] = a
}

func clearSessionActivity(userID, sessionID string) {
	sessionActivityMu.Lock()
	defer sessionActivityMu.Unlock()
	if m := sessionActivities[userID]; m != nil {
		delete(m, sessionID)
		if len(m) == 0 {
			delete(sessionActivities, userID)
		}
	}
}

func userSessionActivities(userID string) []sessionActivity {
	sessionActivityMu.Lock()
	defer sessionActivityMu.Unlock()
	out := make([]sessionActivity, 0, len(sessionActivities[userID]))
	for _, a := range sessionActivities[userID] {
		out = append(out, a)
	}
	return out
}

func resetSessionActivitiesForTests() {
	sessionActivityMu.Lock()
	defer sessionActivityMu.Unlock()
	sessionActivities = map[string]map[string]sessionActivity{}
}

type setSessionActivityRequest struct {
	Active   bool   `json:"active"`
	Platform string `json:"platform"`
}

// SetSessionActivityRPC records the calling session's activity. It must be
// called over the realtime socket: an HTTP call has no session to attach to.
func SetSessionActivityRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok || userID == "" {
		return "", runtime.NewError("authentication required", 16)
	}
	sessionID, _ := ctx.Value(runtime.RUNTIME_CTX_SESSION_ID).(string)
	if sessionID == "" {
		return "", runtime.NewError("set_session_activity must be called over the realtime socket", 9)
	}
	var req setSessionActivityRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.Platform != "desktop" && req.Platform != "ios" {
		return "", runtime.NewError(`platform must be "desktop" or "ios"`, 3)
	}
	setSessionActivity(userID, sessionID, sessionActivity{Platform: req.Platform, Active: req.Active})
	logger.Debug("push: activity user=%s session=%s platform=%s active=%v", userID, sessionID, req.Platform, req.Active)
	return `{"success":true}`, nil
}

// ---------------------------------------------------------------------------
// Delivery decision (spec 23 §6.2)
// ---------------------------------------------------------------------------

type pushAction int

const (
	pushSkip pushAction = iota
	pushNow
	pushAfterGrace
)

func (a pushAction) String() string {
	switch a {
	case pushNow:
		return "now"
	case pushAfterGrace:
		return "after-grace"
	default:
		return "skip"
	}
}

// decidePush applies the spec 23 §6.2 table. sessions is the user's connected
// session count; reports are the activity reports of those sessions.
func decidePush(sessions int, reports []sessionActivity) pushAction {
	if sessions <= 0 {
		return pushNow
	}
	// A session that never reported counts as active: an older client that
	// cannot report keeps the step 1 behavior (no push while connected).
	if len(reports) < sessions {
		return pushSkip
	}
	desktopInactive := false
	for _, r := range reports {
		if r.Active {
			return pushSkip
		}
		if r.Platform == "desktop" {
			desktopInactive = true
		}
	}
	if desktopInactive {
		return pushAfterGrace
	}
	// Only backgrounded phone sessions: the phone is the target, so no delay.
	return pushNow
}

func sessionCount(userID string) int {
	sessionCountsMu.Lock()
	defer sessionCountsMu.Unlock()
	return sessionCounts[userID]
}

// pushDecisionFor reads the live session state for a user.
func pushDecisionFor(userID string) pushAction {
	return decidePush(sessionCount(userID), userSessionActivities(userID))
}

// desktopGrace is how long an inactive desktop session holds a push back.
// PUSH_DESKTOP_GRACE_SECS overrides the 120 s default.
func desktopGrace() time.Duration {
	if v, err := strconv.Atoi(os.Getenv("PUSH_DESKTOP_GRACE_SECS")); err == nil && v >= 0 {
		return time.Duration(v) * time.Second
	}
	return defaultDesktopGrace
}

// afterFunc is time.AfterFunc; tests replace it to run the recheck at once.
var afterFunc = func(d time.Duration, f func()) { time.AfterFunc(d, f) }
