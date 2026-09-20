package main

import (
	"encoding/json"
	"os"
	"strings"
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
	if len(policy.HookAllow) != 0 || len(policy.HookDeny) != 0 {
		t.Fatalf("default lists must be empty, got allow=%v deny=%v", policy.HookAllow, policy.HookDeny)
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
	if len(policy.HookAllow) != 0 {
		t.Fatalf("corrupt blob must yield empty allow list, got %v", policy.HookAllow)
	}
}

func TestParseCapturePolicyRoundTrip(t *testing.T) {
	raw, err := json.Marshal(CapturePolicy{
		HookEnabled:   true,
		PolicyVersion: "2026-09-19:2001",
		HookAllow:     []string{"heaven.exe"},
		HookDeny:      []string{"cs2.exe"},
	})
	if err != nil {
		t.Fatalf("marshal policy: %v", err)
	}
	policy := parseCapturePolicy(string(raw))
	if !policy.HookEnabled {
		t.Fatal("stored hook_enabled=true must survive the round trip")
	}
	if policy.PolicyVersion != "2026-09-19:2001" {
		t.Fatalf("policy version: got %q", policy.PolicyVersion)
	}
	if len(policy.HookAllow) != 1 || policy.HookAllow[0] != "heaven.exe" {
		t.Fatalf("allow list: got %v", policy.HookAllow)
	}
	if len(policy.HookDeny) != 1 || policy.HookDeny[0] != "cs2.exe" {
		t.Fatalf("deny list: got %v", policy.HookDeny)
	}
}

func TestParseCapturePolicyMissingListsStayEmpty(t *testing.T) {
	policy := parseCapturePolicy(`{"hook_enabled":true,"policy_version":"v1"}`)
	if !policy.HookEnabled {
		t.Fatal("hook_enabled must parse")
	}
	if policy.HookAllow == nil || policy.HookDeny == nil {
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

func TestHookPolicySeedParsesAndDeniesCS2(t *testing.T) {
	raw, err := os.ReadFile("hook_policy_seed.json")
	if err != nil {
		t.Fatalf("read seed: %v", err)
	}
	policy := parseCapturePolicy(string(raw))
	if policy.PolicyVersion == "" || policy.PolicyVersion == "none" {
		t.Fatalf("seed must carry a version, got %q", policy.PolicyVersion)
	}
	for _, exe := range policy.HookDeny {
		if strings.EqualFold(exe, "cs2.exe") {
			return
		}
	}
	t.Fatalf("seed deny list must contain cs2.exe permanently, got %v", policy.HookDeny)
}
