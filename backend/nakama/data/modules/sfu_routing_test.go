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

// twoSFURegions is the SFU_REGIONS of a deployment with two regions.
const twoSFURegions = "eu-west=wss://sfu-eu.example.com:8443/ws,us-east=wss://sfu-us.example.com:8443/ws"

// useSFURegions loads the SFU regions from env for one test and puts the
// previous configuration back after it.
func useSFURegions(t *testing.T, env map[string]string) {
	t.Helper()
	endpoints, order, problems := sfuEndpoints, sfuRegionOrder, sfuRegionProblems
	t.Cleanup(func() { sfuEndpoints, sfuRegionOrder, sfuRegionProblems = endpoints, order, problems })
	loadSFURegions(func(k string) string { return env[k] })
}

func TestSelectSFURegion_PrefersAConfiguredClientChoice(t *testing.T) {
	useSFURegions(t, map[string]string{"SFU_REGIONS": twoSFURegions})
	t.Setenv("SFU_DEFAULT_REGION", "")
	if got := selectSFURegion("us-east"); got != "us-east" {
		t.Fatalf("preferred us-east: got %q", got)
	}
	if got := selectSFURegion("eu-west"); got != "eu-west" {
		t.Fatalf("preferred eu-west: got %q", got)
	}
}

func TestSelectSFURegion_UnknownOrEmptyFallsBackToTheFirstRegion(t *testing.T) {
	useSFURegions(t, map[string]string{"SFU_REGIONS": twoSFURegions})
	t.Setenv("SFU_DEFAULT_REGION", "")
	for _, preferred := range []string{"", "ap-south", "US-EAST", " us-east"} {
		if got := selectSFURegion(preferred); got != "eu-west" {
			t.Errorf("preferred %q: got %q, want eu-west", preferred, got)
		}
	}
}

