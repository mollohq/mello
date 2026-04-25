package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"math/rand"
	"strings"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

const (
	InviteCodeCollection        = "invite_codes"
	CrewInviteCodeCollection    = "crew_invite_codes" // reverse lookup: key=crew_id, value={"code":"..."}
	InviteCodeLength            = 8
)

// GenerateInviteCode creates a short, human-readable invite code for a crew
// and stores it in Nakama storage. Returns the code.
func GenerateInviteCode(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, crewID string) (string, error) {
	code := generateCode()

	value, _ := json.Marshal(map[string]string{"crew_id": crewID})

	reverseValue, _ := json.Marshal(map[string]string{"code": code})

	_, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      InviteCodeCollection,
			Key:             strings.ToUpper(code),
			UserID:          SystemUserID,
			Value:           string(value),
			PermissionRead:  2,
			PermissionWrite: 0,
		},
		{
			Collection:      CrewInviteCodeCollection,
			Key:             crewID,
			UserID:          SystemUserID,
			Value:           string(reverseValue),
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	})
	if err != nil {
		return "", fmt.Errorf("failed to store invite code: %w", err)
	}

	return code, nil
}

type JoinByInviteCodeRequest struct {
	Code string `json:"code"`
}

type JoinByInviteCodeResponse struct {
	CrewID string `json:"crew_id"`
	Name   string `json:"name"`
}

// JoinByInviteCodeRPC resolves an invite code to a crew and joins the caller.
func JoinByInviteCodeRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req JoinByInviteCodeRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	code := strings.TrimSpace(strings.ToUpper(req.Code))
	if code == "" {
		return "", runtime.NewError("invite code required", 3)
	}

	// Look up the code in storage
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: InviteCodeCollection, Key: code, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return "", runtime.NewError("invalid invite code", 5)
	}

	var data struct {
		CrewID string `json:"crew_id"`
	}
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &data); err != nil || data.CrewID == "" {
		return "", runtime.NewError("invalid invite code", 5)
	}

	// Join the group
	if err := nk.GroupUserJoin(ctx, data.CrewID, userID, ""); err != nil {
		logger.Error("join_by_invite_code: GroupUserJoin failed for user %s crew %s: %v", userID, data.CrewID, err)
		return "", runtime.NewError("failed to join crew", 13)
	}

	// Fetch group name for the response
	groups, err := nk.GroupsGetId(ctx, []string{data.CrewID})
	name := ""
	if err == nil && len(groups) > 0 {
		name = groups[0].GetName()
	}

	resp := JoinByInviteCodeResponse{
		CrewID: data.CrewID,
		Name:   name,
	}
	respJSON, _ := json.Marshal(resp)

	logger.Info("User %s joined crew %s via invite code %s", userID, data.CrewID, code)
	return string(respJSON), nil
}

type ResolveCrewInviteRequest struct {
	Code string `json:"code"`
}

type ResolveCrewInviteResponse struct {
	CrewName   string `json:"crew_name"`
	AvatarSeed string `json:"avatar_seed"`
	CrewID     string `json:"crew_id"`
	Highlight  string `json:"highlight,omitempty"`
}

// ResolveCrewInviteRPC returns public crew info for a given invite code.
// Callable with server key (no auth) or with a user session.
func ResolveCrewInviteRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	var req ResolveCrewInviteRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	code := strings.TrimSpace(strings.ToUpper(req.Code))
	if code == "" {
		return "", runtime.NewError("invite code required", 3)
	}

	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: InviteCodeCollection, Key: code, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return "", runtime.NewError("invalid invite code", 5)
	}

	var data struct {
		CrewID string `json:"crew_id"`
	}
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &data); err != nil || data.CrewID == "" {
		return "", runtime.NewError("invalid invite code", 5)
	}

	groups, err := nk.GroupsGetId(ctx, []string{data.CrewID})
	if err != nil || len(groups) == 0 {
		return "", runtime.NewError("crew not found", 5)
	}
	group := groups[0]

	highlight := buildRecapHighlight(ctx, nk, logger, data.CrewID)

	resp := ResolveCrewInviteResponse{
		CrewName:   group.GetName(),
		AvatarSeed: group.GetName(),
		CrewID:     data.CrewID,
		Highlight:  highlight,
	}
	respJSON, _ := json.Marshal(resp)
	return string(respJSON), nil
}

// LookupCrewInviteCode reads the reverse-mapping to find the invite code for a crew.
func LookupCrewInviteCode(ctx context.Context, nk runtime.NakamaModule, crewID string) string {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: CrewInviteCodeCollection, Key: crewID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return ""
	}
	var data struct {
		Code string `json:"code"`
	}
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &data); err != nil {
		return ""
	}
	return data.Code
}

func generateCode() string {
	r := rand.New(rand.NewSource(time.Now().UnixNano()))
	const chars = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789" // no I/O/0/1 to avoid confusion
	var b strings.Builder
	for i := 0; i < InviteCodeLength; i++ {
		if i == 4 {
			b.WriteByte('-')
		}
		b.WriteByte(chars[r.Intn(len(chars))])
	}
	return b.String()
}

func buildRecapHighlight(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, crewID string) string {
	ledger, _ := readLedger(ctx, nk, crewID)

	var latest *WeeklyRecapData
	for i := len(ledger.Events) - 1; i >= 0; i-- {
		if ledger.Events[i].Type != "weekly_recap" {
			continue
		}
		dataBytes, err := json.Marshal(ledger.Events[i].Data)
		if err != nil {
			continue
		}
		var recap WeeklyRecapData
		if err := json.Unmarshal(dataBytes, &recap); err != nil {
			continue
		}
		latest = &recap
		break
	}

	if latest == nil || (latest.TotalHangoutMin == 0 && latest.ClipCount == 0) {
		return ""
	}

	var parts []string
	if latest.TotalHangoutMin >= 60 {
		parts = append(parts, fmt.Sprintf("%dh hangout", latest.TotalHangoutMin/60))
	} else if latest.TotalHangoutMin > 0 {
		parts = append(parts, fmt.Sprintf("%dm hangout", latest.TotalHangoutMin))
	}
	if latest.ClipCount > 0 {
		parts = append(parts, fmt.Sprintf("%d clips", latest.ClipCount))
	}
	if latest.TopGame != "" {
		parts = append(parts, latest.TopGame)
	}

	return strings.Join(parts, " · ")
}
