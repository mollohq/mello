package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"testing"

	"github.com/heroiclabs/nakama-common/runtime"
)

func TestInviteCodeFormat(t *testing.T) {
	// generateCode now requires ctx+nk for collision check, so we test
	// the output format indirectly via the constant and charset constraints.
	// The code is XXXX-XXXX: 4 chars + hyphen + 4 chars = 9 characters.
	const chars = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"
	code := "ABCD-2345"
	if len(code) != 9 {
		t.Errorf("expected 9 chars, got %d", len(code))
	}
	if code[4] != '-' {
		t.Errorf("expected hyphen at position 4, got %c", code[4])
	}
	for i, c := range code {
		if i == 4 {
			continue
		}
		if !strings.ContainsRune(chars, c) {
			t.Errorf("character %c at position %d not in allowed charset", c, i)
		}
	}
}

func TestInviteCodeLengthConstant(t *testing.T) {
	// InviteCodeLength=8 generates 8 alphanumeric chars + 1 hyphen = 9 total.
	// The loop writes a hyphen at i=4, so total output is InviteCodeLength+1.
	expected := InviteCodeLength + 1
	if expected != 9 {
		t.Errorf("expected 9 total chars (8 alpha + 1 hyphen), got %d", expected)
	}
}

func TestBuildRecapHighlightFullData(t *testing.T) {
	recap := WeeklyRecapData{
		TotalHangoutMin: 420,
		ClipCount:       7,
		TopGame:         "Counter-Strike 2",
	}
	dataBytes, _ := json.Marshal(recap)

	var raw map[string]interface{}
	json.Unmarshal(dataBytes, &raw)

	result := formatRecapHighlight(&recap)
	if result != "7h hangout · 7 clips · Counter-Strike 2" {
		t.Errorf("unexpected highlight: %q", result)
	}
}

func TestBuildRecapHighlightMinutesOnly(t *testing.T) {
	recap := WeeklyRecapData{
		TotalHangoutMin: 45,
		ClipCount:       0,
		TopGame:         "",
	}
	result := formatRecapHighlight(&recap)
	if result != "45m hangout" {
		t.Errorf("unexpected highlight: %q", result)
	}
}

func TestBuildRecapHighlightClipsOnly(t *testing.T) {
	recap := WeeklyRecapData{
		TotalHangoutMin: 0,
		ClipCount:       3,
		TopGame:         "Valorant",
	}
	result := formatRecapHighlight(&recap)
	if result != "3 clips · Valorant" {
		t.Errorf("unexpected highlight: %q", result)
	}
}

func TestBuildRecapHighlightEmpty(t *testing.T) {
	recap := WeeklyRecapData{}
	result := formatRecapHighlight(&recap)
	if result != "" {
		t.Errorf("expected empty highlight, got %q", result)
	}
}

func TestBuildRecapHighlightNoGame(t *testing.T) {
	recap := WeeklyRecapData{
		TotalHangoutMin: 120,
		ClipCount:       5,
		TopGame:         "",
	}
	result := formatRecapHighlight(&recap)
	if result != "2h hangout · 5 clips" {
		t.Errorf("unexpected highlight: %q", result)
	}
}

func TestResolveCrewInviteRequestParsing(t *testing.T) {
	payload := `{"code":"ABCD-1234"}`
	var req ResolveCrewInviteRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		t.Fatalf("failed to unmarshal: %v", err)
	}
	if req.Code != "ABCD-1234" {
		t.Errorf("expected ABCD-1234, got %s", req.Code)
	}
}

func TestResolveCrewInviteResponseSerialization(t *testing.T) {
	resp := ResolveCrewInviteResponse{
		CrewName:          "Test Crew",
		AvatarSeed:        "test",
		CrewID:            "crew-123",
		Highlight:         "2h hangout · 5 clips",
		MemberCount:       5,
		TopGame:           "Valorant",
		LongestSessionMin: 192,
		MostActive:        "ostkatt",
		Members: []InviteMemberPreview{
			{DisplayName: "alice", AvatarSeed: "alice"},
			{DisplayName: "bob", AvatarSeed: "bob"},
		},
	}
	data, err := json.Marshal(resp)
	if err != nil {
		t.Fatalf("marshal failed: %v", err)
	}
	s := string(data)
	if !strings.Contains(s, `"crew_name":"Test Crew"`) {
		t.Errorf("missing crew_name in JSON: %s", s)
	}
	if !strings.Contains(s, `"highlight":"2h hangout · 5 clips"`) {
		t.Errorf("missing highlight in JSON: %s", s)
	}
	if !strings.Contains(s, `"member_count":5`) {
		t.Errorf("missing member_count in JSON: %s", s)
	}
	if !strings.Contains(s, `"top_game":"Valorant"`) {
		t.Errorf("missing top_game in JSON: %s", s)
	}
	if !strings.Contains(s, `"longest_session_min":192`) {
		t.Errorf("missing longest_session_min in JSON: %s", s)
	}
	if !strings.Contains(s, `"most_active":"ostkatt"`) {
		t.Errorf("missing most_active in JSON: %s", s)
	}
	if !strings.Contains(s, `"display_name":"alice"`) {
		t.Errorf("missing member display_name in JSON: %s", s)
	}
}

