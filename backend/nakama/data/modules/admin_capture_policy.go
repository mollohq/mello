package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"sort"
	"strings"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// Admin read and write of the game capture hook policy (CapturePolicy in
// streaming.go). Both RPCs are server-to-server only: the mello-admin Worker
// calls them with the http_key and writes the audit log. A client session is
// refused.

const (
	// capturePolicyMaxIDs caps each list. The catalogue holds about 2,000
	// games, so this leaves room and still bounds the stored blob.
	capturePolicyMaxIDs = 20000
	// capturePolicyMaxActor bounds the admin name stamped into the version.
	capturePolicyMaxActor = 64
)

// AdminCapturePolicyRead is the admin_capture_policy_get response.
type AdminCapturePolicyRead struct {
	Policy CapturePolicy `json:"policy"`
	// Version is the storage version for a conditional write. Empty when no
	// policy is stored and the safe default is in use.
	Version   string `json:"version"`
	Stored    bool   `json:"stored"`
	UpdatedAt string `json:"updated_at"`
}

// AdminCapturePolicyWrite is the admin_capture_policy_set request.
type AdminCapturePolicyWrite struct {
	HookEnabled  bool     `json:"hook_enabled"`
	HookAllowIDs []uint32 `json:"hook_allow_ids"`
	HookDenyIDs  []uint32 `json:"hook_deny_ids"`
	// Version is the storage version the admin read. Empty means the admin
	// read the safe default, so the write succeeds only if nothing is stored.
	Version string `json:"version"`
	// Actor is the admin console user. It is stamped into policy_version.
	Actor string `json:"actor"`
}

// CapturePolicyDiff tells the audit log what a write changed.
type CapturePolicyDiff struct {
	EnabledFrom  bool `json:"enabled_from"`
	EnabledTo    bool `json:"enabled_to"`
	AllowAdded   int  `json:"allow_added"`
	AllowRemoved int  `json:"allow_removed"`
	DenyAdded    int  `json:"deny_added"`
	DenyRemoved  int  `json:"deny_removed"`
}

// AdminCapturePolicySetResponse is the admin_capture_policy_set response.
type AdminCapturePolicySetResponse struct {
	Policy        CapturePolicy     `json:"policy"`
	Version       string            `json:"version"`
	PolicyVersion string            `json:"policy_version"`
	Diff          CapturePolicyDiff `json:"diff"`
}

func requireServerCaller(ctx context.Context, rpc string) error {
	if uid, _ := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string); uid != "" {
		return runtime.NewError(rpc+" is server-to-server only", 7) // PERMISSION_DENIED
	}
	return nil
}

// readStoredCapturePolicy returns the stored policy, or the safe default with
// stored=false when nothing is stored.
func readStoredCapturePolicy(ctx context.Context, nk runtime.NakamaModule) (AdminCapturePolicyRead, error) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: CapturePolicyCollection, Key: CapturePolicyKey, UserID: SystemUserID},
	})
	if err != nil {
		return AdminCapturePolicyRead{}, err
	}
	if len(objects) == 0 {
		return AdminCapturePolicyRead{Policy: defaultCapturePolicy()}, nil
	}
	obj := objects[0]
	read := AdminCapturePolicyRead{
		Policy:  parseCapturePolicy(obj.GetValue()),
		Version: obj.GetVersion(),
		Stored:  true,
	}
	if ts := obj.GetUpdateTime(); ts != nil {
		read.UpdatedAt = ts.AsTime().UTC().Format(time.RFC3339)
	}
	return read, nil
}

// normalizeCapturePolicyWrite validates a write and returns its sorted,
// de-duplicated lists. Every rule here protects a player: deny wins in the
// client too, but an id on both lists is refused so the stored policy says
// one thing per game.
func normalizeCapturePolicyWrite(w AdminCapturePolicyWrite) (allow, deny []uint32, actor string, err error) {
	actor = strings.TrimSpace(w.Actor)
	if actor == "" {
		return nil, nil, "", fmt.Errorf("actor is required")
	}
	if len(actor) > capturePolicyMaxActor {
		actor = actor[:capturePolicyMaxActor]
	}
	if w.HookAllowIDs == nil || w.HookDenyIDs == nil {
		return nil, nil, "", fmt.Errorf("hook_allow_ids and hook_deny_ids are required")
	}
	if len(w.HookAllowIDs) > capturePolicyMaxIDs || len(w.HookDenyIDs) > capturePolicyMaxIDs {
		return nil, nil, "", fmt.Errorf("a list holds at most %d ids", capturePolicyMaxIDs)
	}
	allow = sortedUniqueIDs(w.HookAllowIDs)
	deny = sortedUniqueIDs(w.HookDenyIDs)
	if len(allow) > 0 && allow[0] == 0 || len(deny) > 0 && deny[0] == 0 {
		return nil, nil, "", fmt.Errorf("id 0 means an unknown game and cannot be on a list")
	}
	denied := make(map[uint32]bool, len(deny))
	for _, id := range deny {
		denied[id] = true
	}
	for _, id := range allow {
		if denied[id] {
			return nil, nil, "", fmt.Errorf("id %d is on both lists", id)
		}
		if id == CS2IgdbID {
			return nil, nil, "", fmt.Errorf("Counter-Strike 2 (%d) is never hooked", CS2IgdbID)
		}
	}
	return allow, deny, actor, nil
}

