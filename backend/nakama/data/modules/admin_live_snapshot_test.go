package main

import (
	"encoding/json"
	"testing"
)

func seedLiveSnapshotRooms() {
	voiceRoomsMu.Lock()
	defer voiceRoomsMu.Unlock()
	voiceRooms["ch_full"] = &VoiceRoom{
		ChannelID: "ch_full",
		CrewID:    "crew_a",
		Members: map[string]*VoiceMemberState{
			"u2": {UserID: "u2", Username: "b", JoinedAt: 200},
			"u1": {UserID: "u1", Username: "a", JoinedAt: 100},
		},
	}
	// Empty rooms must never appear in the snapshot.
	voiceRooms["ch_empty"] = &VoiceRoom{
		ChannelID: "ch_empty",
		CrewID:    "crew_a",
		Members:   map[string]*VoiceMemberState{},
	}
	voiceRooms["ch_other"] = &VoiceRoom{
		ChannelID: "ch_other",
		CrewID:    "crew_b",
		Members: map[string]*VoiceMemberState{
			"u3": {UserID: "u3", Username: "c", Speaking: true, JoinedAt: 50},
		},
	}
}

func clearLiveSnapshotRooms() {
	voiceRoomsMu.Lock()
	defer voiceRoomsMu.Unlock()
	delete(voiceRooms, "ch_full")
	delete(voiceRooms, "ch_empty")
	delete(voiceRooms, "ch_other")
}

func TestSnapshotActiveVoiceRooms(t *testing.T) {
	seedLiveSnapshotRooms()
	defer clearLiveSnapshotRooms()

	rooms := snapshotActiveVoiceRooms()

	if len(rooms) != 2 {
		t.Fatalf("rooms = %d, want 2 (empty room excluded)", len(rooms))
	}
	// Deterministic order: crew_a before crew_b.
	if rooms[0].ChannelID != "ch_full" || rooms[1].ChannelID != "ch_other" {
		t.Fatalf("order = %s,%s, want ch_full,ch_other",
			rooms[0].ChannelID, rooms[1].ChannelID)
	}
	// Members sort by join time, earliest first.
	if rooms[0].Members[0].UserID != "u1" || rooms[0].Members[1].UserID != "u2" {
		t.Fatalf("members not join-ordered")
	}
	// Speaking state survives the copy.
	if !rooms[1].Members[0].Speaking {
		t.Fatalf("speaking flag lost in copy")
	}
}

func TestLiveSnapshotContract(t *testing.T) {
	seedLiveSnapshotRooms()
	defer clearLiveSnapshotRooms()

	rooms := snapshotActiveVoiceRooms()
	rooms[0].ChannelName = "General"
	snap := &LiveSnapshot{
		VoiceRooms: rooms,
		Streams: []LiveStream{{
			StreamID: "s1", CrewID: "crew_a", StreamerID: "u1",
			StreamerUsername: "a", Title: "T", StartedAt: "2026-03-08T14:00:00Z",
			ViewerCount: 3,
		}},
	}

	out, err := json.Marshal(snap)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	var decoded map[string]any
	if err := json.Unmarshal(out, &decoded); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if _, ok := decoded["voice_rooms"]; !ok {
		t.Fatalf("missing voice_rooms key")
	}
	if _, ok := decoded["streams"]; !ok {
		t.Fatalf("missing streams key")
	}
	vr := decoded["voice_rooms"].([]any)
	m := vr[0].(map[string]any)
	for _, key := range []string{"channel_id", "crew_id", "channel_name", "members"} {
		if _, ok := m[key]; !ok {
			t.Fatalf("room missing key %s", key)
		}
	}
}
