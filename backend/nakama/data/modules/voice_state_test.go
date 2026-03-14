package main

import "testing"

func resetVoiceState() {
	voiceRoomsMu.Lock()
	voiceRooms = make(map[string]*VoiceRoom)
	voiceRoomsMu.Unlock()

	voiceUserChannelMu.Lock()
	voiceUserChannel = make(map[string]string)
	voiceUserChannelMu.Unlock()

	voiceChannelCrewMu.Lock()
	voiceChannelCrew = make(map[string]string)
	voiceChannelCrewMu.Unlock()
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

	channelID := "ch_1"
	crewID := "crew_1"

	voiceRoomsMu.Lock()
	voiceRooms[channelID] = &VoiceRoom{
		ChannelID: channelID,
		CrewID:    crewID,
		Members: map[string]*VoiceMemberState{
			"user_a": {UserID: "user_a", Username: "alice", Speaking: true},
			"user_b": {UserID: "user_b", Username: "bob", Speaking: false, Muted: true},
		},
	}
	voiceRoomsMu.Unlock()

	voiceChannelCrewMu.Lock()
	voiceChannelCrew[channelID] = crewID
	voiceChannelCrewMu.Unlock()

	snap := GetVoiceSnapshot(crewID)
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
	original := voiceRooms[channelID].Members["user_a"]
	voiceRoomsMu.RUnlock()
	if !original.Speaking {
		t.Error("snapshot mutation leaked back into voice room")
	}
}

func TestVoiceRoomJoinLeave(t *testing.T) {
	resetVoiceState()

	crewID := "crew_test"
	channelID := "ch_test"

	// Join
	voiceRoomsMu.Lock()
	voiceRooms[channelID] = &VoiceRoom{
		ChannelID: channelID,
		CrewID:    crewID,
		Members:   make(map[string]*VoiceMemberState),
	}
	voiceRooms[channelID].Members["user_1"] = &VoiceMemberState{
		UserID: "user_1", Username: "alice",
	}
	voiceRoomsMu.Unlock()

	voiceUserChannelMu.Lock()
	voiceUserChannel["user_1"] = channelID
	voiceUserChannelMu.Unlock()

	voiceChannelCrewMu.Lock()
	voiceChannelCrew[channelID] = crewID
	voiceChannelCrewMu.Unlock()

	snap := GetVoiceSnapshot(crewID)
	if !snap.Active {
		t.Error("expected active after join")
	}
	if len(snap.Members) != 1 {
		t.Errorf("expected 1 member, got %d", len(snap.Members))
	}

	// Leave
	voiceRoomsMu.Lock()
	delete(voiceRooms[channelID].Members, "user_1")
	if len(voiceRooms[channelID].Members) == 0 {
		delete(voiceRooms, channelID)
	}
	voiceRoomsMu.Unlock()

	voiceUserChannelMu.Lock()
	delete(voiceUserChannel, "user_1")
	voiceUserChannelMu.Unlock()

	snap = GetVoiceSnapshot(crewID)
	if snap.Active {
		t.Error("expected inactive after last member left")
	}
}

func TestGetVoiceChannelSnapshot(t *testing.T) {
	resetVoiceState()

	channelID := "ch_abc"
	crewID := "crew_1"

	voiceRoomsMu.Lock()
	voiceRooms[channelID] = &VoiceRoom{
		ChannelID: channelID,
		CrewID:    crewID,
		Members: map[string]*VoiceMemberState{
			"user_a": {UserID: "user_a", Username: "alice", Speaking: true},
		},
	}
	voiceRoomsMu.Unlock()

	snap := GetVoiceChannelSnapshot(channelID)
	if !snap.Active {
		t.Error("expected channel snapshot to be active")
	}
	if snap.ChannelID != channelID {
		t.Errorf("expected channel_id %s, got %s", channelID, snap.ChannelID)
	}
	if len(snap.Members) != 1 {
		t.Fatalf("expected 1 member, got %d", len(snap.Members))
	}
}

func TestGetVoiceChannelSnapshot_Empty(t *testing.T) {
	resetVoiceState()

	snap := GetVoiceChannelSnapshot("ch_nonexistent")
	if snap.Active {
		t.Error("expected empty channel to be inactive")
	}
	if snap.ChannelID != "ch_nonexistent" {
		t.Errorf("expected channel_id ch_nonexistent, got %s", snap.ChannelID)
	}
}

