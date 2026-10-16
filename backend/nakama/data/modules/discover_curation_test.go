package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"reflect"
	"strconv"
	"strings"
	"testing"

	"github.com/heroiclabs/nakama-common/api"
	"github.com/heroiclabs/nakama-common/runtime"
	"google.golang.org/protobuf/types/known/wrapperspb"
)

// fakeDiscoverNk adds groups and memberships to the versioned storage of
// fakePolicyNk. GroupsList pages the groups in insertion order with a numeric
// cursor. Other methods panic on the nil embedded interface.
type fakeDiscoverNk struct {
	*fakePolicyNk

	order   []string
	groups  map[string]*api.Group
	joined  map[string][]string // userID -> crew ids
	readErr error
}

func newFakeDiscoverNk() *fakeDiscoverNk {
	return &fakeDiscoverNk{
		fakePolicyNk: newFakePolicyNk(),
		groups:       map[string]*api.Group{},
		joined:       map[string][]string{},
	}
}

// crewID returns a valid group id for crew n.
func crewID(n int) string {
	return fmt.Sprintf("00000000-0000-4000-8000-%012d", n)
}

func (f *fakeDiscoverNk) addCrew(n int, open bool) string {
	id := crewID(n)
	f.order = append(f.order, id)
	f.groups[id] = &api.Group{Id: id, Name: "crew " + strconv.Itoa(n), Open: wrapperspb.Bool(open), EdgeCount: 1}
	return id
}

// storeCuration writes a curation directly, without the admin RPC.
func (f *fakeDiscoverNk) storeCuration(featured, hidden []string) {
	data, _ := json.Marshal(DiscoverCuration{Featured: featured, Hidden: hidden})
	f.storeRaw(string(data))
}

func (f *fakeDiscoverNk) storeRaw(value string) {
	_, err := f.fakePolicyNk.StorageWrite(context.Background(), []*runtime.StorageWrite{
		{Collection: DiscoverCurationCollection, Key: DiscoverCurationKey, UserID: SystemUserID, Value: value},
	})
	if err != nil {
		panic(err)
	}
}

func (f *fakeDiscoverNk) StorageRead(ctx context.Context, reads []*runtime.StorageRead) ([]*api.StorageObject, error) {
	if f.readErr != nil {
		return nil, f.readErr
	}
	return f.fakePolicyNk.StorageRead(ctx, reads)
}

func (f *fakeDiscoverNk) GroupsGetId(_ context.Context, ids []string) ([]*api.Group, error) {
	var out []*api.Group
	for _, id := range ids {
		if g, ok := f.groups[id]; ok {
			out = append(out, g)
		}
	}
	return out, nil
}

func (f *fakeDiscoverNk) GroupsList(_ context.Context, _, _ string, _ *int, open *bool, limit int, cursor string) ([]*api.Group, string, error) {
	var matching []*api.Group
	for _, id := range f.order {
		g := f.groups[id]
		if open != nil && g.GetOpen().GetValue() != *open {
			continue
		}
		matching = append(matching, g)
	}
	start := 0
	if cursor != "" {
		start, _ = strconv.Atoi(cursor)
	}
	end := start + limit
	if end >= len(matching) {
		return matching[start:], "", nil
	}
	return matching[start:end], strconv.Itoa(end), nil
}

func (f *fakeDiscoverNk) UserGroupsList(_ context.Context, userID string, _ int, _ *int, _ string) ([]*api.UserGroupList_UserGroup, string, error) {
	var out []*api.UserGroupList_UserGroup
	for _, id := range f.joined[userID] {
		out = append(out, &api.UserGroupList_UserGroup{Group: f.groups[id]})
	}
	return out, "", nil
}

type discoverPage struct {
	Crews  []discoverCrewEntry `json:"crews"`
	Cursor string              `json:"cursor"`
}

func discover(t *testing.T, ctx context.Context, nk runtime.NakamaModule, cursor string) discoverPage {
	t.Helper()
	payload, _ := json.Marshal(map[string]string{"cursor": cursor})
	out, err := DiscoverCrewsRPC(ctx, testLogger(), nil, nk, string(payload))
	if err != nil {
		t.Fatalf("discover_crews: %v", err)
	}
	var page discoverPage
	if err := json.Unmarshal([]byte(out), &page); err != nil {
		t.Fatalf("decode discover_crews: %v", err)
	}
	return page
}