func sortedUniqueIDs(ids []uint32) []uint32 {
	out := append([]uint32(nil), ids...)
	sort.Slice(out, func(i, j int) bool { return out[i] < out[j] })
	n := 0
	for i, id := range out {
		if i == 0 || id != out[n-1] {
			out[n] = id
			n++
		}
	}
	return out[:n]
}

// idDelta counts the ids added to and removed from a list.
func idDelta(from, to []uint32) (added, removed int) {
	before := make(map[uint32]bool, len(from))
	for _, id := range from {
		before[id] = true
	}
	after := make(map[uint32]bool, len(to))
	for _, id := range to {
		after[id] = true
		if !before[id] {
			added++
		}
	}
	for _, id := range from {
		if !after[id] {
			removed++
		}
	}
	return added, removed
}

func diffCapturePolicy(from, to CapturePolicy) CapturePolicyDiff {
	d := CapturePolicyDiff{EnabledFrom: from.HookEnabled, EnabledTo: to.HookEnabled}
	d.AllowAdded, d.AllowRemoved = idDelta(from.HookAllowIDs, to.HookAllowIDs)
	d.DenyAdded, d.DenyRemoved = idDelta(from.HookDenyIDs, to.HookDenyIDs)
	return d
}

// AdminCapturePolicyGetRPC returns the stored hook policy and its storage
// version, for the admin tool.
func AdminCapturePolicyGetRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	if err := requireServerCaller(ctx, "admin_capture_policy_get"); err != nil {
		return "", err
	}
	read, err := readStoredCapturePolicy(ctx, nk)
	if err != nil {
		logger.Error("admin_capture_policy_get: storage read failed: %v", err)
		return "", runtime.NewError("policy read failed", 13) // INTERNAL
	}
	out, err := json.Marshal(read)
	if err != nil {
		return "", runtime.NewError("encode failed", 13)
	}
	return string(out), nil
}

// AdminCapturePolicySetRPC replaces the stored hook policy. The write is
// conditional on the storage version the admin read, so two admins cannot
// overwrite each other without seeing it. policy_version is stamped here.
func AdminCapturePolicySetRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	if err := requireServerCaller(ctx, "admin_capture_policy_set"); err != nil {
		return "", err
	}
	var w AdminCapturePolicyWrite
	if err := json.Unmarshal([]byte(payload), &w); err != nil {
		return "", runtime.NewError("invalid request: "+err.Error(), 3) // INVALID_ARGUMENT
	}
	allow, deny, actor, err := normalizeCapturePolicyWrite(w)
	if err != nil {
		return "", runtime.NewError(err.Error(), 3)
	}

	current, err := readStoredCapturePolicy(ctx, nk)
	if err != nil {
		logger.Error("admin_capture_policy_set: storage read failed: %v", err)
		return "", runtime.NewError("policy read failed", 13)
	}
	if current.Version != w.Version {
		return "", runtime.NewError("policy changed since read", 10) // ABORTED
	}

	next := CapturePolicy{
		HookEnabled:   w.HookEnabled,
		PolicyVersion: time.Now().UTC().Format("2006-01-02T15:04:05Z") + ":admin:" + actor,
		HookAllowIDs:  allow,
		HookDenyIDs:   deny,
	}
	data, err := json.Marshal(next)
	if err != nil {
		return "", runtime.NewError("encode failed", 13)
	}
	// "*" creates only: a policy stored by someone else since the read fails.
	writeVersion := current.Version
	if writeVersion == "" {
		writeVersion = "*"
	}
	acks, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      CapturePolicyCollection,
			Key:             CapturePolicyKey,
			UserID:          SystemUserID,
			Value:           string(data),
			Version:         writeVersion,
			PermissionRead:  0, // server only
			PermissionWrite: 0, // server only
		},
	})
	if err != nil {
		// A version check failure and a storage failure look the same here.
		// Read again to tell them apart.
		if again, rerr := readStoredCapturePolicy(ctx, nk); rerr == nil && again.Version != current.Version {
			return "", runtime.NewError("policy changed since read", 10)
		}
		logger.Error("admin_capture_policy_set: storage write failed: %v", err)
		return "", runtime.NewError("policy write failed", 13)
	}

	resp := AdminCapturePolicySetResponse{
		Policy:        next,
		PolicyVersion: next.PolicyVersion,
		Diff:          diffCapturePolicy(current.Policy, next),
	}
	if len(acks) > 0 {
		resp.Version = acks[0].GetVersion()
	}
	logger.Info("admin_capture_policy_set: actor=%s hook_enabled=%v allow=%d deny=%d policy_version=%s",
		actor, next.HookEnabled, len(allow), len(deny), next.PolicyVersion)
	out, err := json.Marshal(resp)
	if err != nil {
		return "", runtime.NewError("encode failed", 13)
	}
	return string(out), nil
}
