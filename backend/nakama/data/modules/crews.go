package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"time"

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
	if code, err := GenerateInviteCode(ctx, nk, logger, group.Id, userID); err != nil {
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

func UpdateCrewRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID       string  `json:"crew_id"`
		Name         string  `json:"name,omitempty"`
		Description  string  `json:"description,omitempty"`
		Avatar       string  `json:"avatar,omitempty"`
		Open         *bool   `json:"open,omitempty"`
		InvitePolicy *string `json:"invite_policy,omitempty"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}

	if !canManageChannels(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not authorized to update crew", 7)
	}

	if req.Name != "" && (len(req.Name) < 2 || len(req.Name) > 32) {
		return "", runtime.NewError("name must be 2-32 characters", 3)
	}

	// Resolve current group state so we can preserve fields we're not changing
	groups, err := nk.GroupsGetId(ctx, []string{req.CrewID})
	if err != nil || len(groups) == 0 {
		return "", runtime.NewError("crew not found", 5)
	}
	group := groups[0]

	name := req.Name
	description := req.Description
	avatarURL := group.GetAvatarUrl()
	open := group.GetOpen().GetValue()
	if req.Open != nil {
		open = *req.Open
	}

	// Store new avatar if provided
	if req.Avatar != "" {
		avatarValue, _ := json.Marshal(map[string]string{"data": req.Avatar})
		_, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{
			{
				Collection:      CrewAvatarCollection,
				Key:             req.CrewID,
				UserID:          SystemUserID,
				Value:           string(avatarValue),
				PermissionRead:  2,
				PermissionWrite: 0,
			},
		})
		if err != nil {
			logger.Error("Failed to store avatar for crew %s: %v", req.CrewID, err)
			return "", runtime.NewError("failed to store avatar", 13)
		}
		avatarURL = fmt.Sprintf("/v2/storage/%s/%s/%s", CrewAvatarCollection, SystemUserID, req.CrewID)
	}

	// Update metadata if invite_policy changed
	var metadata map[string]interface{}
	if req.InvitePolicy != nil {
		p := *req.InvitePolicy
		if p != "admins" && p != "everyone" {
			return "", runtime.NewError("invite_policy must be 'admins' or 'everyone'", 3)
		}
		metadata = map[string]interface{}{}
		if group.GetMetadata() != "" {
			_ = json.Unmarshal([]byte(group.GetMetadata()), &metadata)
		}
		metadata["invite_policy"] = p
	}

	if err := nk.GroupUpdate(ctx, req.CrewID, userID, name, "", "", description, avatarURL, open, metadata, 0); err != nil {
		logger.Error("Failed to update crew %s: %v", req.CrewID, err)
		return "", runtime.NewError("failed to update crew", 13)
	}

	InvalidateCrewState(req.CrewID)

	logger.Info("User %s updated crew %s name=%q", userID, req.CrewID, name)
	return `{"success":true}`, nil
}

func DeleteCrewRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID string `json:"crew_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil || req.CrewID == "" {
		return "", runtime.NewError("invalid request", 3)
	}

	// Only superadmin (state 0) can delete
	members, _, err := nk.GroupUsersList(ctx, req.CrewID, 100, nil, "")
	if err != nil {
		return "", runtime.NewError("failed to check permissions", 13)
	}
	isSuperadmin := false
	for _, m := range members {
		if m.GetUser().GetId() == userID && m.GetState().GetValue() == 0 {
			isSuperadmin = true
			break
		}
	}
	if !isSuperadmin {
		return "", runtime.NewError("only the crew owner can delete", 7)
	}

	if err := nk.GroupDelete(ctx, req.CrewID); err != nil {
		logger.Error("Failed to delete crew %s: %v", req.CrewID, err)
		return "", runtime.NewError("failed to delete crew", 13)
	}

	InvalidateCrewState(req.CrewID)

	logger.Info("User %s deleted crew %s", userID, req.CrewID)
	return `{"success":true}`, nil
}

func ChangeCrewRoleRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	callerID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID   string `json:"crew_id"`
		UserID   string `json:"user_id"`
		NewRole  int    `json:"new_role"` // 1=admin, 2=member
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" || req.UserID == "" {
		return "", runtime.NewError("crew_id and user_id required", 3)
	}
	if req.NewRole < 1 || req.NewRole > 2 {
		return "", runtime.NewError("new_role must be 1 (admin) or 2 (member)", 3)
	}
	if req.UserID == callerID {
		return "", runtime.NewError("cannot change your own role", 3)
	}

	// Only superadmin (state 0) can change roles
	members, _, err := nk.GroupUsersList(ctx, req.CrewID, 100, nil, "")
	if err != nil {
		return "", runtime.NewError("failed to check permissions", 13)
	}
	callerRole := -1
	targetRole := -1
	for _, m := range members {
		uid := m.GetUser().GetId()
		if uid == callerID {
			callerRole = int(m.GetState().GetValue())
		}
		if uid == req.UserID {
			targetRole = int(m.GetState().GetValue())
		}
	}
	if callerRole != 0 {
		return "", runtime.NewError("only the crew owner can change roles", 7)
	}
	if targetRole < 0 {
		return "", runtime.NewError("user is not a crew member", 5)
	}
	if targetRole == 0 {
		return "", runtime.NewError("cannot change the owner's role", 3)
	}

	if req.NewRole < targetRole {
		// Promote: e.g. 2 -> 1
		if err := nk.GroupUsersPromote(ctx, callerID, req.CrewID, []string{req.UserID}); err != nil {
			logger.Error("Failed to promote user %s in crew %s: %v", req.UserID, req.CrewID, err)
			return "", runtime.NewError("failed to promote user", 13)
		}
	} else if req.NewRole > targetRole {
		// Demote: e.g. 1 -> 2
		if err := nk.GroupUsersDemote(ctx, callerID, req.CrewID, []string{req.UserID}); err != nil {
			logger.Error("Failed to demote user %s in crew %s: %v", req.UserID, req.CrewID, err)
			return "", runtime.NewError("failed to demote user", 13)
		}
	}

	InvalidateCrewState(req.CrewID)

	logger.Info("User %s changed role of %s to %d in crew %s", callerID, req.UserID, req.NewRole, req.CrewID)
	return `{"success":true}`, nil
}

func KickCrewMemberRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	callerID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID string `json:"crew_id"`
		UserID string `json:"user_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" || req.UserID == "" {
		return "", runtime.NewError("crew_id and user_id required", 3)
	}
	if req.UserID == callerID {
		return "", runtime.NewError("cannot kick yourself", 3)
	}

	members, _, err := nk.GroupUsersList(ctx, req.CrewID, 100, nil, "")
	if err != nil {
		return "", runtime.NewError("failed to check permissions", 13)
	}
	callerRole := -1
	targetRole := -1
	for _, m := range members {
		uid := m.GetUser().GetId()
		if uid == callerID {
			callerRole = int(m.GetState().GetValue())
		}
		if uid == req.UserID {
			targetRole = int(m.GetState().GetValue())
		}
	}
	if callerRole < 0 || callerRole > 1 {
		return "", runtime.NewError("only admins can kick members", 7)
	}
	if targetRole < 0 {
		return "", runtime.NewError("user is not a crew member", 5)
	}
	if targetRole == 0 {
		return "", runtime.NewError("cannot kick the owner", 3)
	}
	// Admins can only kick members, not other admins
	if callerRole == 1 && targetRole <= 1 {
		return "", runtime.NewError("admins cannot kick other admins", 7)
	}

	if err := nk.GroupUsersKick(ctx, callerID, req.CrewID, []string{req.UserID}); err != nil {
		logger.Error("Failed to kick user %s from crew %s: %v", req.UserID, req.CrewID, err)
		return "", runtime.NewError("failed to kick user", 13)
	}

	InvalidateCrewState(req.CrewID)

	logger.Info("User %s kicked %s from crew %s", callerID, req.UserID, req.CrewID)
	return `{"success":true}`, nil
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
	crewID := in.GetGroupId()
	logger.Info("User %s joined crew %s", userID, crewID)

	displayName := resolveUsername(ctx, nk, userID)
	event := CrewEvent{
		ID:        generateEventID(),
		CrewID:    crewID,
		Type:      "member_joined",
		ActorID:   userID,
		Timestamp: time.Now().UnixMilli(),
		Score:     15,
		Data: MemberJoinedData{
			Username:    displayName,
			DisplayName: displayName,
		},
	}
	if err := AppendCrewEvent(ctx, nk, crewID, event); err != nil {
		logger.Warn("Failed to write member_joined event for crew %s: %v", crewID, err)
	}

	return nil
}

func AfterLeaveCrew(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, in *api.LeaveGroupRequest) error {
	userID, _ := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	crewID := in.GetGroupId()
	logger.Info("User %s left crew %s", userID, crewID)

	displayName := resolveUsername(ctx, nk, userID)
	event := CrewEvent{
		ID:        generateEventID(),
		CrewID:    crewID,
		Type:      "member_left",
		ActorID:   userID,
		Timestamp: time.Now().UnixMilli(),
		Score:     5,
		Data: MemberLeftData{
			Username:    displayName,
			DisplayName: displayName,
		},
	}
	if err := AppendCrewEvent(ctx, nk, crewID, event); err != nil {
		logger.Warn("Failed to write member_left event for crew %s: %v", crewID, err)
	}

	return nil
}