// discoverAll walks every page and returns the crew ids per page.
func discoverAll(t *testing.T, ctx context.Context, nk runtime.NakamaModule) [][]string {
	t.Helper()
	var pages [][]string
	cursor := ""
	for {
		page := discover(t, ctx, nk, cursor)
		ids := make([]string, 0, len(page.Crews))
		for _, c := range page.Crews {
			ids = append(ids, c.ID)
		}
		pages = append(pages, ids)
		if page.Cursor == "" {
			return pages
		}
		cursor = page.Cursor
	}
}

func contains(ids []string, id string) bool {
	for _, x := range ids {
		if x == id {
			return true
		}
	}
	return false
}

func TestDiscoverWithNoCurationListsOpenCrewsInOrder(t *testing.T) {
	nk := newFakeDiscoverNk()
	a := nk.addCrew(1, true)
	nk.addCrew(2, false)
	c := nk.addCrew(3, true)

	pages := discoverAll(t, context.Background(), nk)
	if want := [][]string{{a, c}}; !reflect.DeepEqual(pages, want) {
		t.Fatalf("got %v, want %v", pages, want)
	}
}

func TestDiscoverNeverShowsAHiddenCrew(t *testing.T) {
	nk := newFakeDiscoverNk()
	for n := 1; n <= 60; n++ {
		nk.addCrew(n, true)
	}
	// One hidden crew on each page of 50.
	nk.storeCuration([]string{}, []string{crewID(2), crewID(55)})

	pages := discoverAll(t, context.Background(), nk)
	if len(pages) != 2 {
		t.Fatalf("want 2 pages, got %d", len(pages))
	}
	total := 0
	for i, ids := range pages {
		total += len(ids)
		for _, hidden := range []string{crewID(2), crewID(55)} {
			if contains(ids, hidden) {
				t.Fatalf("page %d shows hidden crew %s", i+1, hidden)
			}
		}
	}
	if total != 58 {
		t.Fatalf("want 58 crews over all pages, got %d", total)
	}
}

func TestDiscoverPutsFeaturedCrewsFirstOnPageOneOnly(t *testing.T) {
	nk := newFakeDiscoverNk()
	for n := 1; n <= 60; n++ {
		nk.addCrew(n, true)
	}
	// Crew 55 is on page 2 of the plain list. Featured moves it to the top.
	nk.storeCuration([]string{crewID(55), crewID(3)}, []string{})

	pages := discoverAll(t, context.Background(), nk)
	if got := pages[0][:2]; !reflect.DeepEqual(got, []string{crewID(55), crewID(3)}) {
		t.Fatalf("page 1 must start with the featured crews in order, got %v", got)
	}
	seen := map[string]int{}
	for _, ids := range pages {
		for _, id := range ids {
			seen[id]++
		}
	}
	for n := 1; n <= 60; n++ {
		if seen[crewID(n)] != 1 {
			t.Fatalf("crew %d shows %d times over all pages, want once", n, seen[crewID(n)])
		}
	}
}

func TestDiscoverSkipsFeaturedCrewsThatAreClosedDeletedOrJoined(t *testing.T) {
	nk := newFakeDiscoverNk()
	closed := nk.addCrew(1, false)
	joined := nk.addCrew(2, true)
	open := nk.addCrew(3, true)
	deleted := crewID(99)
	nk.joined["user_1"] = []string{joined}
	nk.storeCuration([]string{closed, deleted, joined, open}, []string{})

	page := discover(t, ctxWithUser("user_1"), nk, "")
	var ids []string
	for _, c := range page.Crews {
		ids = append(ids, c.ID)
	}
	if want := []string{open}; !reflect.DeepEqual(ids, want) {
		t.Fatalf("got %v, want %v", ids, want)
	}
}

func TestDiscoverFailsWhenTheCurationCannotBeRead(t *testing.T) {
	nk := newFakeDiscoverNk()
	nk.addCrew(1, true)
	nk.readErr = errors.New("db down")
	if _, err := DiscoverCrewsRPC(context.Background(), testLogger(), nil, nk, "{}"); errCode(err) != 13 {
		t.Fatalf("want INTERNAL when the curation read fails, got %v", err)
	}

	nk.readErr = nil
	nk.storeRaw("not json")
	if _, err := DiscoverCrewsRPC(context.Background(), testLogger(), nil, nk, "{}"); errCode(err) != 13 {
		t.Fatalf("want INTERNAL when the stored curation does not parse, got %v", err)
	}
}

