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
	InviteCodeCollection = "invite_codes"
	InviteCodeLength     = 8
)

// GenerateInviteCode creates a short, human-readable invite code for a crew
// and stores it in Nakama storage. Returns the code.
func GenerateInviteCode(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, crewID, inviterUserID string) (string, error) {
	code, err := generateCode(ctx, nk)
	if err != nil {
		return "", err
	}

	valueMap := map[string]string{"crew_id": crewID}
	if inviterUserID != "" {
		valueMap["inviter_user_id"] = inviterUserID
	}
	value, _ := json.Marshal(valueMap)

	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      InviteCodeCollection,
			Key:             strings.ToUpper(code),
			UserID:          SystemUserID,
			Value:           string(value),
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

// CreateInviteCodeRPC generates a fresh invite code for a crew, tagged with the caller's user ID.
func CreateInviteCodeRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID string `json:"crew_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}

	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	code, err := GenerateInviteCode(ctx, nk, logger, req.CrewID, userID)
	if err != nil {
		logger.Error("create_invite_code: failed for user %s crew %s: %v", userID, req.CrewID, err)
		return "", runtime.NewError("failed to generate invite code", 13)
	}

	logger.Info("User %s created invite code %s for crew %s", userID, code, req.CrewID)
	resp, _ := json.Marshal(map[string]string{"code": code})
	return string(resp), nil
}

type ResolveCrewInviteRequest struct {
	Code string `json:"code"`
}

type InviteMemberPreview struct {
	DisplayName string `json:"display_name"`
	AvatarSeed  string `json:"avatar_seed"`
}

type InviteClipPreview struct {
	ClipType        string  `json:"clip_type"`
	ClipperName     string  `json:"clipper_name"`
	DurationSeconds float64 `json:"duration_seconds"`
	Game            string  `json:"game,omitempty"`
	MediaURL        string  `json:"media_url,omitempty"`
}

type ResolveCrewInviteResponse struct {
	CrewName           string                `json:"crew_name"`
	AvatarSeed         string                `json:"avatar_seed"`
	CrewID             string                `json:"crew_id"`
	Highlight          string                `json:"highlight,omitempty"`
	MemberCount        int                   `json:"member_count"`
	Members            []InviteMemberPreview `json:"members,omitempty"`
	TopGame            string                `json:"top_game,omitempty"`
	LongestSessionMin  int                   `json:"longest_session_min,omitempty"`
	MostActive         string                `json:"most_active,omitempty"`
	InviterDisplayName string                `json:"inviter_display_name,omitempty"`
	InviterAvatarSeed  string                `json:"inviter_avatar_seed,omitempty"`
	RecentClips        []InviteClipPreview   `json:"recent_clips,omitempty"`
	SessionSnapshots   []string              `json:"session_snapshots,omitempty"`
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
		CrewID        string `json:"crew_id"`
		InviterUserID string `json:"inviter_user_id"`
	}
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &data); err != nil || data.CrewID == "" {
		return "", runtime.NewError("invalid invite code", 5)
	}

	groups, err := nk.GroupsGetId(ctx, []string{data.CrewID})
	if err != nil || len(groups) == 0 {
		return "", runtime.NewError("crew not found", 5)
	}
	group := groups[0]

	highlight, recap, ledger := buildRecapHighlightWithData(ctx, nk, logger, data.CrewID)

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

	// Resolve inviter display name
	if data.InviterUserID != "" {
		users, err := nk.UsersGetId(ctx, []string{data.InviterUserID}, nil)
		if err == nil && len(users) > 0 {
			u := users[0]
			name := u.GetDisplayName()
			if name == "" {
				name = u.GetUsername()
			}
			resp.InviterDisplayName = name
			resp.InviterAvatarSeed = name
		}
	}

	// Extract recent clips and session snapshots from ledger
	if ledger != nil {
		clips := make([]InviteClipPreview, 0, 4)
		snapshots := make([]string, 0, 8)

		for i := len(ledger.Events) - 1; i >= 0; i-- {
			e := ledger.Events[i]
			if e.Type == "clip" && len(clips) < 4 {
				dataBytes, _ := json.Marshal(e.Data)
				var cd ClipData
				if json.Unmarshal(dataBytes, &cd) == nil && cd.MediaURL != "" {
					clips = append(clips, InviteClipPreview{
						ClipType:        cd.ClipType,
						ClipperName:     cd.ClipperName,
						DurationSeconds: cd.DurationSeconds,
						Game:            cd.Game,
						MediaURL:        cd.MediaURL,
					})
				}
			}
			if e.Type == "stream_session" && len(snapshots) < 8 {
				dataBytes, _ := json.Marshal(e.Data)
				var sd StreamSessionData
				if json.Unmarshal(dataBytes, &sd) == nil && len(sd.SnapshotURLs) > 0 {
					for _, url := range sd.SnapshotURLs {
						if len(snapshots) >= 8 {
							break
						}
						snapshots = append(snapshots, url)
					}
				}
			}
			if len(clips) >= 4 && len(snapshots) >= 8 {
				break
			}
		}

		resp.RecentClips = clips
		resp.SessionSnapshots = snapshots
	}

	respJSON, _ := json.Marshal(resp)
	return string(respJSON), nil
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

func buildRecapHighlight(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, crewID string) string {
	hl, _, _ := buildRecapHighlightWithData(ctx, nk, logger, crewID)
	return hl
}

func buildRecapHighlightWithData(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, crewID string) (string, *WeeklyRecapData, *CrewEventLedger) {
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
		return "", nil, ledger
	}
	return formatRecapHighlight(latest), latest, ledger
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
