package main

import (
	"context"
	"encoding/json"
	"sync"

	"github.com/heroiclabs/nakama-common/api"
	"github.com/heroiclabs/nakama-common/runtime"
)

// fakeGuestNk is an in-memory runtime.NakamaModule for the guest RPC tests. It
// implements only the storage, group and user reads that the guest RPCs use.
// A call to any other method panics on the nil embedded interface, so a test
// shows at once when an RPC starts to use a new part of the module.
type fakeGuestNk struct {
	runtime.NakamaModule

	mu      sync.Mutex
	storage map[string]string // collection/key/userID -> JSON value
	groups  map[string]*api.Group
	members map[string][]*api.GroupUserList_GroupUser // groupID -> members
	users   map[string]*api.User
}

func newFakeGuestNk() *fakeGuestNk {
	return &fakeGuestNk{
		storage: make(map[string]string),
		groups:  make(map[string]*api.Group),
		members: make(map[string][]*api.GroupUserList_GroupUser),
		users:   make(map[string]*api.User),
	}
}

func fakeStorageKey(collection, key, userID string) string {
	return collection + "/" + key + "/" + userID
}

// put stores v as JSON under collection/key/userID.
func (f *fakeGuestNk) put(collection, key, userID string, v interface{}) {
	data, err := json.Marshal(v)
	if err != nil {
		panic(err)
	}
	f.mu.Lock()
	f.storage[fakeStorageKey(collection, key, userID)] = string(data)
	f.mu.Unlock()
}

// addMember adds a user to a crew and to the user table.
func (f *fakeGuestNk) addMember(crewID, userID, displayName string) {
	u := &api.User{Id: userID, Username: displayName, DisplayName: displayName}
	f.users[userID] = u
	f.members[crewID] = append(f.members[crewID], &api.GroupUserList_GroupUser{User: u})
}

func (f *fakeGuestNk) StorageRead(_ context.Context, reads []*runtime.StorageRead) ([]*api.StorageObject, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	var out []*api.StorageObject
	for _, r := range reads {
		if v, ok := f.storage[fakeStorageKey(r.Collection, r.Key, r.UserID)]; ok {
			out = append(out, &api.StorageObject{Collection: r.Collection, Key: r.Key, UserId: r.UserID, Value: v})
		}
	}
	return out, nil
}

func (f *fakeGuestNk) StorageWrite(_ context.Context, writes []*runtime.StorageWrite) ([]*api.StorageObjectAck, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	acks := make([]*api.StorageObjectAck, 0, len(writes))
	for _, w := range writes {
		f.storage[fakeStorageKey(w.Collection, w.Key, w.UserID)] = w.Value
		acks = append(acks, &api.StorageObjectAck{Collection: w.Collection, Key: w.Key, UserId: w.UserID})
	}
	return acks, nil
}

func (f *fakeGuestNk) GroupsGetId(_ context.Context, ids []string) ([]*api.Group, error) {
	var out []*api.Group
	for _, id := range ids {
		if g, ok := f.groups[id]; ok {
			out = append(out, g)
		}
	}
	return out, nil
}

func (f *fakeGuestNk) GroupUsersList(_ context.Context, id string, _ int, _ *int, _ string) ([]*api.GroupUserList_GroupUser, string, error) {
	return f.members[id], "", nil
}

func (f *fakeGuestNk) UsersGetId(_ context.Context, ids []string, _ []string) ([]*api.User, error) {
	var out []*api.User
	for _, id := range ids {
		if u, ok := f.users[id]; ok {
			out = append(out, u)
		}
	}
	return out, nil
}

// ctxWithUser returns a context that carries an authenticated Nakama user ID.
func ctxWithUser(userID string) context.Context {
	return context.WithValue(context.Background(), runtime.RUNTIME_CTX_USER_ID, userID)
}