func curationSet(nk runtime.NakamaModule, w AdminDiscoverCurationWrite) (AdminDiscoverCurationSetResponse, error) {
	payload, _ := json.Marshal(w)
	out, err := AdminDiscoverCurationSetRPC(context.Background(), testLogger(), nil, nk, string(payload))
	var resp AdminDiscoverCurationSetResponse
	if err == nil {
		_ = json.Unmarshal([]byte(out), &resp)
	}
	return resp, err
}

func curationGet(t *testing.T, nk runtime.NakamaModule) AdminDiscoverCurationRead {
	t.Helper()
	out, err := AdminDiscoverCurationGetRPC(context.Background(), testLogger(), nil, nk, "{}")
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	var read AdminDiscoverCurationRead
	if err := json.Unmarshal([]byte(out), &read); err != nil {
		t.Fatalf("decode get: %v", err)
	}
	return read
}

func TestAdminDiscoverCurationRefusesClientSession(t *testing.T) {
	ctx := ctxWithUser("user_1")
	nk := newFakeDiscoverNk()
	if _, err := AdminDiscoverCurationGetRPC(ctx, testLogger(), nil, nk, "{}"); errCode(err) != 7 {
		t.Fatalf("get from a client session: want PERMISSION_DENIED, got %v", err)
	}
	if _, err := AdminDiscoverCurationSetRPC(ctx, testLogger(), nil, nk, "{}"); errCode(err) != 7 {
		t.Fatalf("set from a client session: want PERMISSION_DENIED, got %v", err)
	}
}

func TestAdminDiscoverCurationSetThenDiscoverReadsIt(t *testing.T) {
	nk := newFakeDiscoverNk()
	a := nk.addCrew(1, true)
	b := nk.addCrew(2, true)
	c := nk.addCrew(3, true)

	resp, err := curationSet(nk, AdminDiscoverCurationWrite{
		Featured: []string{strings.ToUpper(c)}, Hidden: []string{a}, Actor: "bob",
	})
	if err != nil {
		t.Fatalf("set: %v", err)
	}
	if resp.Version == "" {
		t.Fatal("set must return the new version")
	}

	page := discover(t, context.Background(), nk, "")
	var ids []string
	for _, e := range page.Crews {
		ids = append(ids, e.ID)
	}
	if want := []string{c, b}; !reflect.DeepEqual(ids, want) {
		t.Fatalf("got %v, want %v", ids, want)
	}

	read := curationGet(t, nk)
	if read.Version != resp.Version || read.UpdatedBy != "bob" {
		t.Fatalf("get after set: %+v", read)
	}
	if len(read.Featured) != 1 || read.Featured[0].ID != c || read.Featured[0].Name != "crew 3" || !read.Featured[0].Open {
		t.Fatalf("featured summary: %+v", read.Featured)
	}
}

func TestAdminDiscoverCurationGetMarksADeletedCrew(t *testing.T) {
	nk := newFakeDiscoverNk()
	a := nk.addCrew(1, true)
	nk.storeCuration([]string{a}, []string{crewID(99)})

	read := curationGet(t, nk)
	if !read.Featured[0].Exists {
		t.Fatal("an existing crew must have exists=true")
	}
	if read.Hidden[0].Exists || read.Hidden[0].ID != crewID(99) {
		t.Fatalf("a deleted crew must stay listed with exists=false, got %+v", read.Hidden[0])
	}
}

func TestAdminDiscoverCurationSetRefusesAnUnknownFeaturedCrew(t *testing.T) {
	nk := newFakeDiscoverNk()
	_, err := curationSet(nk, AdminDiscoverCurationWrite{
		Featured: []string{crewID(99)}, Hidden: []string{}, Actor: "bob",
	})
	if errCode(err) != 3 || !strings.Contains(err.Error(), "does not exist") {
		t.Fatalf("want INVALID_ARGUMENT for an unknown featured crew, got %v", err)
	}
}

