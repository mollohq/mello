package main

import (
	"crypto/rand"
	"crypto/rsa"
	"encoding/base64"
	"encoding/json"
	"strings"
	"testing"
)

func TestSelectSFURegion_PrefersAConfiguredClientChoice(t *testing.T) {
	t.Setenv("SFU_DEFAULT_REGION", "")
	if got := selectSFURegion("us-east"); got != "us-east" {
		t.Fatalf("preferred us-east: got %q", got)
	}
	if got := selectSFURegion("eu-west"); got != "eu-west" {
		t.Fatalf("preferred eu-west: got %q", got)
	}
}

func TestSelectSFURegion_UnknownOrEmptyFallsBackToEuWest(t *testing.T) {
	t.Setenv("SFU_DEFAULT_REGION", "")
	for _, preferred := range []string{"", "ap-south", "US-EAST", " us-east"} {
		if got := selectSFURegion(preferred); got != "eu-west" {
			t.Errorf("preferred %q: got %q, want eu-west", preferred, got)
		}
	}
}

func TestSelectSFURegion_DefaultRegionFromEnvironment(t *testing.T) {
	t.Setenv("SFU_DEFAULT_REGION", "us-east")
	if got := selectSFURegion(""); got != "us-east" {
		t.Fatalf("no preference with SFU_DEFAULT_REGION=us-east: got %q", got)
	}
	if got := selectSFURegion("eu-west"); got != "eu-west" {
		t.Fatalf("the client choice beats the default: got %q", got)
	}

	// A default that names no configured endpoint is ignored.
	t.Setenv("SFU_DEFAULT_REGION", "mars-1")
	if got := selectSFURegion(""); got != "eu-west" {
		t.Fatalf("unknown SFU_DEFAULT_REGION: got %q, want eu-west", got)
	}
}

func TestVoiceRoomSFURegion_FirstMemberPicksAndTheRoomKeepsIt(t *testing.T) {
	t.Setenv("SFU_DEFAULT_REGION", "")
	resetVoiceState()
	defer resetVoiceState()

	voiceRoomsMu.Lock()
	voiceRooms["ch_region"] = &VoiceRoom{
		ChannelID: "ch_region",
		CrewID:    "crew_region",
		Members:   map[string]*VoiceMemberState{"a": {UserID: "a"}},
	}
	voiceRoomsMu.Unlock()

	if got := voiceRoomSFURegion("ch_region", "us-east"); got != "us-east" {
		t.Fatalf("first member: got %q", got)
	}
	// A second member who prefers another region must still reach the same
	// SFU instance: instances do not relay between regions.
	if got := voiceRoomSFURegion("ch_region", "eu-west"); got != "us-east" {
		t.Fatalf("second member: got %q, want the room's us-east", got)
	}
}

// voice_join issues the SFU token and endpoint for the preferred region, and
// the token carries it.
func TestIssueVoiceSFUToken_UsesThePreferredRegion(t *testing.T) {
	t.Setenv("SFU_DEFAULT_REGION", "")
	resetVoiceState()
	defer resetVoiceState()
	key, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		t.Fatal(err)
	}
	saved := sfuPrivateKey
	sfuPrivateKey = key
	defer func() { sfuPrivateKey = saved }()

	voiceRoomsMu.Lock()
	voiceRooms["ch_tok"] = &VoiceRoom{
		ChannelID: "ch_tok",
		CrewID:    "crew_tok",
		Members:   map[string]*VoiceMemberState{"u1": {UserID: "u1"}},
	}
	voiceRoomsMu.Unlock()

	endpoint, token, ok := issueVoiceSFUToken(testLogger(), voiceJoinParams{
		CrewID:          "crew_tok",
		ChannelID:       "ch_tok",
		UserID:          "u1",
		PreferredRegion: "us-east",
	})
	if !ok {
		t.Fatal("token not issued")
	}
	if endpoint != sfuEndpoints["us-east"] {
		t.Fatalf("endpoint %q, want the us-east endpoint", endpoint)
	}
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
	if claims.Region != "us-east" {
		t.Fatalf("token region %q, want us-east", claims.Region)
	}
}

func TestStartStreamRequest_AcceptsPreferredRegion(t *testing.T) {
	var start StartStreamRequest
	if err := json.Unmarshal([]byte(`{"crew_id":"c","preferred_region":"us-east"}`), &start); err != nil {
		t.Fatal(err)
	}
	if start.PreferredRegion != "us-east" {
		t.Fatalf("start_stream: got %q", start.PreferredRegion)
	}
}
