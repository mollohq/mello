package main

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"strings"
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

func watchTestStream(t *testing.T, nk *fakeStreamNk, userID, sessionID string) {
	t.Helper()
	if _, err := WatchStreamRPC(ctxWithUser(userID), testLogger(), nil, nk,
		`{"session_id":"`+sessionID+`"}`); err != nil {
		t.Fatalf("watch_stream as %s: %v", userID, err)
	}
}

func viewerCount(nk *fakeStreamNk) int {
	return getActiveStreamForCrew(context.Background(), nk, sfuOnlyCrew).ViewerCount
}

// watch_stream counts the viewer in, once; stream_viewer_left from the host
// counts it out. crew_state viewer_count follows. It was always 0: only the
// dev seed wrote ViewerIDs.
func TestViewerCount_WatchAndLeave(t *testing.T) {
	nk := seedStreamCrew(t)
	sessionID := startTestStream(t, nk)

	watchTestStream(t, nk, streamTestViewer, sessionID)
	watchTestStream(t, nk, streamTestViewer, sessionID)
	if got := viewerCount(nk); got != 1 {
		t.Fatalf("viewer_count after one viewer watched twice = %d, want 1", got)
	}

	_, err := StreamViewerLeftRPC(ctxWithUser(streamTestViewer), testLogger(), nil, nk,
		`{"crew_id":"`+sfuOnlyCrew+`","viewer_id":"`+streamTestViewer+`"}`)
	assertRuntimeCode(t, err, 7)
	if got := viewerCount(nk); got != 1 {
		t.Fatalf("a non-host changed viewer_count to %d", got)
	}

	if _, err := StreamViewerLeftRPC(ctxWithUser(sfuOnlyUser), testLogger(), nil, nk,
		`{"crew_id":"`+sfuOnlyCrew+`","viewer_id":"`+streamTestViewer+`"}`); err != nil {
		t.Fatalf("stream_viewer_left by the host: %v", err)
	}
	if got := viewerCount(nk); got != 0 {
		t.Fatalf("viewer_count after the viewer left = %d, want 0", got)
	}
}

// The session ledger keeps every viewer that watched and the peak, also
// after the viewers left.
func TestStopStream_LedgerKeepsPeakAndAllViewers(t *testing.T) {
	nk := seedStreamCrew(t)
	sessionID := startTestStream(t, nk)
	watchTestStream(t, nk, streamTestViewer, sessionID)
	if _, err := StreamViewerLeftRPC(ctxWithUser(sfuOnlyUser), testLogger(), nil, nk,
		`{"crew_id":"`+sfuOnlyCrew+`","viewer_id":"`+streamTestViewer+`"}`); err != nil {
		t.Fatal(err)
	}

	if _, err := StopStreamRPC(ctxWithUser(sfuOnlyUser), testLogger(), nil, nk,
		`{"crew_id":"`+sfuOnlyCrew+`"}`); err != nil {
		t.Fatal(err)
	}
	ledger, _ := readLedger(context.Background(), nk, sfuOnlyCrew)
	for _, e := range ledger.Events {
		if e.Type != "stream_session" {
			continue
		}
		data, err := decodeStreamSessionData(e.Data)
		if err != nil {
			t.Fatal(err)
		}
		if data.PeakViewers != 1 || len(data.ViewerIDs) != 1 || data.ViewerIDs[0] != streamTestViewer {
			t.Fatalf("stream_session data %+v, want peak 1 and the one viewer", data)
		}
		return
	}
	t.Fatal("no stream_session event")
}

// --- streaming-quality stage 6: advertise on the open track, token refresh ---

