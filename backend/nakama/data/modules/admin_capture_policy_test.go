package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"sync"
	"testing"

	"github.com/heroiclabs/nakama-common/api"
	"github.com/heroiclabs/nakama-common/runtime"
)

// fakePolicyNk is an in-memory storage with Nakama's version rules: an empty
// version writes always, "*" writes only when nothing is stored, and any
// other version must match the stored one. Other methods panic on the nil
// embedded interface.
type fakePolicyNk struct {
	runtime.NakamaModule

	mu      sync.Mutex
	value   map[string]string
	version map[string]int
	// beforeWrite runs inside StorageWrite, before the version check. A test
	// uses it to store a competing write between the RPC's read and write.
	beforeWrite func()
}

func newFakePolicyNk() *fakePolicyNk {
	return &fakePolicyNk{value: map[string]string{}, version: map[string]int{}}
}

func (f *fakePolicyNk) StorageRead(_ context.Context, reads []*runtime.StorageRead) ([]*api.StorageObject, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	var out []*api.StorageObject
	for _, r := range reads {
		k := fakeStorageKey(r.Collection, r.Key, r.UserID)
		if v, ok := f.value[k]; ok {
			out = append(out, &api.StorageObject{
				Collection: r.Collection, Key: r.Key, UserId: r.UserID,
				Value: v, Version: fmt.Sprintf("v%d", f.version[k]),
			})
		}
	}
	return out, nil
}

