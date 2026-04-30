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
	code, err := generateCode(ctx, nk)
	if err != nil {
		return "", err
	}

	value, _ := json.Marshal(map[string]string{"crew_id": crewID})

	reverseValue, _ := json.Marshal(map[string]string{"code": code})

	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{
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

type InviteMemberPreview struct {
	DisplayName string `json:"display_name"`
	AvatarSeed  string `json:"avatar_seed"`
}

type ResolveCrewInviteResponse struct {
	CrewName          string                `json:"crew_name"`
	AvatarSeed        string                `json:"avatar_seed"`
	CrewID            string                `json:"crew_id"`
	Highlight         string                `json:"highlight,omitempty"`
	MemberCount       int                   `json:"member_count"`
	Members           []InviteMemberPreview `json:"members,omitempty"`
	TopGame           string                `json:"top_game,omitempty"`
	LongestSessionMin int                   `json:"longest_session_min,omitempty"`
	MostActive        string                `json:"most_active,omitempty"`
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

	highlight, recap := buildRecapHighlightWithData(ctx, nk, logger, data.CrewID)

	resp := ResolveCrewInviteResponse{
		CrewName:    group.GetName(),
		AvatarSeed:  group.GetName(),
		CrewID:      data.CrewID,
		Highlight:   highlight,
		MemberCount: int(group.GetEdgeCount()),
	}

	if recap != nil {
		resp.TopGame = recap.TopGame
		resp.LongestSessionMin = recap.LongestSessionMin
		resp.MostActive = recap.MostActive
	}

	members, _, err := nk.GroupUsersList(ctx, data.CrewID, 100, nil, "")
	if err == nil && len(members) > 0 {
		previews := make([]InviteMemberPreview, 0, len(members))
		for _, m := range members {
			u := m.GetUser()
			name := u.GetDisplayName()
			if name == "" {
				name = u.GetUsername()
			}
			previews = append(previews, InviteMemberPreview{
				DisplayName: name,
				AvatarSeed:  name,
			})
		}
		rand.Shuffle(len(previews), func(i, j int) {
			previews[i], previews[j] = previews[j], previews[i]
		})
		if len(previews) > 5 {
			previews = previews[:5]
		}
		resp.Members = previews
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

const maxCodeAttempts = 5

func generateCode(ctx context.Context, nk runtime.NakamaModule) (string, error) {
	r := rand.New(rand.NewSource(time.Now().UnixNano()))
	const chars = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789" // no I/O/0/1 to avoid confusion

	for attempt := 0; attempt < maxCodeAttempts; attempt++ {
		var b strings.Builder
		for i := 0; i < InviteCodeLength; i++ {
			if i == 4 {
				b.WriteByte('-')
			}
			b.WriteByte(chars[r.Intn(len(chars))])
		}
		code := b.String()

		objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
			{Collection: InviteCodeCollection, Key: code, UserID: SystemUserID},
		})
		if err != nil {
			return "", fmt.Errorf("storage read failed: %w", err)
		}
		if len(objects) == 0 {
			return code, nil
		}
	}
	return "", fmt.Errorf("failed to generate unique code after %d attempts", maxCodeAttempts)
}

// MigrateInviteCodesRPC generates invite codes for all crews that don't have one.
// Admin-only: callable via server key with ?unwrap=true&http_key=...
func MigrateInviteCodesRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	cursor := ""
	migrated := 0
	skipped := 0

	for {
		groups, newCursor, err := nk.GroupsList(ctx, "", "", nil, nil, 100, cursor)
		if err != nil {
			return "", fmt.Errorf("failed to list groups: %w", err)
		}

		for _, g := range groups {
			existing := LookupCrewInviteCode(ctx, nk, g.GetId())
			if existing != "" {
				skipped++
				continue
			}
			code, err := GenerateInviteCode(ctx, nk, logger, g.GetId())
			if err != nil {
				logger.Error("migrate_invite_codes: failed for crew %s: %v", g.GetId(), err)
				continue
			}
			logger.Info("migrate_invite_codes: crew %s (%s) → %s", g.GetId(), g.GetName(), code)
			migrated++
		}

		if newCursor == "" || len(groups) == 0 {
			break
		}
		cursor = newCursor
	}

	resp, _ := json.Marshal(map[string]int{
		"migrated": migrated,
		"skipped":  skipped,
	})
	logger.Info("migrate_invite_codes: done. migrated=%d skipped=%d", migrated, skipped)
	return string(resp), nil
}

func buildRecapHighlight(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, crewID string) string {
	hl, _ := buildRecapHighlightWithData(ctx, nk, logger, crewID)
	return hl
}

func buildRecapHighlightWithData(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, crewID string) (string, *WeeklyRecapData) {
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

	if latest == nil {
		return "", nil
	}
	return formatRecapHighlight(latest), latest
}

func formatRecapHighlight(recap *WeeklyRecapData) string {
	if recap.TotalHangoutMin == 0 && recap.ClipCount == 0 {
		return ""
	}

	var parts []string
	if recap.TotalHangoutMin >= 60 {
		parts = append(parts, fmt.Sprintf("%dh hangout", recap.TotalHangoutMin/60))
	} else if recap.TotalHangoutMin > 0 {
		parts = append(parts, fmt.Sprintf("%dm hangout", recap.TotalHangoutMin))
	}
	if recap.ClipCount > 0 {
		parts = append(parts, fmt.Sprintf("%d clips", recap.ClipCount))
	}
	if recap.TopGame != "" {
		parts = append(parts, recap.TopGame)
	}

	return strings.Join(parts, " · ")
}