func startTestStreamOnLive(t *testing.T, nk *fakeStreamNk) string {
	t.Helper()
	out, err := StartStreamRPC(ctxWithUser(sfuOnlyUser), testLogger(), nil, nk,
		`{"crew_id":"`+sfuOnlyCrew+`","title":"Raid night","width":1280,"height":720,"fps":60,"advertise":"on_live"}`)
	if err != nil {
		t.Fatalf("start_stream: %v", err)
	}
	var resp struct {
		SessionID string `json:"session_id"`
		Advertise string `json:"advertise"`
		Token     string `json:"sfu_token"`
	}
	if err := json.Unmarshal([]byte(out), &resp); err != nil || resp.SessionID == "" {
		t.Fatalf("start_stream response %q: %v", out, err)
	}
	if resp.Advertise != advertiseOnLive || resp.Token == "" {
		t.Fatalf("start_stream on_live response %q: want advertise on_live and a token", out)
	}
	return resp.SessionID
}

func streamLive(nk *fakeStreamNk, userID, sessionID string) (string, error) {
	return StreamLiveRPC(ctxWithUser(userID), testLogger(), nil, nk,
		`{"crew_id":"`+sfuOnlyCrew+`","session_id":"`+sessionID+`","width":1920,"height":1080,"fps":60}`)
}

// With advertise on_live, start_stream writes only the session record: the
// crew sees no stream until stream_live, when the host's track is open.
func TestStartStreamOnLive_AdvertisesOnlyAtStreamLive(t *testing.T) {
	nk := seedStreamCrew(t)
	sessionID := startTestStreamOnLive(t, nk)

	if nk.has(StreamMetaCollection, sfuOnlyCrew, SystemUserID) ||
		nk.has(StreamCollection, sfuOnlyCrew, sfuOnlyUser) {
		t.Fatal("start_stream with advertise on_live advertised the stream")
	}
	if !nk.has(StreamSessionCol, sessionID, SystemUserID) {
		t.Fatal("start_stream with advertise on_live wrote no session record")
	}

	if _, err := streamLive(nk, sfuOnlyUser, sessionID); err != nil {
		t.Fatalf("stream_live: %v", err)
	}
	meta := readStreamMeta(context.Background(), nk, sfuOnlyCrew)
	if meta == nil || meta.StreamID != sessionID {
		t.Fatalf("stream_live did not advertise the stream: %+v", meta)
	}
	if meta.Width != 1920 || meta.Height != 1080 || meta.Fps != 60 || meta.Title != "Raid night" {
		t.Fatalf("advertised meta %+v, want the live size 1920x1080 at 60 fps", meta)
	}
	if session := loadStreamSession(context.Background(), nk, sessionID); session == nil || session.Pending != nil {
		t.Fatalf("session after stream_live %+v, want no pending advert", session)
	}
	// A viewer finds it and gets the live size.
	out, err := WatchStreamRPC(ctxWithUser(streamTestViewer), testLogger(), nil, nk,
		`{"session_id":"`+sessionID+`"}`)
	if err != nil {
		t.Fatalf("watch_stream: %v", err)
	}
	var watch struct {
		Width  uint32 `json:"width"`
		Height uint32 `json:"height"`
	}
	if err := json.Unmarshal([]byte(out), &watch); err != nil || watch.Width != 1920 || watch.Height != 1080 {
		t.Fatalf("watch_stream response %q: want 1920x1080", out)
	}
}

func TestStreamLive_OnlyTheHost(t *testing.T) {
	nk := seedStreamCrew(t)
	sessionID := startTestStreamOnLive(t, nk)

	_, err := streamLive(nk, streamTestViewer, sessionID)
	assertRuntimeCode(t, err, 7)
	if nk.has(StreamMetaCollection, sfuOnlyCrew, SystemUserID) {
		t.Fatal("a stream_live by a non-host advertised the stream")
	}
	_, err = streamLive(nk, sfuOnlyUser, "stream_unknown")
	assertRuntimeCode(t, err, 5)
}