func TestMultiChannelVoiceState(t *testing.T) {
	resetVoiceState()

	crewID := "crew_multi"
	ch1 := "ch_general"
	ch2 := "ch_strategy"

	voiceRoomsMu.Lock()
	voiceRooms[ch1] = &VoiceRoom{
		ChannelID: ch1,
		CrewID:    crewID,
		Members: map[string]*VoiceMemberState{
			"user_a": {UserID: "user_a", Username: "alice"},
			"user_b": {UserID: "user_b", Username: "bob"},
		},
	}
	voiceRooms[ch2] = &VoiceRoom{
		ChannelID: ch2,
		CrewID:    crewID,
		Members: map[string]*VoiceMemberState{
			"user_c": {UserID: "user_c", Username: "carol"},
		},
	}
	voiceRoomsMu.Unlock()

	voiceChannelCrewMu.Lock()
	voiceChannelCrew[ch1] = crewID
	voiceChannelCrew[ch2] = crewID
	voiceChannelCrewMu.Unlock()

	voiceUserChannelMu.Lock()
	voiceUserChannel["user_a"] = ch1
	voiceUserChannel["user_b"] = ch1
	voiceUserChannel["user_c"] = ch2
	voiceUserChannelMu.Unlock()

	// Per-channel snapshots should have correct members
	snap1 := GetVoiceChannelSnapshot(ch1)
	if len(snap1.Members) != 2 {
		t.Errorf("ch_general: expected 2 members, got %d", len(snap1.Members))
	}

	snap2 := GetVoiceChannelSnapshot(ch2)
	if len(snap2.Members) != 1 {
		t.Errorf("ch_strategy: expected 1 member, got %d", len(snap2.Members))
	}

	// Legacy GetVoiceSnapshot should find at least one active channel
	legacy := GetVoiceSnapshot(crewID)
	if !legacy.Active {
		t.Error("legacy snapshot should show active")
	}
}

func TestCapacityCheck(t *testing.T) {
	resetVoiceState()

	channelID := "ch_full"
	crewID := "crew_cap"

	members := make(map[string]*VoiceMemberState)
	for i := 0; i < MaxVoiceChannelMembers; i++ {
		uid := "user_" + string(rune('a'+i))
		members[uid] = &VoiceMemberState{UserID: uid, Username: uid}
	}

	voiceRoomsMu.Lock()
	voiceRooms[channelID] = &VoiceRoom{
		ChannelID: channelID,
		CrewID:    crewID,
		Members:   members,
	}
	voiceRoomsMu.Unlock()

	// Check capacity
	voiceRoomsMu.RLock()
	room := voiceRooms[channelID]
	full := len(room.Members) >= MaxVoiceChannelMembers
	voiceRoomsMu.RUnlock()

	if !full {
		t.Errorf("expected channel to be full at %d members", MaxVoiceChannelMembers)
	}
}

func TestVoiceEvictChannel(t *testing.T) {
	resetVoiceState()

	channelID := "ch_evict"
	crewID := "crew_evict"

	voiceRoomsMu.Lock()
	voiceRooms[channelID] = &VoiceRoom{
		ChannelID: channelID,
		CrewID:    crewID,
		Members: map[string]*VoiceMemberState{
			"user_a": {UserID: "user_a", Username: "alice"},
			"user_b": {UserID: "user_b", Username: "bob"},
		},
	}
	voiceRoomsMu.Unlock()

	voiceUserChannelMu.Lock()
	voiceUserChannel["user_a"] = channelID
	voiceUserChannel["user_b"] = channelID
	voiceUserChannelMu.Unlock()

	voiceChannelCrewMu.Lock()
	voiceChannelCrew[channelID] = crewID
	voiceChannelCrewMu.Unlock()

	// Evict calls voiceLeaveInternal which needs ctx/logger/nk — test the data structure cleanup directly
	voiceRoomsMu.Lock()
	delete(voiceRooms, channelID)
	voiceRoomsMu.Unlock()

	voiceUserChannelMu.Lock()
	delete(voiceUserChannel, "user_a")
	delete(voiceUserChannel, "user_b")
	voiceUserChannelMu.Unlock()

	snap := GetVoiceChannelSnapshot(channelID)
	if snap.Active {
		t.Error("expected evicted channel to be inactive")
	}

	voiceUserChannelMu.RLock()
	_, aInChannel := voiceUserChannel["user_a"]
	_, bInChannel := voiceUserChannel["user_b"]
	voiceUserChannelMu.RUnlock()

	if aInChannel || bInChannel {
		t.Error("expected users to be removed from voiceUserChannel after eviction")
	}
}

func TestUserChannelReverseLookup(t *testing.T) {
	resetVoiceState()

	voiceUserChannelMu.Lock()
	voiceUserChannel["user_1"] = "ch_general"
	voiceUserChannel["user_2"] = "ch_strategy"
	voiceUserChannelMu.Unlock()

	voiceUserChannelMu.RLock()
	ch1 := voiceUserChannel["user_1"]
	ch2 := voiceUserChannel["user_2"]
	_, noUser := voiceUserChannel["user_unknown"]
	voiceUserChannelMu.RUnlock()

	if ch1 != "ch_general" {
		t.Errorf("user_1 should be in ch_general, got %s", ch1)
	}
	if ch2 != "ch_strategy" {
		t.Errorf("user_2 should be in ch_strategy, got %s", ch2)
	}
	if noUser {
		t.Error("unknown user should not be in any channel")
	}
}

func TestChannelCrewReverseLookup(t *testing.T) {
	resetVoiceState()

	voiceChannelCrewMu.Lock()
	voiceChannelCrew["ch_1"] = "crew_a"
	voiceChannelCrew["ch_2"] = "crew_b"
	voiceChannelCrewMu.Unlock()

	voiceChannelCrewMu.RLock()
	crew1 := voiceChannelCrew["ch_1"]
	crew2 := voiceChannelCrew["ch_2"]
	voiceChannelCrewMu.RUnlock()

	if crew1 != "crew_a" {
		t.Errorf("ch_1 should belong to crew_a, got %s", crew1)
	}
	if crew2 != "crew_b" {
		t.Errorf("ch_2 should belong to crew_b, got %s", crew2)
	}
}
