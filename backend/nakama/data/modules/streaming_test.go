package main

import (
	"context"
	"encoding/json"
	"errors"
	"testing"

	"github.com/heroiclabs/nakama-common/runtime"
)

// fakeStreamNk adds StorageDelete to the guest fake, for the stream RPCs.
type fakeStreamNk struct {
	*fakeGuestNk
}

func (f *fakeStreamNk) StorageDelete(_ context.Context, deletes []*runtime.StorageDelete) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	for _, d := range deletes {
		delete(f.storage, fakeStorageKey(d.Collection, d.Key, d.UserID))
	}
	return nil
}

func (f *fakeStreamNk) has(collection, key, userID string) bool {
	f.mu.Lock()
	defer f.mu.Unlock()
	_, ok := f.storage[fakeStorageKey(collection, key, userID)]
	return ok
}

const streamTestViewer = "user-sfu-only-0002"

func seedStreamCrew(t *testing.T) *fakeStreamNk {
	t.Helper()
	withSFUKey(t, true)
	nk := &fakeStreamNk{fakeGuestNk: seedSfuOnlyCrew("")}
	nk.addMember(sfuOnlyCrew, streamTestViewer, "bob")
	return nk
}

func startTestStream(t *testing.T, nk *fakeStreamNk) string {
	t.Helper()
	out, err := StartStreamRPC(ctxWithUser(sfuOnlyUser), testLogger(), nil, nk,
		`{"crew_id":"`+sfuOnlyCrew+`","title":"Raid night"}`)
	if err != nil {
		t.Fatalf("start_stream: %v", err)
	}
	var resp struct {
		SessionID string `json:"session_id"`
	}
	if err := json.Unmarshal([]byte(out), &resp); err != nil || resp.SessionID == "" {
		t.Fatalf("start_stream response %q: %v", out, err)
	}
	return resp.SessionID
}

func assertRuntimeCode(t *testing.T, err error, code int) {
	t.Helper()
	var rerr *runtime.Error
	if !errors.As(err, &rerr) || rerr.Code != code {
		t.Fatalf("want runtime error code %d, got %v", code, err)
	}
}

// Only the host stops a stream: another crew member gets PERMISSION_DENIED
// and the stream stays.
func TestStopStream_NonHostIsRejected(t *testing.T) {
	nk := seedStreamCrew(t)
	sessionID := startTestStream(t, nk)

	_, err := StopStreamRPC(ctxWithUser(streamTestViewer), testLogger(), nil, nk,
		`{"crew_id":"`+sfuOnlyCrew+`"}`)

	assertRuntimeCode(t, err, 7)
	if !nk.has(StreamMetaCollection, sfuOnlyCrew, SystemUserID) ||
		!nk.has(StreamSessionCol, sessionID, SystemUserID) {
		t.Fatal("a stop by a non-host removed the stream")
	}
}

func TestStopStream_HostStops(t *testing.T) {
	nk := seedStreamCrew(t)
	sessionID := startTestStream(t, nk)

	if _, err := StopStreamRPC(ctxWithUser(sfuOnlyUser), testLogger(), nil, nk,
		`{"crew_id":"`+sfuOnlyCrew+`"}`); err != nil {
		t.Fatalf("stop_stream by the host: %v", err)
	}
	if nk.has(StreamMetaCollection, sfuOnlyCrew, SystemUserID) ||
		nk.has(StreamSessionCol, sessionID, SystemUserID) ||
		nk.has(StreamCollection, sfuOnlyCrew, sfuOnlyUser) {
		t.Fatal("the host's stop left stream records")
	}
}

func resumeTestStream(nk *fakeStreamNk, userID, sessionID string) (string, error) {
	return StartStreamRPC(ctxWithUser(userID), testLogger(), nil, nk,
		`{"crew_id":"`+sfuOnlyCrew+`","resume_session_id":"`+sessionID+`"}`)
}

func ledgerEventCount(nk *fakeStreamNk, eventType string) int {
	ledger, _ := readLedger(context.Background(), nk, sfuOnlyCrew)
	n := 0
	for _, e := range ledger.Events {
		if e.Type == eventType {
			n++
		}
	}
	return n
}