func (f *fakePolicyNk) StorageWrite(_ context.Context, writes []*runtime.StorageWrite) ([]*api.StorageObjectAck, error) {
	if f.beforeWrite != nil {
		hook := f.beforeWrite
		f.beforeWrite = nil
		hook()
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	acks := make([]*api.StorageObjectAck, 0, len(writes))
	for _, w := range writes {
		k := fakeStorageKey(w.Collection, w.Key, w.UserID)
		_, exists := f.value[k]
		current := fmt.Sprintf("v%d", f.version[k])
		switch {
		case w.Version == "":
		case w.Version == "*" && exists:
			return nil, errors.New("Storage write rejected - version check failed.")
		case w.Version != "*" && (!exists || w.Version != current):
			return nil, errors.New("Storage write rejected - version check failed.")
		}
		f.value[k] = w.Value
		f.version[k]++
		acks = append(acks, &api.StorageObjectAck{
			Collection: w.Collection, Key: w.Key, UserId: w.UserID,
			Version: fmt.Sprintf("v%d", f.version[k]),
		})
	}
	return acks, nil
}

func policyGet(t *testing.T, nk runtime.NakamaModule) AdminCapturePolicyRead {
	t.Helper()
	out, err := AdminCapturePolicyGetRPC(context.Background(), testLogger(), nil, nk, "{}")
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	var read AdminCapturePolicyRead
	if err := json.Unmarshal([]byte(out), &read); err != nil {
		t.Fatalf("decode get: %v", err)
	}
	return read
}

func policySet(nk runtime.NakamaModule, w AdminCapturePolicyWrite) (AdminCapturePolicySetResponse, error) {
	payload, _ := json.Marshal(w)
	out, err := AdminCapturePolicySetRPC(context.Background(), testLogger(), nil, nk, string(payload))
	var resp AdminCapturePolicySetResponse
	if err == nil {
		_ = json.Unmarshal([]byte(out), &resp)
	}
	return resp, err
}

func errCode(err error) int {
	var re *runtime.Error
	if errors.As(err, &re) {
		return re.Code
	}
	return -1
}

func TestAdminCapturePolicyRefusesClientSession(t *testing.T) {
	ctx := context.WithValue(context.Background(), runtime.RUNTIME_CTX_USER_ID, "user_1")
	nk := newFakePolicyNk()
	if _, err := AdminCapturePolicyGetRPC(ctx, testLogger(), nil, nk, "{}"); errCode(err) != 7 {
		t.Fatalf("get from a client session: want PERMISSION_DENIED, got %v", err)
	}
	if _, err := AdminCapturePolicySetRPC(ctx, testLogger(), nil, nk, "{}"); errCode(err) != 7 {
		t.Fatalf("set from a client session: want PERMISSION_DENIED, got %v", err)
	}
}

func TestAdminCapturePolicyGetWithNothingStoredIsSafeDefault(t *testing.T) {
	read := policyGet(t, newFakePolicyNk())
	if read.Stored || read.Version != "" {
		t.Fatalf("nothing stored: want stored=false version=\"\", got %+v", read)
	}
	if read.Policy.HookEnabled {
		t.Fatal("the default must not enable the hook")
	}
}

func TestAdminCapturePolicySetThenStreamStartReadsIt(t *testing.T) {
	nk := newFakePolicyNk()
	resp, err := policySet(nk, AdminCapturePolicyWrite{
		HookEnabled:  true,
		HookAllowIDs: []uint32{1942, 3, 1942},
		HookDenyIDs:  []uint32{CS2IgdbID},
		Version:      "",
		Actor:        "bob",
	})
	if err != nil {
		t.Fatalf("set: %v", err)
	}
	if !strings.HasSuffix(resp.PolicyVersion, ":admin:bob") {
		t.Fatalf("policy_version must name the actor, got %q", resp.PolicyVersion)
	}
	if resp.Diff.AllowAdded != 2 || resp.Diff.DenyAdded != 1 || resp.Diff.EnabledFrom || !resp.Diff.EnabledTo {
		t.Fatalf("diff: got %+v", resp.Diff)
	}

	// The stream start path reads the same object.
	policy := loadCapturePolicy(context.Background(), nk)
	if !policy.HookEnabled || policy.PolicyVersion != resp.PolicyVersion {
		t.Fatalf("stream start sees %+v", policy)
	}
	if len(policy.HookAllowIDs) != 2 || policy.HookAllowIDs[0] != 3 || policy.HookAllowIDs[1] != 1942 {
		t.Fatalf("allow ids must be sorted and unique, got %v", policy.HookAllowIDs)
	}

	read := policyGet(t, nk)
	if !read.Stored || read.Version != resp.Version {
		t.Fatalf("get after set: want stored with version %q, got %+v", resp.Version, read)
	}
}

func TestAdminCapturePolicySetRefusesStaleVersion(t *testing.T) {
	nk := newFakePolicyNk()
	first, err := policySet(nk, AdminCapturePolicyWrite{HookAllowIDs: []uint32{1}, HookDenyIDs: []uint32{}, Actor: "a"})
	if err != nil {
		t.Fatalf("first set: %v", err)
	}
	if _, err := policySet(nk, AdminCapturePolicyWrite{HookAllowIDs: []uint32{2}, HookDenyIDs: []uint32{}, Version: first.Version, Actor: "b"}); err != nil {
		t.Fatalf("second set: %v", err)
	}
	// A third admin still holds the first version.
	_, err = policySet(nk, AdminCapturePolicyWrite{HookAllowIDs: []uint32{3}, HookDenyIDs: []uint32{}, Version: first.Version, Actor: "c"})
	if errCode(err) != 10 || !strings.Contains(err.Error(), "changed since read") {
		t.Fatalf("stale version: want ABORTED, got %v", err)
	}
	// An admin who read the default cannot overwrite a stored policy.
	_, err = policySet(nk, AdminCapturePolicyWrite{HookAllowIDs: []uint32{4}, HookDenyIDs: []uint32{}, Version: "", Actor: "d"})
	if errCode(err) != 10 {
		t.Fatalf("write over a stored policy from the default: want ABORTED, got %v", err)
	}
	if got := loadCapturePolicy(context.Background(), nk).HookAllowIDs; len(got) != 1 || got[0] != 2 {
		t.Fatalf("the refused writes must not land, got %v", got)
	}
}

func TestAdminCapturePolicySetRefusesARaceBetweenReadAndWrite(t *testing.T) {
	nk := newFakePolicyNk()
	nk.beforeWrite = func() {
		_, _ = nk.StorageWrite(context.Background(), []*runtime.StorageWrite{{
			Collection: CapturePolicyCollection, Key: CapturePolicyKey, UserID: SystemUserID,
			Value: `{"hook_enabled":false,"policy_version":"other","hook_allow_ids":[],"hook_deny_ids":[]}`,
		}})
	}
	_, err := policySet(nk, AdminCapturePolicyWrite{HookAllowIDs: []uint32{1}, HookDenyIDs: []uint32{}, Actor: "a"})
	if errCode(err) != 10 {
		t.Fatalf("a write that lost the race: want ABORTED, got %v", err)
	}
	if v := loadCapturePolicy(context.Background(), nk).PolicyVersion; v != "other" {
		t.Fatalf("the competing write must stay, got %q", v)
	}
}

func TestAdminCapturePolicySetValidates(t *testing.T) {
	cases := []struct {
		name string
		w    AdminCapturePolicyWrite
		want string
	}{
		{"no actor", AdminCapturePolicyWrite{HookAllowIDs: []uint32{}, HookDenyIDs: []uint32{}}, "actor"},
		{"missing list", AdminCapturePolicyWrite{HookAllowIDs: []uint32{1}, Actor: "a"}, "required"},
		{"zero id", AdminCapturePolicyWrite{HookAllowIDs: []uint32{0, 5}, HookDenyIDs: []uint32{}, Actor: "a"}, "id 0"},
		{"both lists", AdminCapturePolicyWrite{HookAllowIDs: []uint32{5}, HookDenyIDs: []uint32{5}, Actor: "a"}, "both lists"},
		{"cs2 allowed", AdminCapturePolicyWrite{HookAllowIDs: []uint32{CS2IgdbID}, HookDenyIDs: []uint32{}, Actor: "a"}, "Counter-Strike 2"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			nk := newFakePolicyNk()
			_, err := policySet(nk, c.w)
			if errCode(err) != 3 || !strings.Contains(err.Error(), c.want) {
				t.Fatalf("want INVALID_ARGUMENT containing %q, got %v", c.want, err)
			}
			if len(nk.value) != 0 {
				t.Fatal("a refused write must store nothing")
			}
		})
	}
}

func TestAdminCapturePolicySetRefusesNegativeIDs(t *testing.T) {
	_, err := AdminCapturePolicySetRPC(context.Background(), testLogger(), nil, newFakePolicyNk(),
		`{"hook_allow_ids":[-1],"hook_deny_ids":[],"actor":"a"}`)
	if errCode(err) != 3 {
		t.Fatalf("negative id: want INVALID_ARGUMENT, got %v", err)
	}
}

func TestDiffCapturePolicyCountsBothDirections(t *testing.T) {
	d := diffCapturePolicy(
		CapturePolicy{HookEnabled: true, HookAllowIDs: []uint32{1, 2, 3}, HookDenyIDs: []uint32{9}},
		CapturePolicy{HookEnabled: false, HookAllowIDs: []uint32{2, 4}, HookDenyIDs: []uint32{9, 10, 11}},
	)
	want := CapturePolicyDiff{EnabledFrom: true, EnabledTo: false, AllowAdded: 1, AllowRemoved: 2, DenyAdded: 2, DenyRemoved: 0}
	if d != want {
		t.Fatalf("diff: got %+v, want %+v", d, want)
	}
}
