package main

import (
	"crypto/rand"
	"crypto/rsa"
	"errors"
	"strings"
	"testing"

	"github.com/heroiclabs/nakama-common/api"
	"github.com/heroiclabs/nakama-common/runtime"
)

const (
	sfuOnlyCrew = "crew-sfu-only"
	sfuOnlyUser = "user-sfu-only-0001"
)

func seedSfuOnlyCrew(metadata string) *fakeGuestNk {
	nk := newFakeGuestNk()
	nk.groups[sfuOnlyCrew] = &api.Group{Id: sfuOnlyCrew, Name: "Night Owls", Metadata: metadata}
	nk.addMember(sfuOnlyCrew, sfuOnlyUser, "alice")
	nk.put(VoiceChannelCollection, sfuOnlyCrew, SystemUserID, VoiceChannelList{Channels: []*VoiceChannelDef{
		{ID: "ch_general", Name: "General", IsDefault: true},
	}})
	return nk
}

// withSFUKey sets the SFU signing key for one test. A nil key means the
// server has no SFU configured.
func withSFUKey(t *testing.T, configured bool) {
	t.Helper()
	saved := sfuPrivateKey
	sfuPrivateKey = nil
	if configured {
		key, err := rsa.GenerateKey(rand.Reader, 2048)
		if err != nil {
			t.Fatal(err)
		}
		sfuPrivateKey = key
	}
	t.Cleanup(func() { sfuPrivateKey = saved })
}

func assertSFUUnavailable(t *testing.T, err error) {
	t.Helper()
	var rerr *runtime.Error
	if !errors.As(err, &rerr) {
		t.Fatalf("want the SFU_UNAVAILABLE runtime error, got %v", err)
	}
	if rerr.Message != "SFU_UNAVAILABLE" || rerr.Code != 14 {
		t.Fatalf("want SFU_UNAVAILABLE code 14, got %q code %d", rerr.Message, rerr.Code)
	}
}

func assertNoStreamState(t *testing.T, nk *fakeGuestNk) {
	t.Helper()
	nk.mu.Lock()
	defer nk.mu.Unlock()
	for key := range nk.storage {
		if strings.HasPrefix(key, StreamCollection+"/") ||
			strings.HasPrefix(key, StreamMetaCollection+"/") ||
			strings.HasPrefix(key, StreamSessionCol+"/") {
			t.Errorf("a failed start_stream left state behind: %s", key)
		}
	}
}

// A server without an SFU key fails start_stream and writes no stream.
func TestStartStream_NoSFUKey_ReturnsSFUUnavailable(t *testing.T) {
	withSFUKey(t, false)
	nk := seedSfuOnlyCrew("")

	_, err := StartStreamRPC(ctxWithUser(sfuOnlyUser), testLogger(), nil, nk,
		`{"crew_id":"`+sfuOnlyCrew+`","title":"t"}`)

	assertSFUUnavailable(t, err)
	assertNoStreamState(t, nk)
}

// The crew kill switch fails start_stream even when the server has a key.
func TestStartStream_KillSwitch_ReturnsSFUUnavailable(t *testing.T) {
	withSFUKey(t, true)
	nk := seedSfuOnlyCrew(`{"sfu_enabled":false}`)

	_, err := StartStreamRPC(ctxWithUser(sfuOnlyUser), testLogger(), nil, nk,
		`{"crew_id":"`+sfuOnlyCrew+`","title":"t"}`)

	assertSFUUnavailable(t, err)
	assertNoStreamState(t, nk)
}

// A stream session without an SFU endpoint has no transport for a viewer.
func TestWatchStream_SessionWithoutEndpoint_ReturnsSFUUnavailable(t *testing.T) {
	withSFUKey(t, true)
	nk := seedSfuOnlyCrew("")
	nk.put(StreamSessionCol, "stream_old", SystemUserID, map[string]string{
		"crew_id": sfuOnlyCrew, "host_user_id": "host",
	})

	_, err := WatchStreamRPC(ctxWithUser(sfuOnlyUser), testLogger(), nil, nk,
		`{"session_id":"stream_old"}`)

	assertSFUUnavailable(t, err)
}

// A server without an SFU key fails voice_join and seats nobody.
func TestVoiceJoin_NoSFUKey_ReturnsSFUUnavailable(t *testing.T) {
	resetVoiceState()
	defer resetVoiceState()
	withSFUKey(t, false)
	nk := seedSfuOnlyCrew("")

	_, err := VoiceJoinRPC(ctxWithUser(sfuOnlyUser), testLogger(), nil, nk,
		`{"crew_id":"`+sfuOnlyCrew+`"}`)

	assertSFUUnavailable(t, err)
	voiceUserChannelMu.RLock()
	_, seated := voiceUserChannel[sfuOnlyUser]
	voiceUserChannelMu.RUnlock()
	if seated {
		t.Error("a failed voice_join seated the user")
	}
}

// The crew kill switch fails voice_join even when the server has a key.
func TestVoiceJoin_KillSwitch_ReturnsSFUUnavailable(t *testing.T) {
	resetVoiceState()
	defer resetVoiceState()
	withSFUKey(t, true)
	nk := seedSfuOnlyCrew(`{"sfu_enabled":false}`)

	_, err := VoiceJoinRPC(ctxWithUser(sfuOnlyUser), testLogger(), nil, nk,
		`{"crew_id":"`+sfuOnlyCrew+`"}`)

	assertSFUUnavailable(t, err)
}