func TestSelectSFURegion_DefaultRegionFromEnvironment(t *testing.T) {
	useSFURegions(t, map[string]string{"SFU_REGIONS": twoSFURegions})
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
	useSFURegions(t, map[string]string{"SFU_REGIONS": twoSFURegions})
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
	useSFURegions(t, map[string]string{"SFU_REGIONS": twoSFURegions})
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
	useSFURegions(t, map[string]string{"SFU_REGIONS": twoSFURegions})

	out, err := GetSFURegionsRPC(ctxWithUser("u1"), testLogger(), nil, nil, "{}")
	if err != nil {
		t.Fatal(err)
	}
	var resp GetSFURegionsResponse
	if err := json.Unmarshal([]byte(out), &resp); err != nil {
		t.Fatalf("response is not JSON: %v: %s", err, out)
	}
	want := []SFURegion{
		{Region: "eu-west", HealthURL: "https://sfu-eu.example.com:8080/health"},
		{Region: "us-east", HealthURL: "https://sfu-us.example.com:8080/health"},
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
	useSFURegions(t, map[string]string{"SFU_REGIONS": twoSFURegions})
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

// No built-in SFU: a backend without SFU_REGIONS (or the older
// SFU_ENDPOINT_EU/US) has no region, no default and no SFU.
func TestSFURegions_NoneConfiguredMeansNoSFU(t *testing.T) {
	useSFURegions(t, map[string]string{})
	t.Setenv("SFU_DEFAULT_REGION", "eu-west")
	if sfuRegionsConfigured() {
		t.Fatal("regions configured with an empty environment")
	}
	if got := selectSFURegion("eu-west"); got != "" {
		t.Fatalf("selectSFURegion: got %q, want none", got)
	}
	if got := sfuEndpointForRegion("eu-west"); got != "" {
		t.Fatalf("sfuEndpointForRegion: got %q, want none", got)
	}
	if got := sfuRegions(); len(got) != 0 {
		t.Fatalf("sfuRegions: got %+v, want none", got)
	}
	saved := sfuPrivateKey
	key, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		t.Fatal(err)
	}
	sfuPrivateKey = key
	defer func() { sfuPrivateKey = saved }()
	if sfuAvailable() {
		t.Fatal("sfuAvailable with a key and no region")
	}
}

// A self-hoster with one SFU: get_sfu_regions lists only it, and every
// preference ends there. Before, the backend also listed the official
// us-east SFU, which refuses tokens signed with another key.
func TestSFURegions_OneSelfHostedRegion(t *testing.T) {
	useSFURegions(t, map[string]string{"SFU_REGIONS": "home=wss://sfu.example.com:8443/ws"})
	t.Setenv("SFU_DEFAULT_REGION", "")
	got := sfuRegions()
	want := []SFURegion{{Region: "home", HealthURL: "https://sfu.example.com:8080/health"}}
	if len(got) != 1 || got[0] != want[0] {
		t.Fatalf("sfuRegions: got %+v, want %+v", got, want)
	}
	for _, preferred := range []string{"", "us-east", "eu-west", "home"} {
		if r := selectSFURegion(preferred); r != "home" {
			t.Errorf("preferred %q: region %q, want home", preferred, r)
		}
	}
	if ep := sfuEndpointForRegion("us-east"); ep != "wss://sfu.example.com:8443/ws" {
		t.Errorf("endpoint for an unknown region: %q", ep)
	}
}

// The default is the first region in SFU_REGIONS; SFU_DEFAULT_REGION
// overrides it when it names a configured region.
func TestSFURegions_DefaultIsTheFirstRegion(t *testing.T) {
	useSFURegions(t, map[string]string{"SFU_REGIONS": "us-east=wss://a.example.com/ws,eu-west=wss://b.example.com/ws"})
	t.Setenv("SFU_DEFAULT_REGION", "")
	if got := defaultSFURegion(); got != "us-east" {
		t.Fatalf("default: got %q, want us-east", got)
	}
	t.Setenv("SFU_DEFAULT_REGION", "eu-west")
	if got := defaultSFURegion(); got != "eu-west" {
		t.Fatalf("default with SFU_DEFAULT_REGION=eu-west: got %q", got)
	}
}

// Without SFU_REGIONS the older SFU_ENDPOINT_EU and SFU_ENDPOINT_US still
// work, and only the ones that are set become regions.
func TestSFURegions_OlderEndpointVariables(t *testing.T) {
	useSFURegions(t, map[string]string{"SFU_ENDPOINT_EU": "wss://sfu-eu.example.com:8443/ws"})
	if got := strings.Join(sfuRegionOrder, ","); got != "eu-west" {
		t.Fatalf("regions: got %q, want eu-west only", got)
	}
	useSFURegions(t, map[string]string{
		"SFU_REGIONS":     "home=ws://127.0.0.1:8443/ws",
		"SFU_ENDPOINT_EU": "wss://sfu-eu.example.com:8443/ws",
	})
	if got := strings.Join(sfuRegionOrder, ","); got != "home" {
		t.Fatalf("SFU_REGIONS wins over SFU_ENDPOINT_EU: got %q", got)
	}
}

// Bad entries are left out and named for the start log; good ones stay.
func TestSFURegions_BadEntriesAreLeftOutAndReported(t *testing.T) {
	useSFURegions(t, map[string]string{"SFU_REGIONS": " eu-west = wss://a.example.com/ws ,nonsense,=wss://b.example.com/ws,x=https://c.example.com/ws,eu-west=wss://d.example.com/ws,"})
	if got := strings.Join(sfuRegionOrder, ","); got != "eu-west" {
		t.Fatalf("regions: got %q, want eu-west", got)
	}
	if ep := sfuEndpoints["eu-west"]; ep != "wss://a.example.com/ws" {
		t.Fatalf("eu-west endpoint %q, want the first entry, trimmed", ep)
	}
	if len(sfuRegionProblems) != 4 {
		t.Fatalf("problems: %q, want 4 (no '=', no name, not ws, duplicate)", sfuRegionProblems)
	}
}

func TestSFUHealthURL_PortFromEnvironment(t *testing.T) {
	t.Setenv("SFU_HEALTH_PORT", "9090")
	if got := sfuHealthURL("wss://sfu.example.com:8443/ws"); got != "https://sfu.example.com:9090/health" {
		t.Fatalf("got %q", got)
	}
}
