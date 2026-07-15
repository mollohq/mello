package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"sync"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// systemUserID is Nakama's built-in system user, excluded from all people counts.
const systemUserID = "00000000-0000-0000-0000-000000000000"

// dashboardStatsTTL caches the (moderately expensive) aggregate so repeated
// mission-control polls are effectively free. See MISSION-CONTROL.md §3.2.
const dashboardStatsTTL = 60 * time.Second

// dashboardTZ is the day-boundary timezone for the users_daily sparkline.
const dashboardTZ = "Europe/Stockholm"

type dashboardDailyPoint struct {
	Date  string `json:"date"`
	Count int    `json:"count"`
}

// DashboardStats is the admin_dashboard_stats response contract.
type DashboardStats struct {
	UsersTotal     int                   `json:"users_total"`
	UsersNew24h    int                   `json:"users_new_24h"`
	UsersNew7d     int                   `json:"users_new_7d"`
	UsersActive24h int                   `json:"users_active_24h"`
	CrewsTotal     int                   `json:"crews_total"`
	CrewsNew24h    int                   `json:"crews_new_24h"`
	UsersDaily     []dashboardDailyPoint `json:"users_daily"`
}

var (
	dashboardStatsMu   sync.Mutex
	dashboardStatsJSON string
	dashboardStatsAt   time.Time
)

// AdminDashboardStatsRPC returns growth metrics for the internal mission control
// dashboard. It is server-to-server only (called with http_key, never a client
// session) and reads directly from Nakama's users and groups tables.
func AdminDashboardStatsRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	// Reject client-session calls; only http_key (no user in context) is allowed.
	if uid, _ := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string); uid != "" {
		return "", runtime.NewError("admin_dashboard_stats is server-to-server only", 7) // PERMISSION_DENIED
	}

	dashboardStatsMu.Lock()
	defer dashboardStatsMu.Unlock()

	if dashboardStatsJSON != "" && time.Since(dashboardStatsAt) < dashboardStatsTTL {
		return dashboardStatsJSON, nil
	}

	stats, err := collectDashboardStats(ctx, db)
	if err != nil {
		logger.Error("admin_dashboard_stats: collect failed: %v", err)
		// Serve stale cache rather than blanking the panel if we ever have one.
		if dashboardStatsJSON != "" {
			return dashboardStatsJSON, nil
		}
		return "", runtime.NewError("failed to collect dashboard stats", 13) // INTERNAL
	}

	out, err := json.Marshal(stats)
	if err != nil {
		return "", runtime.NewError("failed to encode dashboard stats", 13)
	}

	dashboardStatsJSON = string(out)
	dashboardStatsAt = time.Now()
	return dashboardStatsJSON, nil
}

func collectDashboardStats(ctx context.Context, db *sql.DB) (*DashboardStats, error) {
	stats := &DashboardStats{UsersDaily: []dashboardDailyPoint{}}

	// User totals and windows in a single scan. update_time is Nakama's proxy for
	// last activity, so users_active_24h is a documented approximation.
	err := db.QueryRowContext(ctx, `
		SELECT
			COUNT(*),
			COUNT(*) FILTER (WHERE create_time > now() - interval '24 hours'),
			COUNT(*) FILTER (WHERE create_time > now() - interval '7 days'),
			COUNT(*) FILTER (WHERE update_time > now() - interval '24 hours')
		FROM users
		WHERE id != $1`, systemUserID,
	).Scan(&stats.UsersTotal, &stats.UsersNew24h, &stats.UsersNew7d, &stats.UsersActive24h)
	if err != nil {
		return nil, err
	}

	// Crew (group) totals.
	err = db.QueryRowContext(ctx, `
		SELECT
			COUNT(*),
			COUNT(*) FILTER (WHERE create_time > now() - interval '24 hours')
		FROM groups`,
	).Scan(&stats.CrewsTotal, &stats.CrewsNew24h)
	if err != nil {
		return nil, err
	}

	daily, err := collectUsersDaily(ctx, db)
	if err != nil {
		return nil, err
	}
	stats.UsersDaily = daily

	return stats, nil
}

// collectUsersDaily returns the last 14 days of cumulative user totals bucketed
// by Europe/Stockholm calendar day. New-user counts per day are grouped in SQL;
// the running (cumulative) total is computed in Go to stay portable across the
// Postgres and CockroachDB backends Nakama supports (no generate_series).
func collectUsersDaily(ctx context.Context, db *sql.DB) ([]dashboardDailyPoint, error) {
	rows, err := db.QueryContext(ctx, `
		SELECT (create_time AT TIME ZONE $1)::date AS day, COUNT(*)
		FROM users
		WHERE id != $2
		GROUP BY day
		ORDER BY day`, dashboardTZ, systemUserID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	perDay := make(map[string]int)
	for rows.Next() {
		var day time.Time
		var count int
		if err := rows.Scan(&day, &count); err != nil {
			return nil, err
		}
		perDay[day.Format("2006-01-02")] = count
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}

	loc, err := time.LoadLocation(dashboardTZ)
	if err != nil {
		loc = time.UTC
	}
	now := time.Now().In(loc)
	today := time.Date(now.Year(), now.Month(), now.Day(), 0, 0, 0, 0, loc)
	return cumulativeDaily(perDay, today), nil
}

// dashboardDailyWindow is how many days of cumulative history the sparkline shows.
const dashboardDailyWindow = 14

// cumulativeDaily turns per-day new-user counts into cumulative totals for the
// last dashboardDailyWindow days ending at today. Days with no signups still
// appear (flat), and the first bar includes everyone created before the window
// so it reads as a running total, not a windowed delta. Pure and DB-free so the
// baseline/running math is unit-testable.
func cumulativeDaily(perDay map[string]int, today time.Time) []dashboardDailyPoint {
	windowStart := today.AddDate(0, 0, -(dashboardDailyWindow - 1))
	windowStartKey := windowStart.Format("2006-01-02")

	baseline := 0
	for dayStr, c := range perDay {
		if dayStr < windowStartKey {
			baseline += c
		}
	}

	out := make([]dashboardDailyPoint, 0, dashboardDailyWindow)
	running := baseline
	for d := windowStart; !d.After(today); d = d.AddDate(0, 0, 1) {
		key := d.Format("2006-01-02")
		running += perDay[key]
		out = append(out, dashboardDailyPoint{Date: key, Count: running})
	}
	return out
}
