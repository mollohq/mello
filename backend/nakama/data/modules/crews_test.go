package main

import (
	"encoding/json"
	"testing"
)

// readSfuEnabled mirrors exactly how hasPremiumCrew (sfu_entitlement.go) reads
// the flag back off a group: unmarshal the stored JSON into a generic map and
// type-assert to bool. Asserting through this shape rather than on the Go map
// directly is the point — the original bug was that creation and the
// entitlement gate disagreed about what was on the group.
func readSfuEnabled(t *testing.T, metadata map[string]interface{}) bool {
	t.Helper()

	raw, err := json.Marshal(metadata)
	if err != nil {
		t.Fatalf("marshal crew metadata: %v", err)
	}
	var decoded map[string]interface{}
	if err := json.Unmarshal(raw, &decoded); err != nil {
		t.Fatalf("unmarshal crew metadata: %v", err)
	}
	enabled, _ := decoded["sfu_enabled"].(bool)
	return enabled
}

// A crew created without sfu_enabled silently runs P2P voice and streaming:
// hasPremiumCrew returns false, voice_join answers mode "p2p", and the client
// joins the mesh without logging anything. Two users hit exactly that and left.
func TestNewCrewMetadataEnablesSFU(t *testing.T) {
	if !readSfuEnabled(t, newCrewMetadata("user-123", true)) {
		t.Error("newly created crews must have sfu_enabled=true, " +
			"otherwise voice and streaming silently fall back to P2P")
	}
	if !readSfuEnabled(t, newCrewMetadata("user-123", false)) {
		t.Error("sfu_enabled must not depend on invite_only")
	}
}

func TestNewCrewMetadataKeepsExistingFields(t *testing.T) {
	meta := newCrewMetadata("user-abc", true)

	if got := meta["created_by"]; got != "user-abc" {
		t.Errorf("created_by = %v, want user-abc", got)
	}
	if got := meta["invite_only"]; got != true {
		t.Errorf("invite_only = %v, want true", got)
	}
	if got := meta["stream_enabled"]; got != true {
		t.Errorf("stream_enabled = %v, want true", got)
	}
	if got := meta["max_members"]; got != MaxCrewMembers {
		t.Errorf("max_members = %v, want %v", got, MaxCrewMembers)
	}

	if meta := newCrewMetadata("user-abc", false); meta["invite_only"] != false {
		t.Errorf("invite_only = %v, want false", meta["invite_only"])
	}
}