func TestAdminDiscoverCurationSetRefusesStaleVersion(t *testing.T) {
	nk := newFakeDiscoverNk()
	a := nk.addCrew(1, true)
	first, err := curationSet(nk, AdminDiscoverCurationWrite{Featured: []string{a}, Hidden: []string{}, Actor: "bob"})
	if err != nil {
		t.Fatalf("first set: %v", err)
	}
	// A second admin read nothing stored, then saves.
	_, err = curationSet(nk, AdminDiscoverCurationWrite{Featured: []string{}, Hidden: []string{}, Actor: "eve"})
	if errCode(err) != 10 || !strings.Contains(err.Error(), "changed since read") {
		t.Fatalf("want ABORTED for a stale empty version, got %v", err)
	}
	_, err = curationSet(nk, AdminDiscoverCurationWrite{Featured: []string{}, Hidden: []string{}, Version: first.Version, Actor: "eve"})
	if err != nil {
		t.Fatalf("set with the current version: %v", err)
	}
}

func TestAdminDiscoverCurationSetRefusesARaceBetweenReadAndWrite(t *testing.T) {
	nk := newFakeDiscoverNk()
	a := nk.addCrew(1, true)
	nk.beforeWrite = func() { nk.storeCuration([]string{}, []string{a}) }
	_, err := curationSet(nk, AdminDiscoverCurationWrite{Featured: []string{a}, Hidden: []string{}, Actor: "bob"})
	if errCode(err) != 10 {
		t.Fatalf("want ABORTED when a write lands between read and write, got %v", err)
	}
}

func TestNormalizeDiscoverCurationWrite(t *testing.T) {
	ids := func(ns ...int) []string {
		out := []string{}
		for _, n := range ns {
			out = append(out, crewID(n))
		}
		return out
	}
	refused := []struct {
		name string
		w    AdminDiscoverCurationWrite
		want string
	}{
		{"no actor", AdminDiscoverCurationWrite{Featured: ids(), Hidden: ids()}, "actor"},
		{"no featured list", AdminDiscoverCurationWrite{Hidden: ids(), Actor: "bob"}, "required"},
		{"no hidden list", AdminDiscoverCurationWrite{Featured: ids(), Actor: "bob"}, "required"},
		{"bad id", AdminDiscoverCurationWrite{Featured: []string{"crew-1"}, Hidden: ids(), Actor: "bob"}, "not a crew id"},
		{"too many featured", AdminDiscoverCurationWrite{Featured: ids(1, 2, 3, 4, 5, 6, 7, 8, 9), Hidden: ids(), Actor: "bob"}, "at most 8"},
		{"on both lists", AdminDiscoverCurationWrite{Featured: ids(1), Hidden: ids(2, 1), Actor: "bob"}, "both featured and hidden"},
	}
	for _, c := range refused {
		t.Run(c.name, func(t *testing.T) {
			_, _, _, err := normalizeDiscoverCurationWrite(c.w)
			if err == nil || !strings.Contains(err.Error(), c.want) {
				t.Fatalf("want an error with %q, got %v", c.want, err)
			}
		})
	}

	featured, hidden, actor, err := normalizeDiscoverCurationWrite(AdminDiscoverCurationWrite{
		Featured: []string{crewID(3), " " + strings.ToUpper(crewID(1)), crewID(3)},
		Hidden:   ids(9, 5, 9),
		Actor:    "  bob ",
	})
	if err != nil {
		t.Fatalf("valid write: %v", err)
	}
	if !reflect.DeepEqual(featured, ids(3, 1)) {
		t.Fatalf("featured keeps the first position of each id: got %v", featured)
	}
	if !reflect.DeepEqual(hidden, ids(5, 9)) {
		t.Fatalf("hidden is sorted and unique: got %v", hidden)
	}
	if actor != "bob" {
		t.Fatalf("actor is trimmed: got %q", actor)
	}
}

func TestDiffDiscoverCuration(t *testing.T) {
	from := DiscoverCuration{Featured: []string{"a", "b", "c"}, Hidden: []string{"x"}}
	to := DiscoverCuration{Featured: []string{"c", "a", "d"}, Hidden: []string{"y"}}
	d := diffDiscoverCuration(from, to)
	want := DiscoverCurationDiff{
		FeaturedAdded:   []string{"d"},
		FeaturedRemoved: []string{"b"},
		HiddenAdded:     []string{"y"},
		HiddenRemoved:   []string{"x"},
		OrderChanged:    true,
	}
	if !reflect.DeepEqual(d, want) {
		t.Fatalf("got %+v, want %+v", d, want)
	}

	// Removing a crew is not a change of order.
	d = diffDiscoverCuration(DiscoverCuration{Featured: []string{"a", "b", "c"}}, DiscoverCuration{Featured: []string{"a", "c"}})
	if d.OrderChanged {
		t.Fatal("a removal alone must not count as a change of order")
	}
}
