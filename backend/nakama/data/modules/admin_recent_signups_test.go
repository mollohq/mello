package main

import (
	"encoding/json"
	"testing"
)

func TestParseRecentSignupsLimit(t *testing.T) {
	// The aggregator sends no payload at all, which must not be an error.
	if got, err := parseRecentSignupsLimit(""); err != nil || got != recentSignupsDefaultLimit {
		t.Fatalf("empty payload = (%d, %v), want (%d, nil)", got, err, recentSignupsDefaultLimit)
	}

	if got, err := parseRecentSignupsLimit(`{"limit":25}`); err != nil || got != 25 {
		t.Fatalf("explicit limit = (%d, %v), want (25, nil)", got, err)
	}

	// Out of range is clamped rather than rejected: a typo should not be able to
	// ask for the whole users table, nor fail the poll outright.
	if got, _ := parseRecentSignupsLimit(`{"limit":100000}`); got != recentSignupsMaxLimit {
		t.Fatalf("over-max = %d, want %d", got, recentSignupsMaxLimit)
	}
	for _, payload := range []string{`{"limit":0}`, `{"limit":-5}`, `{}`} {
		if got, err := parseRecentSignupsLimit(payload); err != nil || got != recentSignupsDefaultLimit {
			t.Fatalf("%s = (%d, %v), want (%d, nil)", payload, got, err, recentSignupsDefaultLimit)
		}
	}

	// Malformed input is an error rather than a silent default, per the RPC
	// input-validation rule in CLAUDE.md.
	if _, err := parseRecentSignupsLimit("not json"); err == nil {
		t.Fatal("malformed payload should error")
	}
}

// The aggregator and the iOS app decode this shape; keep the wire contract
// pinned so a field rename cannot pass silently.
func TestRecentSignupsWireContract(t *testing.T) {
	body, err := json.Marshal(RecentSignups{
		Users: []RecentUser{{ID: "u1", DisplayName: "Kessler", CreatedAt: 1789593257}},
		Crews: []RecentCrew{{ID: "c1", Name: "night crew", Members: 4, CreatedAt: 1789593000}},
	})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}

	var decoded map[string]any
	if err := json.Unmarshal(body, &decoded); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	user := decoded["users"].([]any)[0].(map[string]any)
	for _, key := range []string{"id", "display_name", "created_at"} {
		if _, ok := user[key]; !ok {
			t.Fatalf("user is missing %q: %v", key, user)
		}
	}
	// Usernames are deliberately absent: this payload is mirrored off the
	// operator's hardware, so it carries display names only.
	if _, leaked := user["username"]; leaked {
		t.Fatal("user payload must not carry a username")
	}

	crew := decoded["crews"].([]any)[0].(map[string]any)
	for _, key := range []string{"id", "name", "members", "created_at"} {
		if _, ok := crew[key]; !ok {
			t.Fatalf("crew is missing %q: %v", key, crew)
		}
	}
}

// Empty results must marshal as [] rather than null, so the aggregator can range
// over them without a nil check and the app decodes a list either way.
func TestRecentSignupsEmptyMarshalsAsArrays(t *testing.T) {
	body, err := json.Marshal(&RecentSignups{Users: []RecentUser{}, Crews: []RecentCrew{}})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	if got := string(body); got != `{"users":[],"crews":[]}` {
		t.Fatalf("empty payload = %s", got)
	}
}
