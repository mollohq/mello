package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"strings"

	"github.com/heroiclabs/nakama-common/runtime"
)

type SearchUsersRequest struct {
	Query string `json:"query"`
}

type SearchUserEntry struct {
	ID          string `json:"id"`
	DisplayName string `json:"display_name"`
	IsFriend    bool   `json:"is_friend"`
}

// SearchUsersRPC returns users matching a display_name query.
// Friends are returned first, then non-friends. Excludes the caller.
func SearchUsersRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req SearchUsersRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	query := strings.TrimSpace(req.Query)
	if len(query) < 2 {
		resp, _ := json.Marshal(map[string]interface{}{"users": []SearchUserEntry{}})
		return string(resp), nil
	}
	queryLower := strings.ToLower(query)

	// Fetch the caller's friends list
	friendSet := make(map[string]bool)
	friends, _, err := nk.FriendsList(ctx, userID, 100, nil, "")
	if err != nil {
		logger.Warn("search_users: failed to list friends: %v", err)
	} else {
		for _, f := range friends {
			if f.GetUser() != nil {
				friendSet[f.GetUser().GetId()] = true
			}
		}
	}

	// Collect matching friends
	var friendResults []SearchUserEntry
	if friends != nil {
		for _, f := range friends {
			u := f.GetUser()
			if u == nil || u.GetId() == userID {
				continue
			}
			dn := u.GetDisplayName()
			if dn == "" {
				dn = u.GetUsername()
			}
			if strings.Contains(strings.ToLower(dn), queryLower) {
				friendResults = append(friendResults, SearchUserEntry{
					ID:          u.GetId(),
					DisplayName: dn,
					IsFriend:    true,
				})
			}
		}
	}

	// Search all users by display name using SQL (Nakama exposes DB).
	// Limit to 20 results total.
	var otherResults []SearchUserEntry
	rows, err := db.QueryContext(ctx,
		`SELECT id, display_name, username FROM users
		 WHERE (LOWER(display_name) LIKE '%' || $1 || '%' OR LOWER(username) LIKE '%' || $1 || '%')
		   AND id != $2 AND disable_time = '1970-01-01 00:00:00 UTC'
		 LIMIT 30`,
		queryLower, userID,
	)
	if err != nil {
		logger.Warn("search_users: SQL query failed: %v", err)
	} else {
		defer rows.Close()
		for rows.Next() {
			var id, displayName, username string
			if err := rows.Scan(&id, &displayName, &username); err != nil {
				continue
			}
			if friendSet[id] {
				continue // already in friend results
			}
			dn := displayName
			if dn == "" {
				dn = username
			}
			otherResults = append(otherResults, SearchUserEntry{
				ID:          id,
				DisplayName: dn,
				IsFriend:    false,
			})
		}
	}

	// Friends first, then others. Cap at 20 total.
	combined := append(friendResults, otherResults...)
	if len(combined) > 20 {
		combined = combined[:20]
	}

	resp, _ := json.Marshal(map[string]interface{}{"users": combined})
	return string(resp), nil
}
