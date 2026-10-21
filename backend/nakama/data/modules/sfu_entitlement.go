package main

import (
	"context"
	"encoding/json"

	"github.com/heroiclabs/nakama-common/runtime"
)

// errSFUUnavailable is the typed error of start_stream, watch_stream and
// voice_join when the SFU cannot carry the media. The SFU is the only media
// transport, so these RPCs fail instead of falling back. Code 14 is the
// gRPC UNAVAILABLE status.
var errSFUUnavailable = runtime.NewError("SFU_UNAVAILABLE", 14)

// sfuEnabledForCrew reports whether a crew can use the SFU.
// The default is on. The "sfu_enabled": false flag in the group metadata is
// an operator kill switch for one crew. A crew that cannot be read is on,
// because the RPCs that call this function check membership first.
func sfuEnabledForCrew(ctx context.Context, nk runtime.NakamaModule, crewID string) bool {
	groups, err := nk.GroupsGetId(ctx, []string{crewID})
	if err != nil || len(groups) == 0 {
		return true
	}
	return sfuEnabledInMetadata(groups[0].GetMetadata())
}

// sfuEnabledInMetadata reads the kill switch from the stored group metadata.
// Only an explicit boolean false turns the SFU off.
func sfuEnabledInMetadata(metaStr string) bool {
	if metaStr == "" {
		return true
	}
	var meta map[string]interface{}
	if err := json.Unmarshal([]byte(metaStr), &meta); err != nil {
		return true
	}
	enabled, isBool := meta["sfu_enabled"].(bool)
	if !isBool {
		return true
	}
	return enabled
}

// sfuAvailableForCrew reports whether the server can sign SFU tokens and the
// crew kill switch is off.
func sfuAvailableForCrew(ctx context.Context, nk runtime.NakamaModule, crewID string) bool {
	return sfuAuthEnabled() && sfuEnabledForCrew(ctx, nk, crewID)
}
