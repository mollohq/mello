package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"sort"
	"strings"
	"time"

	"github.com/heroiclabs/nakama-common/api"
	"github.com/heroiclabs/nakama-common/runtime"
)

// Admin curation of the crews that discover_crews shows. One storage object
// owned by the system user holds an ordered featured list and a hidden list,
// so crew owners cannot change it. Both admin RPCs are server-to-server only,
// the same as the capture policy: the mello-admin Worker calls them with the
// http_key and writes the audit log. See plans/DISCOVER-CURATION.md.

const (
	DiscoverCurationCollection = "discover"
	DiscoverCurationKey        = "curation"

	// discoverMaxFeatured caps the featured list. Page 1 of discover_crews
	// carries the featured crews on top of a full page.
	discoverMaxFeatured = 8
	// discoverMaxHidden bounds the stored blob.
	discoverMaxHidden = 2000
	// discoverMaxActor bounds the admin name stored with a write.
	discoverMaxActor = 64
	// discoverGroupsChunk is the number of ids sent in one GroupsGetId call.
	discoverGroupsChunk = 100
)

// DiscoverCuration is the stored value. Featured is in display order.
type DiscoverCuration struct {
	Featured  []string `json:"featured"`
	Hidden    []string `json:"hidden"`
	UpdatedBy string   `json:"updated_by,omitempty"`
	UpdatedAt string   `json:"updated_at,omitempty"`
}

// DiscoverCrewSummary names a curated crew for the admin tool.
type DiscoverCrewSummary struct {
	ID          string `json:"id"`
	Name        string `json:"name"`
	Open        bool   `json:"open"`
	MemberCount int32  `json:"member_count"`
	// Exists is false when the crew was deleted after it was curated.
	Exists bool `json:"exists"`
}

// AdminDiscoverCurationRead is the admin_discover_curation_get response.
type AdminDiscoverCurationRead struct {
	Featured []DiscoverCrewSummary `json:"featured"`
	Hidden   []DiscoverCrewSummary `json:"hidden"`
	// Version is the storage version for a conditional write. Empty when
	// nothing is stored.
	Version   string `json:"version"`
	UpdatedBy string `json:"updated_by"`
	UpdatedAt string `json:"updated_at"`
}

// AdminDiscoverCurationWrite is the admin_discover_curation_set request. Both
// lists are the full new lists, not changes.
type AdminDiscoverCurationWrite struct {
	Featured []string `json:"featured"`
	Hidden   []string `json:"hidden"`
	// Version is the storage version the admin read. Empty means the admin
	// read no stored curation, so the write succeeds only if nothing is stored.
	Version string `json:"version"`
	Actor   string `json:"actor"`
}

// DiscoverCurationDiff tells the audit log what a write changed.
type DiscoverCurationDiff struct {
	FeaturedAdded   []string `json:"featured_added"`
	FeaturedRemoved []string `json:"featured_removed"`
	HiddenAdded     []string `json:"hidden_added"`
	HiddenRemoved   []string `json:"hidden_removed"`
	// OrderChanged is true when crews on both the old and the new featured
	// list changed their order.
	OrderChanged bool `json:"order_changed"`
}

// AdminDiscoverCurationSetResponse is the admin_discover_curation_set response.
type AdminDiscoverCurationSetResponse struct {
	Featured []string             `json:"featured"`
	Hidden   []string             `json:"hidden"`
	Version  string               `json:"version"`
	Diff     DiscoverCurationDiff `json:"diff"`
}

// discoverCrewEntry is one crew in the discover_crews response.
type discoverCrewEntry struct {
	ID          string `json:"id"`
	Name        string `json:"name"`
	Description string `json:"description"`
	MemberCount int32  `json:"member_count"`
	MaxMembers  int32  `json:"max_members"`
	Open        bool   `json:"open"`
	AvatarURL   string `json:"avatar_url,omitempty"`
}

func newDiscoverCrewEntry(g *api.Group) discoverCrewEntry {
	return discoverCrewEntry{
		ID:          g.GetId(),
		Name:        g.GetName(),
		Description: g.GetDescription(),
		MemberCount: g.GetEdgeCount(),
		MaxMembers:  g.GetMaxCount(),
		Open:        true,
		AvatarURL:   g.GetAvatarUrl(),
	}
}

// readDiscoverCuration returns the stored curation and its storage version.
// When nothing is stored it returns empty lists and an empty version. A stored
// value that does not parse is an error: discover_crews must not show a
// hidden crew because the list could not be read.
func readDiscoverCuration(ctx context.Context, nk runtime.NakamaModule) (DiscoverCuration, string, error) {
	empty := DiscoverCuration{Featured: []string{}, Hidden: []string{}}
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: DiscoverCurationCollection, Key: DiscoverCurationKey, UserID: SystemUserID},
	})
	if err != nil {
		return empty, "", err
	}
	if len(objects) == 0 {
		return empty, "", nil
	}
	var c DiscoverCuration
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &c); err != nil {
		return empty, "", fmt.Errorf("stored curation does not parse: %w", err)
	}
	if c.Featured == nil {
		c.Featured = []string{}
	}
	if c.Hidden == nil {
		c.Hidden = []string{}
	}
	return c, objects[0].GetVersion(), nil
}

