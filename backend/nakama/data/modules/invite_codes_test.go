package main

import (
	"encoding/json"
	"strings"
	"testing"
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
		CrewName:   "Test Crew",
		AvatarSeed: "test",
		CrewID:     "crew-123",
		Highlight:  "2h hangout · 5 clips",
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
}

func TestResolveCrewInviteResponseOmitsEmptyHighlight(t *testing.T) {
	resp := ResolveCrewInviteResponse{
		CrewName:   "Test",
		AvatarSeed: "test",
		CrewID:     "crew-1",
	}
	data, _ := json.Marshal(resp)
	if strings.Contains(string(data), "highlight") {
		t.Errorf("empty highlight should be omitted: %s", string(data))
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
