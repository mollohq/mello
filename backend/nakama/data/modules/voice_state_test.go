package main

import "testing"

func resetVoiceState() {
	voiceRoomsMu.Lock()
	voiceRooms = make(map[string]*VoiceRoom)
	voiceRoomsMu.Unlock()

	voiceUserCrewMu.Lock()
	voiceUserCrew = make(map[string]string)
	voiceUserCrewMu.Unlock()
}

func TestGetVoiceSnapshot_Empty(t *testing.T) {
	resetVoiceState()

	snap := GetVoiceSnapshot("crew_xyz")
	if snap.Active {
		t.Error("expected empty room to be inactive")
	}
	if len(snap.Members) != 0 {
		t.Errorf("expected 0 members, got %d", len(snap.Members))
	}
}

func TestGetVoiceSnapshot_WithMembers(t *testing.T) {
	resetVoiceState()

	voiceRoomsMu.Lock()
	voiceRooms["crew_1"] = &VoiceRoom{
		CrewID: "crew_1",
		Members: map[string]*VoiceMemberState{
			"user_a": {UserID: "user_a", Username: "alice", Speaking: true},
			"user_b": {UserID: "user_b", Username: "bob", Speaking: false, Muted: true},
		},
	}
	voiceRoomsMu.Unlock()

	snap := GetVoiceSnapshot("crew_1")
	if !snap.Active {
		t.Error("expected room to be active")
	}
	if len(snap.Members) != 2 {
		t.Fatalf("expected 2 members, got %d", len(snap.Members))
	}
	if len(snap.MemberIDs) != 2 {
		t.Fatalf("expected 2 member IDs, got %d", len(snap.MemberIDs))
	}

	// Snapshot should be a copy — mutating it shouldn't affect the room
	snap.Members[0].Speaking = false
	voiceRoomsMu.RLock()
	original := voiceRooms["crew_1"].Members["user_a"]
	voiceRoomsMu.RUnlock()
	if !original.Speaking {
		t.Error("snapshot mutation leaked back into voice room")
	}
}

func TestVoiceRoomJoinLeave(t *testing.T) {
	resetVoiceState()

	crewID := "crew_test"

	// Join
	voiceRoomsMu.Lock()
	voiceRooms[crewID] = &VoiceRoom{
		CrewID:  crewID,
		Members: make(map[string]*VoiceMemberState),
	}
	voiceRooms[crewID].Members["user_1"] = &VoiceMemberState{
		UserID: "user_1", Username: "alice",
	}
	voiceRoomsMu.Unlock()

	voiceUserCrewMu.Lock()
	voiceUserCrew["user_1"] = crewID
	voiceUserCrewMu.Unlock()

	snap := GetVoiceSnapshot(crewID)
	if !snap.Active {
		t.Error("expected active after join")
	}
	if len(snap.Members) != 1 {
		t.Errorf("expected 1 member, got %d", len(snap.Members))
	}

	// Leave
	voiceRoomsMu.Lock()
	delete(voiceRooms[crewID].Members, "user_1")
	if len(voiceRooms[crewID].Members) == 0 {
		delete(voiceRooms, crewID)
	}
	voiceRoomsMu.Unlock()

	voiceUserCrewMu.Lock()
	delete(voiceUserCrew, "user_1")
	voiceUserCrewMu.Unlock()

	snap = GetVoiceSnapshot(crewID)
	if snap.Active {
		t.Error("expected inactive after last member left")
	}
}
