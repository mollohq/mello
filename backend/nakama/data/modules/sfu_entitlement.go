package main

import (
	"context"
	"encoding/json"

	"github.com/heroiclabs/nakama-common/runtime"
)

// hasPremiumCrew checks if a crew has SFU access.
// Beta: checks "sfu_enabled" flag in group metadata.
// Production: will check credits/subscription system.
func hasPremiumCrew(ctx context.Context, nk runtime.NakamaModule, crewID string) bool {
	groups, err := nk.GroupsGetId(ctx, []string{crewID})
	if err != nil || len(groups) == 0 {
		return false
	}

	metaStr := groups[0].GetMetadata()
	if metaStr == "" {
		return false
	}

	var meta map[string]interface{}
	if err := json.Unmarshal([]byte(metaStr), &meta); err != nil {
		return false
	}

	enabled, _ := meta["sfu_enabled"].(bool)
	return enabled
}
