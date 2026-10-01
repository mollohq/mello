package main

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"reflect"
	"strings"
	"testing"
	"time"
)

func readFixture(t *testing.T, name string) []byte {
	t.Helper()
	b, err := os.ReadFile("testdata/" + name)
	if err != nil {
		t.Fatalf("read fixture: %v", err)
	}
	return b
}

func TestPushTokenKeyIsStableHexAndFitsNakamaKeys(t *testing.T) {
	long := strings.Repeat("f", 300) // FCM-length token
	k := pushTokenKey(long)
	if len(k) != 64 || k != pushTokenKey(long) {
		t.Fatalf("key %q must be 64 stable hex chars", k)
	}
	if pushTokenKey("a") == pushTokenKey("b") {
		t.Fatal("different tokens must not share a key")
	}
}

func TestValidatePushToken(t *testing.T) {
	cases := []struct {
		name    string
		req     registerPushTokenRequest
		wantErr bool
		wantEnv string
	}{
		{"ios default env", registerPushTokenRequest{Token: "ab12", Platform: "ios"}, false, "production"},
		{"ios sandbox", registerPushTokenRequest{Token: "AB12", Platform: "ios", Environment: "sandbox"}, false, "sandbox"},
		{"android any chars", registerPushTokenRequest{Token: "fcm:abc-_1", Platform: "android"}, false, "production"},
		{"ios non-hex", registerPushTokenRequest{Token: "zz", Platform: "ios"}, true, ""},
		{"empty token", registerPushTokenRequest{Platform: "ios"}, true, ""},
		{"too long", registerPushTokenRequest{Token: strings.Repeat("a", maxPushTokenChars+1), Platform: "ios"}, true, ""},
		{"bad platform", registerPushTokenRequest{Token: "ab", Platform: "web"}, true, ""},
		{"bad environment", registerPushTokenRequest{Token: "ab", Platform: "ios", Environment: "staging"}, true, ""},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			req := c.req
			err := validatePushToken(&req)
			if (err != nil) != c.wantErr {
				t.Fatalf("err = %v, wantErr %v", err, c.wantErr)
			}
			if !c.wantErr && req.Environment != c.wantEnv {
				t.Fatalf("environment = %q, want %q", req.Environment, c.wantEnv)
			}
		})
	}
}

func TestMentionPushTargetsDropsSenderDuplicatesAndCaps(t *testing.T) {
	got := mentionPushTargets([]string{"u2", "me", "", "u3", "u2"}, "me")
	if want := []string{"u2", "u3"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("targets = %v, want %v", got, want)
	}
	many := make([]string, 0, 30)
	for i := 0; i < 30; i++ {
		many = append(many, "u"+string(rune('a'+i)))
	}
	if n := len(mentionPushTargets(many, "me")); n != maxMentionPushes {
		t.Fatalf("got %d targets, want cap %d", n, maxMentionPushes)
	}
}

func TestPushBodyIsSenderPrefixedAndBounded(t *testing.T) {
	if got := pushBody("bob", "@alice hi"); got != "bob: @alice hi" {
		t.Fatalf("body = %q", got)
	}
	got := pushBody("bob", strings.Repeat("å", 400))
	if n := len([]rune(got)); n != pushBodyMaxRunes {
		t.Fatalf("body is %d runes, want %d", n, pushBodyMaxRunes)
	}
}

func TestHasActiveSessions(t *testing.T) {
	resetSessionCountsForTests()
	defer resetSessionCountsForTests()
	if HasActiveSessions("u1") {
		t.Fatal("no session yet")
	}
	registerSessionStart("u1")
	if !HasActiveSessions("u1") {
		t.Fatal("one session registered")
	}
	registerSessionEnd("u1")
	if HasActiveSessions("u1") {
		t.Fatal("the session ended")
	}
}

// The request Nakama encodes and the response it decodes must match the
// contract fixtures shared with mello-push-worker (spec 23 §7.5).
func TestPostPushSendMatchesContractFixtures(t *testing.T) {
	wantReq := readFixture(t, "send_request.json")
	respBody := readFixture(t, "send_response.json")

	var gotAuth, gotPath string
	var gotReq map[string]interface{}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotAuth, gotPath = r.Header.Get("Authorization"), r.URL.Path
		b, _ := io.ReadAll(r.Body)
		_ = json.Unmarshal(b, &gotReq)
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write(respBody)
	}))
	defer srv.Close()

	var req pushSendRequest
	if err := json.Unmarshal(wantReq, &req); err != nil {
		t.Fatalf("fixture must decode into pushSendRequest: %v", err)
	}
	resp, err := postPushSend(context.Background(), srv.Client(), srv.URL+"/", "secret", &req)
	if err != nil {
		t.Fatalf("postPushSend: %v", err)
	}

	if gotAuth != "Bearer secret" || gotPath != "/send" {
		t.Fatalf("auth %q path %q", gotAuth, gotPath)
	}
	var want map[string]interface{}
	_ = json.Unmarshal(wantReq, &want)
	if !reflect.DeepEqual(gotReq, want) {
		t.Fatalf("request = %v\nwant %v", gotReq, want)
	}

	wantResp := &pushSendResponse{
		Results: []pushTokenResult{
			{Token: "a1b2", Status: "sent", APNsID: "apns-1"},
			{Token: "c3d4", Status: "pruned", Reason: "Unregistered"},
			{Token: "e5f6", Status: "failed", Reason: "TooManyRequests"},
		},
		Prune: []string{"c3d4"},
	}
	if !reflect.DeepEqual(resp, wantResp) {
		t.Fatalf("response = %+v\nwant %+v", resp, wantResp)
	}
}

func TestPostPushSendReportsWorkerErrors(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
		_, _ = w.Write([]byte(`{"error":"unauthorized"}`))
	}))
	defer srv.Close()
	_, err := postPushSend(context.Background(), srv.Client(), srv.URL, "wrong", &pushSendRequest{})
	if err == nil || !strings.Contains(err.Error(), "401") {
		t.Fatalf("err = %v, want a 401 error", err)
	}

	slow := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		time.Sleep(200 * time.Millisecond)
	}))
	defer slow.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
	defer cancel()
	if _, err := postPushSend(ctx, slow.Client(), slow.URL, "x", &pushSendRequest{}); err == nil {
		t.Fatal("a slow Worker must time out, not block the fan-out")
	}
}
