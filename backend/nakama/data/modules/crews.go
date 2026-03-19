package main

import (
	"context"
	"database/sql"
	"encoding/json"

	"github.com/heroiclabs/nakama-common/api"
	"github.com/heroiclabs/nakama-common/runtime"
)

const (
	MaxCrewMembers  = 6
	MaxCrewsPerUser = 10
)

type CreateCrewRequest struct {
	Name        string `json:"name"`
	Description string `json:"description,omitempty"`
	InviteOnly  bool   `json:"invite_only"`
}

type CreateCrewResponse struct {
	CrewID string `json:"crew_id"`
	Name   string `json:"name"`
}

// DiscoverCrewsRPC lists open crews. Callable without auth via http_key.
func DiscoverCrewsRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	limit := 50
	groups, _, err := nk.GroupsList(ctx, "", "", nil, nil, limit, "")
	if err != nil {
		logger.Error("discover_crews: GroupsList failed: %v", err)
		return "", runtime.NewError("failed to list crews", 13)
	}

	type crewEntry struct {
		ID          string `json:"id"`
		Name        string `json:"name"`
		Description string `json:"description"`
		MemberCount int32  `json:"member_count"`
		MaxMembers  int32  `json:"max_members"`
		Open        bool   `json:"open"`
	}

	var result []crewEntry
	for _, g := range groups {
		if !g.GetOpen().GetValue() {
			continue
		}
		result = append(result, crewEntry{
			ID:          g.GetId(),
			Name:        g.GetName(),
			Description: g.GetDescription(),
			MemberCount: g.GetEdgeCount(),
			MaxMembers:  g.GetMaxCount(),
			Open:        true,
		})
	}

	resp, _ := json.Marshal(map[string]interface{}{"crews": result})
	return string(resp), nil
}

func CreateCrewRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req CreateCrewRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	if len(req.Name) < 2 || len(req.Name) > 32 {
		return "", runtime.NewError("name must be 2-32 characters", 3)
	}

	groups, _, err := nk.UserGroupsList(ctx, userID, 100, nil, "")
	if err != nil {
		return "", runtime.NewError("failed to check user groups", 13)
	}
	if len(groups) >= MaxCrewsPerUser {
		return "", runtime.NewError("maximum crews reached", 9)
	}

	metadata := map[string]interface{}{
		"max_members":    MaxCrewMembers,
		"invite_only":    req.InviteOnly,
		"created_by":     userID,
		"stream_enabled": true,
	}

	group, err := nk.GroupCreate(ctx, userID, req.Name, userID, "en", req.Description, "", !req.InviteOnly, metadata, MaxCrewMembers)
	if err != nil {
		logger.Error("failed to create crew: %v", err)
		return "", runtime.NewError("failed to create crew", 13)
	}

	// Create the default voice channel for this crew
	if err := InitDefaultChannel(ctx, nk, group.Id); err != nil {
		logger.Warn("Failed to create default voice channel for crew %s: %v", group.Id, err)
	}

	resp := CreateCrewResponse{
		CrewID: group.Id,
		Name:   group.Name,
	}
	respJSON, _ := json.Marshal(resp)

	logger.Info("User %s created crew %s (%s)", userID, group.Name, group.Id)
	return string(respJSON), nil
}

func AfterJoinCrew(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, in *api.JoinGroupRequest) error {
	userID, _ := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	logger.Info("User %s joined crew %s", userID, in.GetGroupId())
	return nil
}

func AfterLeaveCrew(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, in *api.LeaveGroupRequest) error {
	userID, _ := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	logger.Info("User %s left crew %s", userID, in.GetGroupId())
	return nil
}