// isCrewID reports whether s is a lowercase UUID, the form Nakama gives
// group ids.
func isCrewID(s string) bool {
	if len(s) != 36 {
		return false
	}
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch i {
		case 8, 13, 18, 23:
			if c != '-' {
				return false
			}
		default:
			if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
				return false
			}
		}
	}
	return true
}

// normalizeCrewIDs trims and lowercases each id, refuses a bad id, and drops
// duplicates. The first position of an id is kept.
func normalizeCrewIDs(ids []string) ([]string, error) {
	out := make([]string, 0, len(ids))
	seen := make(map[string]bool, len(ids))
	for _, raw := range ids {
		id := strings.ToLower(strings.TrimSpace(raw))
		if !isCrewID(id) {
			return nil, fmt.Errorf("%q is not a crew id", raw)
		}
		if seen[id] {
			continue
		}
		seen[id] = true
		out = append(out, id)
	}
	return out, nil
}

// normalizeDiscoverCurationWrite validates a write. It returns the featured
// list in the admin's order and the hidden list sorted.
func normalizeDiscoverCurationWrite(w AdminDiscoverCurationWrite) (featured, hidden []string, actor string, err error) {
	actor = strings.TrimSpace(w.Actor)
	if actor == "" {
		return nil, nil, "", fmt.Errorf("actor is required")
	}
	if len(actor) > discoverMaxActor {
		actor = actor[:discoverMaxActor]
	}
	// A missing list is refused, not read as empty, so a partial request
	// cannot clear a list by accident.
	if w.Featured == nil || w.Hidden == nil {
		return nil, nil, "", fmt.Errorf("featured and hidden are required")
	}
	if featured, err = normalizeCrewIDs(w.Featured); err != nil {
		return nil, nil, "", err
	}
	if hidden, err = normalizeCrewIDs(w.Hidden); err != nil {
		return nil, nil, "", err
	}
	if len(featured) > discoverMaxFeatured {
		return nil, nil, "", fmt.Errorf("at most %d crews can be featured", discoverMaxFeatured)
	}
	if len(hidden) > discoverMaxHidden {
		return nil, nil, "", fmt.Errorf("at most %d crews can be hidden", discoverMaxHidden)
	}
	sort.Strings(hidden)
	isHidden := make(map[string]bool, len(hidden))
	for _, id := range hidden {
		isHidden[id] = true
	}
	for _, id := range featured {
		if isHidden[id] {
			return nil, nil, "", fmt.Errorf("crew %s is both featured and hidden", id)
		}
	}
	return featured, hidden, actor, nil
}

// stringDelta returns the ids added to and removed from a list, in list order.
func stringDelta(from, to []string) (added, removed []string) {
	before := make(map[string]bool, len(from))
	for _, id := range from {
		before[id] = true
	}
	after := make(map[string]bool, len(to))
	added, removed = []string{}, []string{}
	for _, id := range to {
		after[id] = true
		if !before[id] {
			added = append(added, id)
		}
	}
	for _, id := range from {
		if !after[id] {
			removed = append(removed, id)
		}
	}
	return added, removed
}

// keepOnly returns the ids of list that are also in other, in list order.
func keepOnly(list, other []string) []string {
	in := make(map[string]bool, len(other))
	for _, id := range other {
		in[id] = true
	}
	out := []string{}
	for _, id := range list {
		if in[id] {
			out = append(out, id)
		}
	}
	return out
}

func diffDiscoverCuration(from, to DiscoverCuration) DiscoverCurationDiff {
	var d DiscoverCurationDiff
	d.FeaturedAdded, d.FeaturedRemoved = stringDelta(from.Featured, to.Featured)
	d.HiddenAdded, d.HiddenRemoved = stringDelta(from.Hidden, to.Hidden)
	before, after := keepOnly(from.Featured, to.Featured), keepOnly(to.Featured, from.Featured)
	for i := range before {
		if before[i] != after[i] {
			d.OrderChanged = true
			break
		}
	}
	return d
}

// groupsByID gets the groups for ids in chunks and returns them by id. A
// deleted group is absent from the map.
func groupsByID(ctx context.Context, nk runtime.NakamaModule, ids []string) (map[string]*api.Group, error) {
	out := make(map[string]*api.Group, len(ids))
	for start := 0; start < len(ids); start += discoverGroupsChunk {
		end := start + discoverGroupsChunk
		if end > len(ids) {
			end = len(ids)
		}
		groups, err := nk.GroupsGetId(ctx, ids[start:end])
		if err != nil {
			return nil, err
		}
		for _, g := range groups {
			out[g.GetId()] = g
		}
	}
	return out, nil
}

