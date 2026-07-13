package main

import (
	"context"
	"database/sql"
	"encoding/json"

	"github.com/heroiclabs/nakama-common/runtime"
)

// DevEnableSfuCrewRPC sets sfu_enabled=true on crew group metadata.
// Clients cannot set group metadata via the REST API; this dev RPC uses
// GroupUpdate server-side (caller must be a crew admin).
func DevEnableSfuCrewRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	if !sfuAuthEnabled() {
		return "", runtime.NewError("SFU auth not configured (set SFU_JWT_PRIVATE_KEY on Nakama)", 9)
	}

	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID string `json:"crew_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil || req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}

	groups, err := nk.GroupsGetId(ctx, []string{req.CrewID})
	if err != nil || len(groups) == 0 {
		return "", runtime.NewError("crew not found", 5)
	}
	group := groups[0]

	metadata := map[string]interface{}{}
	if group.GetMetadata() != "" {
		_ = json.Unmarshal([]byte(group.GetMetadata()), &metadata)
	}
	metadata["sfu_enabled"] = true

	if err := nk.GroupUpdate(
		ctx,
		req.CrewID,
		userID,
		group.GetName(),
		"",
		"",
		group.GetDescription(),
		group.GetAvatarUrl(),
		group.GetOpen().GetValue(),
		metadata,
		int(group.GetMaxCount()),
	); err != nil {
		logger.Warn("dev_enable_sfu_crew: GroupUpdate failed crew=%s user=%s: %v", req.CrewID, userID, err)
		return "", runtime.NewError("failed to enable SFU for crew (must be crew admin)", 7)
	}

	InvalidateCrewState(req.CrewID)
	logger.Info("dev_enable_sfu_crew: crew=%s user=%s", req.CrewID, userID)

	resp, _ := json.Marshal(map[string]interface{}{
		"success": true,
		"crew_id": req.CrewID,
	})
	return string(resp), nil
}

func enableSfuForCrew(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, adminUserID, crewID string) {
	groups, err := nk.GroupsGetId(ctx, []string{crewID})
	if err != nil || len(groups) == 0 {
		logger.Warn("enableSfuForCrew: crew %s not found: %v", crewID, err)
		return
	}
	group := groups[0]

	metadata := map[string]interface{}{}
	if group.GetMetadata() != "" {
		_ = json.Unmarshal([]byte(group.GetMetadata()), &metadata)
	}
	if enabled, _ := metadata["sfu_enabled"].(bool); enabled {
		return
	}
	metadata["sfu_enabled"] = true

	if err := nk.GroupUpdate(
		ctx,
		crewID,
		adminUserID,
		group.GetName(),
		"",
		"",
		group.GetDescription(),
		group.GetAvatarUrl(),
		group.GetOpen().GetValue(),
		metadata,
		int(group.GetMaxCount()),
	); err != nil {
		logger.Warn("enableSfuForCrew: crew=%s: %v", crewID, err)
		return
	}
	InvalidateCrewState(crewID)
}
