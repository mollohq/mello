package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"

	"github.com/heroiclabs/nakama-common/api"
	"github.com/heroiclabs/nakama-common/runtime"
)

const (
	MaxCrewMembers  = 10000
	MaxCrewsPerUser = 100
)

const (
	CrewAvatarCollection = "crew_avatars"
)

type CreateCrewRequest struct {
	Name          string   `json:"name"`
	Description   string   `json:"description,omitempty"`
	InviteOnly    bool     `json:"invite_only"`
	Avatar        string   `json:"avatar,omitempty"`
	InviteUserIDs []string `json:"invite_user_ids,omitempty"`
}

type CreateCrewResponse struct {
	CrewID     string `json:"crew_id"`
	Name       string `json:"name"`
	InviteCode string `json:"invite_code,omitempty"`
}

// DiscoverCrewsRPC lists open crews. Callable without auth via http_key.
// Accepts optional {"cursor":"..."} for pagination.
func DiscoverCrewsRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	var req struct {
		Cursor string `json:"cursor"`
	}
	if payload != "" {
		_ = json.Unmarshal([]byte(payload), &req)
	}

	limit := 50
	groups, nextCursor, err := nk.GroupsList(ctx, "", "", nil, nil, limit, req.Cursor)
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
		AvatarURL   string `json:"avatar_url,omitempty"`
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
			AvatarURL:   g.GetAvatarUrl(),
		})
	}

	respMap := map[string]interface{}{"crews": result}
	if nextCursor != "" {
		respMap["cursor"] = nextCursor
	}
	resp, _ := json.Marshal(respMap)
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

	logger.Info("Creating crew name=%q avatar_len=%d invite_count=%d by user=%s", req.Name, len(req.Avatar), len(req.InviteUserIDs), userID)

	group, err := nk.GroupCreate(ctx, userID, req.Name, userID, "en", req.Description, "", !req.InviteOnly, metadata, MaxCrewMembers)
	if err != nil {
		logger.Error("failed to create crew: %v", err)
		return "", runtime.NewError("failed to create crew", 13)
	}
	logger.Info("Created crew group_id=%s name=%q", group.Id, group.Name)

	// Store avatar in Nakama storage and set avatar_url on the group
	if req.Avatar != "" {
		avatarValue, _ := json.Marshal(map[string]string{"data": req.Avatar})
		logger.Info("Storing avatar for crew %s (%d bytes base64)", group.Id, len(req.Avatar))
		_, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{
			{
				Collection:      CrewAvatarCollection,
				Key:             group.Id,
				UserID:          SystemUserID,
				Value:           string(avatarValue),
				PermissionRead:  2,
				PermissionWrite: 0,
			},
		})
		if err != nil {
			logger.Error("Failed to store avatar for crew %s: %v", group.Id, err)
		} else {
			avatarURL := fmt.Sprintf("/v2/storage/%s/%s/%s", CrewAvatarCollection, SystemUserID, group.Id)
			if err := nk.GroupUpdate(ctx, group.Id, userID, "", "", "", "", avatarURL, !req.InviteOnly, nil, 0); err != nil {
				logger.Error("Failed to set avatar_url for crew %s: %v", group.Id, err)
			} else {
				logger.Info("Set avatar_url=%s for crew %s", avatarURL, group.Id)
			}
		}
	}

	// Create the default voice channel for this crew
	if err := InitDefaultChannel(ctx, nk, group.Id); err != nil {
		logger.Warn("Failed to create default voice channel for crew %s: %v", group.Id, err)
	}

	// Generate an invite code for the crew
	inviteCode := ""
	if code, err := GenerateInviteCode(ctx, nk, logger, group.Id); err != nil {
		logger.Warn("Failed to generate invite code for crew %s: %v", group.Id, err)
	} else {
		inviteCode = code
	}

	// Send crew invite notifications to requested users
	if len(req.InviteUserIDs) > 0 {
		content := map[string]interface{}{
			"crew_id":   group.Id,
			"crew_name": group.Name,
		}
		for _, targetID := range req.InviteUserIDs {
			if targetID == userID {
				continue
			}
			if err := nk.NotificationSend(ctx, targetID, "crew_invite", content, 200, "", false); err != nil {
				logger.Warn("Failed to send invite notification to %s for crew %s: %v", targetID, group.Id, err)
			}
		}
	}

	resp := CreateCrewResponse{
		CrewID:     group.Id,
		Name:       group.Name,
		InviteCode: inviteCode,
	}
	respJSON, _ := json.Marshal(resp)

	logger.Info("User %s created crew %s (%s) invite_code=%s invited=%d", userID, group.Name, group.Id, inviteCode, len(req.InviteUserIDs))
	return string(respJSON), nil
}

func GetCrewAvatarRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	var req struct {
		CrewID string `json:"crew_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil || req.CrewID == "" {
		return "", runtime.NewError("invalid request", 3)
	}

	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: CrewAvatarCollection, Key: req.CrewID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return "", runtime.NewError("avatar not found", 5)
	}

	return objects[0].GetValue(), nil
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
