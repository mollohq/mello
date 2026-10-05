package main

import (
	"encoding/json"
	"os"
	"testing"
)

func TestDefaultCapturePolicyDisablesHook(t *testing.T) {
	policy := defaultCapturePolicy()
	if policy.HookEnabled {
		t.Fatal("default policy must not enable the hook")
	}
	if policy.PolicyVersion == "" {
		t.Fatal("default policy must carry a version so the client can log it")
	}
	if len(policy.HookAllowIDs) != 0 || len(policy.HookDenyIDs) != 0 {
		t.Fatalf("default lists must be empty, got allow=%v deny=%v", policy.HookAllowIDs, policy.HookDenyIDs)
	}
}

func TestParseCapturePolicyEmptyIsSafeDefault(t *testing.T) {
	policy := parseCapturePolicy("")
	if policy.HookEnabled {
		t.Fatal("empty blob must not enable the hook")
	}
}

func TestParseCapturePolicyCorruptIsSafeDefault(t *testing.T) {
	policy := parseCapturePolicy("{not json")
	if policy.HookEnabled {
		t.Fatal("corrupt blob must not enable the hook")
	}
	if len(policy.HookAllowIDs) != 0 || len(policy.HookDenyIDs) != 0 {
		t.Fatalf("corrupt blob must yield empty lists, got allow=%v deny=%v", policy.HookAllowIDs, policy.HookDenyIDs)
	}
}

func TestParseCapturePolicyRoundTrip(t *testing.T) {
	raw, err := json.Marshal(CapturePolicy{
		HookEnabled:   true,
		PolicyVersion: "2026-10-06:test",
		HookAllowIDs:  []uint32{1942},
		HookDenyIDs:   []uint32{CS2IgdbID},
	})
	if err != nil {
		t.Fatalf("marshal policy: %v", err)
	}
	policy := parseCapturePolicy(string(raw))
	if !policy.HookEnabled {
		t.Fatal("stored hook_enabled=true must survive the round trip")
	}
	if policy.PolicyVersion != "2026-10-06:test" {
		t.Fatalf("policy version: got %q", policy.PolicyVersion)
	}
	if len(policy.HookAllowIDs) != 1 || policy.HookAllowIDs[0] != 1942 {
		t.Fatalf("allow list: got %v", policy.HookAllowIDs)
	}
	if len(policy.HookDenyIDs) != 1 || policy.HookDenyIDs[0] != CS2IgdbID {
		t.Fatalf("deny list: got %v", policy.HookDenyIDs)
	}
}

func TestParseCapturePolicyMissingListsStayEmpty(t *testing.T) {
	policy := parseCapturePolicy(`{"hook_enabled":true,"policy_version":"v1"}`)
	if !policy.HookEnabled {
		t.Fatal("hook_enabled must parse")
	}
	if policy.HookAllowIDs == nil || policy.HookDenyIDs == nil {
		t.Fatal("missing lists must become empty slices, not nil, so marshalling stays stable")
	}
}

func TestStartStreamRequestExeIsOptional(t *testing.T) {
	var req StartStreamRequest
	if err := json.Unmarshal([]byte(`{"crew_id":"c1"}`), &req); err != nil {
		t.Fatalf("unmarshal without exe: %v", err)
	}
	if req.Exe != "" {
		t.Fatalf("exe defaults to empty, got %q", req.Exe)
	}
	if err := json.Unmarshal([]byte(`{"crew_id":"c1","exe":"Heaven.exe"}`), &req); err != nil {
		t.Fatalf("unmarshal with exe: %v", err)
	}
	if req.Exe != "Heaven.exe" {
		t.Fatalf("exe: got %q", req.Exe)
	}
}

func TestParseCapturePolicyOldExeBlobNeverHooks(t *testing.T) {
	// A blob stored before the id lists existed: the exe lists are ignored,
	// so the id lists are empty and nothing is hooked.
	policy := parseCapturePolicy(`{"hook_enabled":true,"policy_version":"v0","hook_allow":["witcher3.exe"],"hook_deny":[]}`)
	if len(policy.HookAllowIDs) != 0 || len(policy.HookDenyIDs) != 0 {
		t.Fatalf("an exe-only blob must give empty id lists, got allow=%v deny=%v", policy.HookAllowIDs, policy.HookDenyIDs)
	}
}

func TestStartStreamRequestIgdbIDIsOptional(t *testing.T) {
	var req StartStreamRequest
	if err := json.Unmarshal([]byte(`{"crew_id":"c1"}`), &req); err != nil {
		t.Fatalf("unmarshal without igdb_id: %v", err)
	}
	if req.IgdbID != 0 {
		t.Fatalf("igdb_id defaults to 0, got %d", req.IgdbID)
	}
	if err := json.Unmarshal([]byte(`{"crew_id":"c1","igdb_id":1942}`), &req); err != nil {
		t.Fatalf("unmarshal with igdb_id: %v", err)
	}
	if req.IgdbID != 1942 {
		t.Fatalf("igdb_id: got %d", req.IgdbID)
	}
}

func TestHookPolicySeedParsesAndDeniesCS2(t *testing.T) {
	raw, err := os.ReadFile("hook_policy_seed.json")
	if err != nil {
		t.Fatalf("read seed: %v", err)
	}
	policy := parseCapturePolicy(string(raw))
	if policy.PolicyVersion == "" || policy.PolicyVersion == "none" {
		t.Fatalf("seed must carry a version, got %q", policy.PolicyVersion)
	}
	if policy.HookEnabled {
		t.Fatal("the seed must never turn the hook on")
	}
	if len(policy.HookAllowIDs) == 0 {
		t.Fatal("seed allow list must carry ids")
	}
	for _, id := range policy.HookAllowIDs {
		if id == 0 || id == CS2IgdbID {
			t.Fatalf("seed allow list must not hold %d", id)
		}
	}
	for _, id := range policy.HookDenyIDs {
		if id == CS2IgdbID {
			return
		}
	}
	t.Fatalf("seed deny list must contain Counter-Strike 2 (%d) permanently", CS2IgdbID)
}
