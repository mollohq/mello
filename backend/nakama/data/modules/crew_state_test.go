package main

import (
	"encoding/json"
	"testing"
)

func TestCrewStateToSidebar(t *testing.T) {
	speaking := true

	state := &CrewState{
		CrewID: "crew_1",
		Name:   "Test Crew",
		Counts: CrewCounts{Online: 3, Total: 10},
		Members: []*CrewMemberInfo{
			{UserID: "user_a", Username: "alice"},
			{UserID: "user_b", Username: "bob"},
		},
		Voice: &CrewVoiceState{
			Active: true,
			Members: []*VoiceMemberInfo{
				{UserID: "user_a", Username: "alice", Speaking: &speaking},
			},
		},
		Stream:         &CrewStreamState{Active: false},
		RecentMessages: []*MessagePreview{},
	}

	sidebar := state.ToSidebar()

	if sidebar.CrewID != "crew_1" {
		t.Errorf("expected crew_1, got %s", sidebar.CrewID)
	}
	if sidebar.Name != "Test Crew" {
		t.Errorf("expected Test Crew, got %s", sidebar.Name)
	}
	if sidebar.Counts.Online != 3 || sidebar.Counts.Total != 10 {
		t.Errorf("counts mismatch: %+v", sidebar.Counts)
	}
	if sidebar.Idle {
		t.Error("expected not idle when online > 0")
	}

	// Voice should be present but speaking should be stripped
	if sidebar.Voice == nil {
		t.Fatal("expected voice state in sidebar")
	}
	if !sidebar.Voice.Active {
		t.Error("expected voice active in sidebar")
	}
	if len(sidebar.Voice.Members) != 1 {
		t.Fatalf("expected 1 voice member, got %d", len(sidebar.Voice.Members))
	}
	if sidebar.Voice.Members[0].Speaking != nil {
		t.Error("expected speaking to be nil in sidebar voice members")
	}
}

func TestCrewStateToSidebar_Idle(t *testing.T) {
	state := &CrewState{
		CrewID: "crew_idle",
		Name:   "Ghost Crew",
		Counts: CrewCounts{Online: 0, Total: 5},
		Voice:  &CrewVoiceState{Active: false, Members: []*VoiceMemberInfo{}},
	}

	sidebar := state.ToSidebar()
	if !sidebar.Idle {
		t.Error("expected idle when online == 0")
	}
}

func TestInvalidateCrewState(t *testing.T) {
	// Pre-populate cache
	crewStateCacheMu.Lock()
	crewStateCache["crew_a"] = &CrewState{CrewID: "crew_a", Name: "A"}
	crewStateCache["crew_b"] = &CrewState{CrewID: "crew_b", Name: "B"}
	crewStateCacheMu.Unlock()

	InvalidateCrewState("crew_a")

	crewStateCacheMu.RLock()
	_, aExists := crewStateCache["crew_a"]
	_, bExists := crewStateCache["crew_b"]
	crewStateCacheMu.RUnlock()

	if aExists {
		t.Error("expected crew_a to be invalidated")
	}
	if !bExists {
		t.Error("expected crew_b to still be cached")
	}

	// Clean up
	crewStateCacheMu.Lock()
	delete(crewStateCache, "crew_b")
	crewStateCacheMu.Unlock()
}

func TestCrewStateSerialization(t *testing.T) {
	speaking := true
	state := &CrewState{
		CrewID: "crew_1",
		Name:   "Neon Syndicate",
		Counts: CrewCounts{Online: 4, Total: 20},
		Voice: &CrewVoiceState{
			Active: true,
			Members: []*VoiceMemberInfo{
				{UserID: "user_a", Username: "vex_r", Speaking: &speaking},
			},
		},
		Stream: &CrewStreamState{
			Active:          true,
			StreamID:        "stream_1",
			StreamerID:      "user_c",
			StreamerUsername: "k0ji",
			Title:           "PROJECT AVALON",
			ViewerCount:     3,
			ThumbnailURL:    "https://example.com/thumb.jpg",
		},
		RecentMessages: []*MessagePreview{
			{Username: "vex_r", Preview: "yo who has the stash...", Timestamp: "2026-03-08T14:15:00Z"},
		},
		UpdatedAt: "2026-03-08T14:16:00Z",
	}

	data, err := json.Marshal(state)
	if err != nil {
		t.Fatalf("marshal failed: %v", err)
	}

	var parsed CrewState
	if err := json.Unmarshal(data, &parsed); err != nil {
		t.Fatalf("unmarshal failed: %v", err)
	}

	if parsed.CrewID != "crew_1" {
		t.Errorf("crew_id mismatch: %s", parsed.CrewID)
	}
	if parsed.Name != "Neon Syndicate" {
		t.Errorf("name mismatch: %s", parsed.Name)
	}
	if parsed.Voice == nil || !parsed.Voice.Active {
		t.Error("expected voice active")
	}
	if parsed.Stream == nil || parsed.Stream.Title != "PROJECT AVALON" {
		t.Error("expected stream title PROJECT AVALON")
	}
}

func TestRecentMessageBuffer(t *testing.T) {
	// Reset
	crewRecentMsgsMu.Lock()
	crewRecentMsgs = make(map[string][]*MessagePreview)
	crewRecentMsgsMu.Unlock()

	crewID := "crew_test"

	// Add 3 messages — buffer should keep only last 2
	for i, content := range []string{"msg1", "msg2", "msg3"} {
		msg := &MessagePreview{
			Username: "user",
			Preview:  content,
		}
		crewRecentMsgsMu.Lock()
		msgs := crewRecentMsgs[crewID]
		msgs = append(msgs, msg)
		if len(msgs) > 2 {
			msgs = msgs[len(msgs)-2:]
		}
		crewRecentMsgs[crewID] = msgs
		crewRecentMsgsMu.Unlock()
		_ = i
	}

	crewRecentMsgsMu.RLock()
	msgs := crewRecentMsgs[crewID]
	crewRecentMsgsMu.RUnlock()

	if len(msgs) != 2 {
		t.Fatalf("expected 2 messages, got %d", len(msgs))
	}
	if msgs[0].Preview != "msg2" {
		t.Errorf("expected msg2, got %s", msgs[0].Preview)
	}
	if msgs[1].Preview != "msg3" {
		t.Errorf("expected msg3, got %s", msgs[1].Preview)
	}
}
