package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"sync"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// recentSignupsTTL matches the dashboard stats cache: mission control polls on a
// ticker and the underlying query is a plain index scan, so a short cache makes
// repeated polls free without making the feed feel stale.
const recentSignupsTTL = 60 * time.Second

const (
	recentSignupsDefaultLimit = 100
	recentSignupsMaxLimit     = 500
)

// notDisabled is Nakama's sentinel for a live row.
//
// disable_time is NOT NULL and defaults to the epoch, so `disable_time IS NULL`
// matches nothing — it does not error, it silently returns an empty feed. Found
// against the local Docker stack; do not "simplify" this back to IS NULL.
const notDisabled = "1970-01-01 00:00:00+00"

// RecentUser is one signup.
//
// DisplayName is the account's display name, falling back to its username when
// unset — roughly half of accounts have no display name, and a feed of blank
// rows is worse than showing the public handle. Nothing more identifying than
// that travels: the payload is mirrored through Cloudflare KV to a phone.
//
// ID is included because the aggregator needs a stable dedupe key to accumulate
// history locally, and it never leaves the operator's own hardware.
type RecentUser struct {
	ID          string `json:"id"`
	DisplayName string `json:"display_name"`
	CreatedAt   int64  `json:"created_at"`
}

// RecentCrew is one crew (Nakama group) creation.
type RecentCrew struct {
	ID        string `json:"id"`
	Name      string `json:"name"`
	Members   int    `json:"members"`
	CreatedAt int64  `json:"created_at"`
}

// RecentSignups is the admin_recent_signups response contract.
type RecentSignups struct {
	Users []RecentUser `json:"users"`
	Crews []RecentCrew `json:"crews"`
}

type recentSignupsRequest struct {
	Limit int `json:"limit"`
}

type recentSignupsCacheEntry struct {
	json string
	at   time.Time
}

var (
	recentSignupsMu    sync.Mutex
	recentSignupsCache = map[int]recentSignupsCacheEntry{}
)

// AdminRecentSignupsRPC returns the most recent users and crews for the internal
// mission control dashboard. Server-to-server only (http_key, never a client
// session), reading directly from Nakama's users and groups tables.
//
// The aggregator keeps its own copy keyed on ID, so "recent" here only has to be
// wide enough to cover the gap between two polls; depth accumulates on the Pi.
func AdminRecentSignupsRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	// Reject client-session calls; only http_key (no user in context) is allowed.
	if uid, _ := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string); uid != "" {
		return "", runtime.NewError("admin_recent_signups is server-to-server only", 7) // PERMISSION_DENIED
	}

	limit, err := parseRecentSignupsLimit(payload)
	if err != nil {
		return "", runtime.NewError(err.Error(), 3) // INVALID_ARGUMENT
	}

	recentSignupsMu.Lock()
	defer recentSignupsMu.Unlock()

	if entry, ok := recentSignupsCache[limit]; ok && time.Since(entry.at) < recentSignupsTTL {
		return entry.json, nil
	}

	signups, err := collectRecentSignups(ctx, db, limit)
	if err != nil {
		logger.Error("admin_recent_signups: collect failed: %v", err)
		// Serve the stale cache rather than blanking the panel.
		if entry, ok := recentSignupsCache[limit]; ok {
			return entry.json, nil
		}
		return "", runtime.NewError("failed to collect recent signups", 13) // INTERNAL
	}

	out, err := json.Marshal(signups)
	if err != nil {
		return "", runtime.NewError("failed to encode recent signups", 13)
	}

	recentSignupsCache[limit] = recentSignupsCacheEntry{json: string(out), at: time.Now()}
	return string(out), nil
}

// parseRecentSignupsLimit reads the optional {"limit": N} payload. An empty
// payload is the common case (the aggregator sends none), a malformed one is an
// error rather than a silent default, and an out-of-range value is clamped so a
// typo cannot ask for the whole table.
func parseRecentSignupsLimit(payload string) (int, error) {
	if payload == "" {
		return recentSignupsDefaultLimit, nil
	}

	var req recentSignupsRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return 0, errInvalidRecentSignupsPayload
	}
	if req.Limit <= 0 {
		return recentSignupsDefaultLimit, nil
	}
	if req.Limit > recentSignupsMaxLimit {
		return recentSignupsMaxLimit, nil
	}
	return req.Limit, nil
}

var errInvalidRecentSignupsPayload = &recentSignupsError{"payload must be JSON like {\"limit\": 100}"}

type recentSignupsError struct{ msg string }

func (e *recentSignupsError) Error() string { return e.msg }

func collectRecentSignups(ctx context.Context, db *sql.DB, limit int) (*RecentSignups, error) {
	out := &RecentSignups{Users: []RecentUser{}, Crews: []RecentCrew{}}

	users, err := collectRecentUsers(ctx, db, limit)
	if err != nil {
		return nil, err
	}
	out.Users = users

	crews, err := collectRecentCrews(ctx, db, limit)
	if err != nil {
		return nil, err
	}
	out.Crews = crews

	return out, nil
}

func collectRecentUsers(ctx context.Context, db *sql.DB, limit int) ([]RecentUser, error) {
	rows, err := db.QueryContext(ctx, `
		SELECT id, COALESCE(NULLIF(display_name, ''), username, ''), create_time
		FROM users
		WHERE id != $1 AND disable_time = $2
		ORDER BY create_time DESC
		LIMIT $3`, systemUserID, notDisabled, limit,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	users := []RecentUser{}
	for rows.Next() {
		var u RecentUser
		var created time.Time
		if err := rows.Scan(&u.ID, &u.DisplayName, &created); err != nil {
			return nil, err
		}
		u.CreatedAt = created.Unix()
		users = append(users, u)
	}
	return users, rows.Err()
}

func collectRecentCrews(ctx context.Context, db *sql.DB, limit int) ([]RecentCrew, error) {
	// edge_count is Nakama's maintained member count for a group, so the member
	// figure costs nothing extra.
	rows, err := db.QueryContext(ctx, `
		SELECT id, name, edge_count, create_time
		FROM groups
		WHERE disable_time = $1
		ORDER BY create_time DESC
		LIMIT $2`, notDisabled, limit,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	crews := []RecentCrew{}
	for rows.Next() {
		var c RecentCrew
		var created time.Time
		if err := rows.Scan(&c.ID, &c.Name, &c.Members, &created); err != nil {
			return nil, err
		}
		c.CreatedAt = created.Unix()
		crews = append(crews, c)
	}
	return crews, rows.Err()
}
