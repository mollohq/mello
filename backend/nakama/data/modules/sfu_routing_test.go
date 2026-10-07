package main

import (
	"context"
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

func TestSFUHealthURL_DerivedFromTheSignalingEndpoint(t *testing.T) {
	cases := map[string]string{
		"wss://sfu-eu.m3llo.app/ws":      "https://sfu-eu.m3llo.app:8080/health",
		"wss://sfu-eu.m3llo.app:8443/ws": "https://sfu-eu.m3llo.app:8080/health",
		"ws://localhost:8443/ws":         "http://localhost:8080/health",
		"ws://192.168.1.67:8443/ws":      "http://192.168.1.67:8080/health",
		"ws://[::1]:8443/ws":             "http://[::1]:8080/health",
		"https://sfu-eu.m3llo.app/ws":    "",
		"":                               "",
		"not a url":                      "",
	}
	for endpoint, want := range cases {
		if got := sfuHealthURL(endpoint); got != want {
			t.Errorf("sfuHealthURL(%q) = %q, want %q", endpoint, got, want)
		}
	}
}

func TestGetSFURegionsRPC_ListsBothRegionsWithHealthURLs(t *testing.T) {
	saved := map[string]string{}
	for k, v := range sfuEndpoints {
		saved[k] = v
	}
	defer func() {
		for k := range sfuEndpoints {
			delete(sfuEndpoints, k)
		}
		for k, v := range saved {
			sfuEndpoints[k] = v
		}
	}()
	sfuEndpoints["eu-west"] = "wss://sfu-eu.m3llo.app:8443/ws"
	sfuEndpoints["us-east"] = "wss://sfu-us.m3llo.app:8443/ws"

	out, err := GetSFURegionsRPC(ctxWithUser("u1"), testLogger(), nil, nil, "{}")
	if err != nil {
		t.Fatal(err)
	}
	var resp GetSFURegionsResponse
	if err := json.Unmarshal([]byte(out), &resp); err != nil {
		t.Fatalf("response is not JSON: %v: %s", err, out)
	}
	want := []SFURegion{
		{Region: "eu-west", HealthURL: "https://sfu-eu.m3llo.app:8080/health"},
		{Region: "us-east", HealthURL: "https://sfu-us.m3llo.app:8080/health"},
	}
	if len(resp.Regions) != len(want) {
		t.Fatalf("regions %+v, want %+v", resp.Regions, want)
	}
	for i := range want {
		if resp.Regions[i] != want[i] {
			t.Errorf("region %d: %+v, want %+v", i, resp.Regions[i], want[i])
		}
	}
	// Every region the RPC names is one that voice_join accepts.
	for _, r := range resp.Regions {
		if !isSFURegion(r.Region) {
			t.Errorf("region %q is not a configured SFU region", r.Region)
		}
	}
}

func TestGetSFURegionsRPC_LeavesOutAnEndpointWithoutHealthURL(t *testing.T) {
	saved := sfuEndpoints["us-east"]
	defer func() { sfuEndpoints["us-east"] = saved }()
	sfuEndpoints["us-east"] = "not a url"

	out, err := GetSFURegionsRPC(ctxWithUser("u1"), testLogger(), nil, nil, "{}")
	if err != nil {
		t.Fatal(err)
	}
	var resp GetSFURegionsResponse
	if err := json.Unmarshal([]byte(out), &resp); err != nil {
		t.Fatal(err)
	}
	for _, r := range resp.Regions {
		if r.Region == "us-east" {
			t.Fatalf("us-east listed with an unusable endpoint: %+v", r)
		}
	}
}

func TestGetSFURegionsRPC_RequiresAUser(t *testing.T) {
	if _, err := GetSFURegionsRPC(context.Background(), testLogger(), nil, nil, "{}"); err == nil {
		t.Fatal("an unauthenticated call got the region list")
	}
}
