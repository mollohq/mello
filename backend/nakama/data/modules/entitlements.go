package main

import (
	"context"
	"encoding/json"
	"fmt"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// m3llo+ entitlement. One record per user, user_plus/{user_id}. Every way to
// get m3llo+ writes it: the quest gift now, a web purchase or an App Store
// subscription later. IsUserPremium reads only it, so the feed and Crew
// history never need to know how a user got m3llo+.

const (
	UserPlusCollection = "user_plus"
	questPlusDays      = 30
)

// UserPlus is user_plus/{user_id}, system-owned. Every way to get m3llo+
// writes it; IsUserPremium reads only it.
type UserPlus struct {
	UntilMs      int64  `json:"until_ms"`
	Source       string `json:"source"`
	QuestGrantMs int64  `json:"quest_grant_ms,omitempty"`
	QuestCrewID  string `json:"quest_crew_id,omitempty"`
}

func readUserPlus(ctx context.Context, nk runtime.NakamaModule, userID string) (*UserPlus, string) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: UserPlusCollection, Key: userID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return nil, ""
	}
	var p UserPlus
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &p); err != nil {
		return nil, ""
	}
	return &p, objects[0].GetVersion()
}

// grantQuestPlus gives a user 30 days of m3llo+ from a crew's quests. A user
// gets one quest grant ever. It reports whether this call granted it.
func grantQuestPlus(ctx context.Context, nk runtime.NakamaModule, userID, crewID string, nowMs int64) (bool, error) {
	for attempt := 0; attempt < 3; attempt++ {
		p, version := readUserPlus(ctx, nk, userID)
		if p != nil && p.QuestGrantMs != 0 {
			return false, nil
		}
		if p == nil {
			p = &UserPlus{}
			version = "*"
		}
		until := nowMs + questPlusDays*dayMs
		if p.UntilMs < until {
			p.UntilMs = until
			p.Source = "quest"
		}
		p.QuestGrantMs = nowMs
		p.QuestCrewID = crewID
		data, _ := json.Marshal(p)
		_, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{{
			Collection: UserPlusCollection, Key: userID, UserID: SystemUserID,
			Value: string(data), Version: version, PermissionRead: 0, PermissionWrite: 0,
		}})
		if err == nil {
			return true, nil
		}
		retryWait(attempt)
	}
	return false, fmt.Errorf("user_plus write failed after 3 retries for user %s", userID)
}

// IsUserPremium reports whether a user has m3llo+ now. It reads user_plus,
// which every source of m3llo+ writes.
func IsUserPremium(ctx context.Context, nk runtime.NakamaModule, userID string) bool {
	p, _ := readUserPlus(ctx, nk, userID)
	return p != nil && p.UntilMs > time.Now().UnixMilli()
}