// A Nakama socket drop runs the session-end cleanup while the SFU session
// still runs. The host re-advertises the same session: the records come
// back with the same session id and endpoint, and the cleanup's
// stream_session ledger event is removed.
func TestStartStreamResume_RestoresTheCleanedUpStream(t *testing.T) {
	nk := seedStreamCrew(t)
	sessionID := startTestStream(t, nk)
	before := loadStreamSession(context.Background(), nk, sessionID)
	if before == nil {
		t.Fatal("no stream session record")
	}

	cleanupStreamIfHost(context.Background(), testLogger(), nk, sfuOnlyCrew, sfuOnlyUser)
	if nk.has(StreamMetaCollection, sfuOnlyCrew, SystemUserID) {
		t.Fatal("the cleanup left the stream advertised")
	}
	if got := ledgerEventCount(nk, "stream_session"); got != 1 {
		t.Fatalf("cleanup wrote %d stream_session events, want 1", got)
	}

	out, err := resumeTestStream(nk, sfuOnlyUser, sessionID)
	if err != nil {
		t.Fatalf("resume: %v", err)
	}
	var resp struct {
		SessionID   string `json:"session_id"`
		SFUEndpoint string `json:"sfu_endpoint"`
		Resumed     bool   `json:"resumed"`
	}
	if err := json.Unmarshal([]byte(out), &resp); err != nil {
		t.Fatal(err)
	}
	if resp.SessionID != sessionID || resp.SFUEndpoint != before.SFUEndpoint || !resp.Resumed {
		t.Fatalf("resume response %s, want session %s on %s", out, sessionID, before.SFUEndpoint)
	}
	meta := readStreamMeta(context.Background(), nk, sfuOnlyCrew)
	if meta == nil || meta.StreamID != sessionID || meta.StreamerID != sfuOnlyUser || meta.Title != "Raid night" {
		t.Fatalf("stream meta after resume: %+v", meta)
	}
	if after := loadStreamSession(context.Background(), nk, sessionID); after == nil || *after != *before {
		t.Fatalf("stream session after resume: %+v, want %+v", after, before)
	}
	if !nk.has(StreamCollection, sfuOnlyCrew, sfuOnlyUser) {
		t.Fatal("active stream not restored")
	}
	if got := ledgerEventCount(nk, "stream_session"); got != 0 {
		t.Fatalf("%d stream_session events after resume, want 0", got)
	}
	if nk.has(StreamResumeCollection, sfuOnlyUser, SystemUserID) {
		t.Fatal("the resume record stays after use")
	}
}

// The old socket ended while a newer one was open, so no cleanup ran: the
// resume confirms the stream and writes nothing.
func TestStartStreamResume_StillAdvertised(t *testing.T) {
	nk := seedStreamCrew(t)
	sessionID := startTestStream(t, nk)

	if _, err := resumeTestStream(nk, sfuOnlyUser, sessionID); err != nil {
		t.Fatalf("resume of a live stream: %v", err)
	}
	if meta := readStreamMeta(context.Background(), nk, sfuOnlyCrew); meta == nil || meta.StreamID != sessionID {
		t.Fatalf("stream meta changed: %+v", meta)
	}
}

// A resume restores only what the server recorded for this host and this
// session.
func TestStartStreamResume_RejectsOtherSessionsAndUsers(t *testing.T) {
	nk := seedStreamCrew(t)
	sessionID := startTestStream(t, nk)
	cleanupStreamIfHost(context.Background(), testLogger(), nk, sfuOnlyCrew, sfuOnlyUser)

	_, err := resumeTestStream(nk, sfuOnlyUser, "stream_other")
	assertRuntimeCode(t, err, 5)
	_, err = resumeTestStream(nk, streamTestViewer, sessionID)
	assertRuntimeCode(t, err, 5)
	if nk.has(StreamMetaCollection, sfuOnlyCrew, SystemUserID) {
		t.Fatal("a rejected resume advertised a stream")
	}
}