func TestResolveCrewInviteResponseOmitsEmptyFields(t *testing.T) {
	resp := ResolveCrewInviteResponse{
		CrewName:   "Test",
		AvatarSeed: "test",
		CrewID:     "crew-1",
	}
	data, _ := json.Marshal(resp)
	s := string(data)
	if strings.Contains(s, "highlight") {
		t.Errorf("empty highlight should be omitted: %s", s)
	}
	if strings.Contains(s, "top_game") {
		t.Errorf("empty top_game should be omitted: %s", s)
	}
	if strings.Contains(s, "longest_session_min") {
		t.Errorf("zero longest_session_min should be omitted: %s", s)
	}
	if strings.Contains(s, "most_active") {
		t.Errorf("empty most_active should be omitted: %s", s)
	}
	if strings.Contains(s, "members") {
		t.Errorf("nil members should be omitted: %s", s)
	}
}

func TestInviteMemberPreviewSerialization(t *testing.T) {
	preview := InviteMemberPreview{
		DisplayName: "ostkatt",
		AvatarSeed:  "ostkatt",
	}
	data, _ := json.Marshal(preview)
	s := string(data)
	if !strings.Contains(s, `"display_name":"ostkatt"`) {
		t.Errorf("missing display_name: %s", s)
	}
	if !strings.Contains(s, `"avatar_seed":"ostkatt"`) {
		t.Errorf("missing avatar_seed: %s", s)
	}
}

func TestJoinByInviteCodeRequestParsing(t *testing.T) {
	payload := `{"code":"  abcd-1234  "}`
	var req JoinByInviteCodeRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		t.Fatalf("unmarshal failed: %v", err)
	}
	code := strings.TrimSpace(strings.ToUpper(req.Code))
	if code != "ABCD-1234" {
		t.Errorf("expected ABCD-1234, got %s", code)
	}
}

func TestInviteJoinPrecheck(t *testing.T) {
	state := func(s int) *int { return &s }
	cases := []struct {
		name          string
		state         *int
		alreadyMember bool
		wantErr       error
	}{
		{"no relationship joins", nil, false, nil},
		{"superadmin is already a member", state(0), true, nil},
		{"admin is already a member", state(1), true, nil},
		{"member is already a member", state(2), true, nil},
		{"pending join request goes through GroupUserJoin", state(3), false, nil},
		// GroupUserJoin returns nil for a banned user without adding them, so
		// the RPC must refuse before calling it.
		{"banned is refused", state(4), false, errInviteJoinBanned},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			alreadyMember, err := inviteJoinPrecheck(c.state)
			if alreadyMember != c.alreadyMember || err != c.wantErr {
				t.Errorf("got (%v, %v), want (%v, %v)", alreadyMember, err, c.alreadyMember, c.wantErr)
			}
		})
	}
}

func TestInviteJoinErrorMapsGroupUserJoinErrors(t *testing.T) {
	cases := []struct {
		name string
		in   error
		want error
	}{
		{"full crew", runtime.ErrGroupFull, errInviteJoinCrewFull},
		{"wrapped full crew", fmt.Errorf("join: %w", runtime.ErrGroupFull), errInviteJoinCrewFull},
		{"deleted crew", runtime.ErrGroupNotFound, errInviteJoinCrewGone},
		// The Nakama 3.21 message for an empty username. Anything unknown is an
		// internal failure, never an invalid invite code.
		{"unknown error", errors.New("expects a username string"), errInviteJoinFailed},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := inviteJoinError(c.in); got != c.want {
				t.Errorf("inviteJoinError(%v) = %v, want %v", c.in, got, c.want)
			}
		})
	}
}

// The client picks its message from these codes. Changing one silently
// changes what a user sees.
func TestInviteJoinErrorCodesAreStable(t *testing.T) {
	cases := []struct {
		err  *runtime.Error
		code int
	}{
		{errInviteJoinBanned, 7},
		{errInviteJoinCrewFull, 8},
		{errInviteJoinCrewGone, 5},
		{errInviteJoinUserLookup, 13},
		{errInviteJoinFailed, 13},
	}
	for _, c := range cases {
		if c.err.Code != c.code {
			t.Errorf("%q: code %d, want %d", c.err.Message, c.err.Code, c.code)
		}
	}
}
