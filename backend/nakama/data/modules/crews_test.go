package main

import (
	"encoding/json"
	"testing"
)

// readSfuEnabled reads the flag back off a group the way sfuEnabledForCrew
// (sfu_entitlement.go) does: from the stored JSON, not from the Go map.
func readSfuEnabled(t *testing.T, metadata map[string]interface{}) bool {
	t.Helper()

	raw, err := json.Marshal(metadata)
	if err != nil {
		t.Fatalf("marshal crew metadata: %v", err)
	}
	return sfuEnabledInMetadata(string(raw))
}

func TestNewCrewMetadataEnablesSFU(t *testing.T) {
	if !readSfuEnabled(t, newCrewMetadata("user-123", true)) {
		t.Error("newly created crews must have the SFU on")
	}
	if !readSfuEnabled(t, newCrewMetadata("user-123", false)) {
		t.Error("sfu_enabled must not depend on invite_only")
	}
}

// The SFU is on unless an operator sets "sfu_enabled": false on the crew.
func TestSfuEnabledInMetadataDefaultsOn(t *testing.T) {
	cases := []struct {
		name string
		meta string
		want bool
	}{
		{"no metadata", "", true},
		{"no flag", `{"invite_only":true}`, true},
		{"flag true", `{"sfu_enabled":true}`, true},
		{"kill switch", `{"sfu_enabled":false}`, false},
		{"flag not a bool", `{"sfu_enabled":"no"}`, true},
		{"corrupt metadata", `{`, true},
	}
	for _, c := range cases {
		if got := sfuEnabledInMetadata(c.meta); got != c.want {
			t.Errorf("%s: sfuEnabledInMetadata(%q) = %v, want %v", c.name, c.meta, got, c.want)
		}
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