// A second stream_live (a retry after a lost answer) changes nothing.
func TestStreamLive_SecondCallChangesNothing(t *testing.T) {
	nk := seedStreamCrew(t)
	sessionID := startTestStreamOnLive(t, nk)
	if _, err := streamLive(nk, sfuOnlyUser, sessionID); err != nil {
		t.Fatal(err)
	}
	watchTestStream(t, nk, streamTestViewer, sessionID)
	if _, err := streamLive(nk, sfuOnlyUser, sessionID); err != nil {
		t.Fatalf("second stream_live: %v", err)
	}
	if got := viewerCount(nk); got != 1 {
		t.Fatalf("a second stream_live reset the viewer count to %d", got)
	}
}

// stream_started carries the mode, size and rate: a viewer starts its
// decoder with no second RPC.
func TestStreamStartedPayload_CarriesModeSizeAndRate(t *testing.T) {
	data := streamStartedPayload(streamAdvert{
		SessionID: "s1", HostID: "h1", HostName: "alice", Title: "t",
		Width: 1280, Height: 720, Fps: 60,
	})
	if data["mode"] != "sfu" || data["width"] != uint32(1280) || data["height"] != uint32(720) ||
		data["fps"] != uint32(60) || data["stream_id"] != "s1" {
		t.Fatalf("stream_started data %v", data)
	}
}

// END STREAM before the track opened removes the unadvertised session.
func TestStopStream_RemovesAnUnadvertisedStream(t *testing.T) {
	nk := seedStreamCrew(t)
	sessionID := startTestStreamOnLive(t, nk)
	if _, err := StopStreamRPC(ctxWithUser(sfuOnlyUser), testLogger(), nil, nk,
		`{"crew_id":"`+sfuOnlyCrew+`","session_id":"`+sessionID+`"}`); err != nil {
		t.Fatalf("stop_stream: %v", err)
	}
	if nk.has(StreamSessionCol, sessionID, SystemUserID) {
		t.Fatal("stop_stream left the unadvertised session record")
	}
}

func refreshTestToken(nk *fakeStreamNk, userID, sessionID string) (string, error) {
	return RefreshStreamTokenRPC(ctxWithUser(userID), testLogger(), nil, nk,
		`{"session_id":"`+sessionID+`"}`)
}

func tokenClaims(t *testing.T, token string) SFUTokenClaims {
	t.Helper()
	parts := strings.Split(token, ".")
	if len(parts) != 3 {
		t.Fatalf("token is not a JWT: %q", token)
	}
	payload, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		t.Fatal(err)
	}
	var claims SFUTokenClaims
	if err := json.Unmarshal(payload, &claims); err != nil {
		t.Fatal(err)
	}
	return claims
}

// refresh_stream_token gives the host a host token and a crew member a
// viewer token for the same session; anyone else gets nothing.
func TestRefreshStreamToken_RolesFollowTheSession(t *testing.T) {
	nk := seedStreamCrew(t)
	sessionID := startTestStream(t, nk)

	for _, c := range []struct {
		user string
		role string
	}{{sfuOnlyUser, "host"}, {streamTestViewer, "viewer"}} {
		out, err := refreshTestToken(nk, c.user, sessionID)
		if err != nil {
			t.Fatalf("refresh_stream_token for %s: %v", c.user, err)
		}
		var resp struct {
			Endpoint string `json:"sfu_endpoint"`
			Token    string `json:"sfu_token"`
			Role     string `json:"role"`
		}
		if err := json.Unmarshal([]byte(out), &resp); err != nil || resp.Endpoint == "" {
			t.Fatalf("refresh_stream_token response %q: %v", out, err)
		}
		claims := tokenClaims(t, resp.Token)
		if resp.Role != c.role || claims.Role != c.role || claims.SessionID != sessionID ||
			claims.UserID != c.user || claims.Type != "stream" {
			t.Fatalf("token for %s: role %q claims %+v, want role %s", c.user, resp.Role, claims, c.role)
		}
	}

	_, err := refreshTestToken(nk, "user-not-in-crew", sessionID)
	assertRuntimeCode(t, err, 7)
	_, err = refreshTestToken(nk, sfuOnlyUser, "stream_unknown")
	assertRuntimeCode(t, err, 5)
}