func summarizeCrews(ids []string, groups map[string]*api.Group) []DiscoverCrewSummary {
	out := make([]DiscoverCrewSummary, 0, len(ids))
	for _, id := range ids {
		s := DiscoverCrewSummary{ID: id}
		if g, ok := groups[id]; ok {
			s.Name = g.GetName()
			s.Open = g.GetOpen().GetValue()
			s.MemberCount = g.GetEdgeCount()
			s.Exists = true
		}
		out = append(out, s)
	}
	return out
}

// AdminDiscoverCurationGetRPC returns the curation, its storage version and a
// summary of each curated crew, for the admin tool.
func AdminDiscoverCurationGetRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	if err := requireServerCaller(ctx, "admin_discover_curation_get"); err != nil {
		return "", err
	}
	c, version, err := readDiscoverCuration(ctx, nk)
	if err != nil {
		logger.Error("admin_discover_curation_get: read failed: %v", err)
		return "", runtime.NewError("curation read failed", 13) // INTERNAL
	}
	groups, err := groupsByID(ctx, nk, append(append([]string{}, c.Featured...), c.Hidden...))
	if err != nil {
		logger.Error("admin_discover_curation_get: GroupsGetId failed: %v", err)
		return "", runtime.NewError("crew lookup failed", 13)
	}
	out, err := json.Marshal(AdminDiscoverCurationRead{
		Featured:  summarizeCrews(c.Featured, groups),
		Hidden:    summarizeCrews(c.Hidden, groups),
		Version:   version,
		UpdatedBy: c.UpdatedBy,
		UpdatedAt: c.UpdatedAt,
	})
	if err != nil {
		return "", runtime.NewError("encode failed", 13)
	}
	return string(out), nil
}

// AdminDiscoverCurationSetRPC replaces the curation. The write is conditional
// on the storage version the admin read, so two admins cannot overwrite each
// other without seeing it.
func AdminDiscoverCurationSetRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	if err := requireServerCaller(ctx, "admin_discover_curation_set"); err != nil {
		return "", err
	}
	var w AdminDiscoverCurationWrite
	if err := json.Unmarshal([]byte(payload), &w); err != nil {
		return "", runtime.NewError("invalid request: "+err.Error(), 3) // INVALID_ARGUMENT
	}
	featured, hidden, actor, err := normalizeDiscoverCurationWrite(w)
	if err != nil {
		return "", runtime.NewError(err.Error(), 3)
	}

	// A featured id must name a crew that exists, so a typo does not take a
	// featured slot. A hidden id for a deleted crew does no harm.
	groups, err := groupsByID(ctx, nk, featured)
	if err != nil {
		logger.Error("admin_discover_curation_set: GroupsGetId failed: %v", err)
		return "", runtime.NewError("crew lookup failed", 13)
	}
	for _, id := range featured {
		if _, ok := groups[id]; !ok {
			return "", runtime.NewError("crew "+id+" does not exist", 3)
		}
	}

	current, version, err := readDiscoverCuration(ctx, nk)
	if err != nil {
		logger.Error("admin_discover_curation_set: read failed: %v", err)
		return "", runtime.NewError("curation read failed", 13)
	}
	if version != w.Version {
		return "", runtime.NewError("curation changed since read", 10) // ABORTED
	}

	next := DiscoverCuration{
		Featured:  featured,
		Hidden:    hidden,
		UpdatedBy: actor,
		UpdatedAt: time.Now().UTC().Format(time.RFC3339),
	}
	data, err := json.Marshal(next)
	if err != nil {
		return "", runtime.NewError("encode failed", 13)
	}
	// "*" creates only: a curation stored by someone else since the read fails.
	writeVersion := version
	if writeVersion == "" {
		writeVersion = "*"
	}
	acks, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      DiscoverCurationCollection,
			Key:             DiscoverCurationKey,
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
		if _, again, rerr := readDiscoverCuration(ctx, nk); rerr == nil && again != version {
			return "", runtime.NewError("curation changed since read", 10)
		}
		logger.Error("admin_discover_curation_set: write failed: %v", err)
		return "", runtime.NewError("curation write failed", 13)
	}

	resp := AdminDiscoverCurationSetResponse{
		Featured: featured,
		Hidden:   hidden,
		Diff:     diffDiscoverCuration(current, next),
	}
	if len(acks) > 0 {
		resp.Version = acks[0].GetVersion()
	}
	logger.Info("admin_discover_curation_set: actor=%s featured=%d hidden=%d",
		actor, len(featured), len(hidden))
	out, err := json.Marshal(resp)
	if err != nil {
		return "", runtime.NewError("encode failed", 13)
	}
	return string(out), nil
}
