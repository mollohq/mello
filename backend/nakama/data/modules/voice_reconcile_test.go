package main

import (
	"net/http"
	"net/http/httptest"
	"testing"
)

func sfuAdmin(t *testing.T, status int, body string) string {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if user, pass, ok := r.BasicAuth(); !ok || user != "nakama" || pass != "pw" {
			w.WriteHeader(http.StatusUnauthorized)
			return
		}
		w.WriteHeader(status)
		_, _ = w.Write([]byte(body))
	}))
	t.Cleanup(srv.Close)
	return srv.URL
}

func TestQuerySFUSession_FoundListsPeers(t *testing.T) {
	base := sfuAdmin(t, http.StatusOK, `{"peers":[{"user_id":"a"},{"user_id":"b"}]}`)
	m, state := querySFUSession(base, "pw", "voice:c:ch")
	if state != sfuSessionFound || !m["a"] || !m["b"] || len(m) != 2 {
		t.Fatalf("got %v %v", m, state)
	}
}

func TestQuerySFUSession_404IsAbsent(t *testing.T) {
	base := sfuAdmin(t, http.StatusNotFound, `404 page not found`)
	if _, state := querySFUSession(base, "pw", "voice:c:ch"); state != sfuSessionAbsent {
		t.Fatalf("got %v, want absent", state)
	}
}

func TestQuerySFUSession_OtherStatusIsFailed(t *testing.T) {
	for _, status := range []int{http.StatusUnauthorized, http.StatusInternalServerError, http.StatusBadGateway} {
		base := sfuAdmin(t, status, ``)
		if _, state := querySFUSession(base, "pw", "voice:c:ch"); state != sfuLookupFailed {
			t.Fatalf("status %d: got %v, want failed", status, state)
		}
	}
}

func TestQuerySFUSession_UnreachableIsFailed(t *testing.T) {
	if _, state := querySFUSession("http://127.0.0.1:1", "pw", "voice:c:ch"); state != sfuLookupFailed {
		t.Fatalf("got %v, want failed", state)
	}
}

func TestLookupSFUSession_AbsentOnlyWhenEverySFUSaysSo(t *testing.T) {
	absent := sfuAdmin(t, http.StatusNotFound, ``)
	down := sfuAdmin(t, http.StatusBadGateway, ``)
	found := sfuAdmin(t, http.StatusOK, `{"peers":[{"user_id":"a"}]}`)

	if _, s := lookupSFUSession(map[string]string{"eu": absent, "us": absent}, "pw", "x"); s != sfuSessionAbsent {
		t.Fatalf("all 404: got %v", s)
	}
	if _, s := lookupSFUSession(map[string]string{"eu": absent, "us": down}, "pw", "x"); s != sfuLookupFailed {
		t.Fatalf("one SFU down must not read as absent: got %v", s)
	}
	if m, s := lookupSFUSession(map[string]string{"eu": absent, "us": found}, "pw", "x"); s != sfuSessionFound || !m["a"] {
		t.Fatalf("found on one SFU: got %v %v", m, s)
	}
	if _, s := lookupSFUSession(map[string]string{}, "pw", "x"); s != sfuLookupFailed {
		t.Fatalf("no SFUs: got %v", s)
	}
}

func TestAbsentFromSFU(t *testing.T) {
	members := map[string]bool{"here": true}
	cases := []struct {
		name          string
		state         sfuLookup
		uid           string
		guest         bool
		absent, known bool
	}{
		{"in the session", sfuSessionFound, "here", false, false, true},
		{"missing from a live session", sfuSessionFound, "gone", false, true, true},
		{"guest, session gone", sfuSessionAbsent, "g", true, true, true},
		// A member's room may be P2P, which no SFU knows: never prune on 404.
		{"member, session gone", sfuSessionAbsent, "m", false, false, false},
		{"lookup failed", sfuLookupFailed, "g", true, false, false},
	}
	for _, c := range cases {
		absent, known := absentFromSFU(c.state, members, c.uid, c.guest)
		if absent != c.absent || known != c.known {
			t.Errorf("%s: got absent=%v known=%v, want %v %v", c.name, absent, known, c.absent, c.known)
		}
	}
}
