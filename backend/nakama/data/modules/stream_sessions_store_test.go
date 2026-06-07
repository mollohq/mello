package main

import "testing"

func storedSession(sessionID string, ts int64, snapshots int) StoredStreamSession {
	urls := make([]string, snapshots)
	for i := range urls {
		urls[i] = "u"
	}
	return StoredStreamSession{
		EventID:      sessionID + "_evt",
		SessionID:    sessionID,
		Ts:           ts,
		SnapshotURLs: urls,
	}
}

func TestCapStreamSessionsKeepsNewest(t *testing.T) {
	sessions := make([]StoredStreamSession, 0, CrewStreamSessionsMaxRetained+10)
	for i := 0; i < CrewStreamSessionsMaxRetained+10; i++ {
		sessions = append(sessions, storedSession("s"+string(rune(i)), int64(i), 0))
	}
	capped := capStreamSessions(sessions)
	if len(capped) != CrewStreamSessionsMaxRetained {
		t.Fatalf("cap: got %d want %d", len(capped), CrewStreamSessionsMaxRetained)
	}
	// Oldest (ts 0..9) dropped; newest retained.
	if capped[0].Ts != 10 {
		t.Fatalf("oldest retained ts: got %d want 10", capped[0].Ts)
	}
}

func TestCapStreamSessionsUnderLimitUnchanged(t *testing.T) {
	in := []StoredStreamSession{storedSession("a", 1, 0), storedSession("b", 2, 0)}
	if got := capStreamSessions(in); len(got) != 2 {
		t.Fatalf("under limit: got %d want 2", len(got))
	}
}

func TestUpsertStreamSessionInserts(t *testing.T) {
	got := upsertStreamSession(nil, storedSession("a", 1, 3))
	if len(got) != 1 || got[0].SessionID != "a" {
		t.Fatalf("expected single inserted session, got %+v", got)
	}
}

func TestUpsertStreamSessionUpdatesInPlace(t *testing.T) {
	sessions := []StoredStreamSession{storedSession("a", 1, 2), storedSession("b", 2, 0)}
	updated := storedSession("a", 1, 5) // same session, more snapshots
	got := upsertStreamSession(sessions, updated)
	if len(got) != 2 {
		t.Fatalf("upsert should not grow the slice: got %d want 2", len(got))
	}
	if len(got[0].SnapshotURLs) != 5 {
		t.Fatalf("snapshots should grow to 5, got %d", len(got[0].SnapshotURLs))
	}
}

// Snapshots only grow: an update carrying fewer URLs (e.g. a transient empty
// list) must not erase a richer set already stored.
func TestUpsertStreamSessionNeverShrinksSnapshots(t *testing.T) {
	sessions := []StoredStreamSession{storedSession("a", 1, 6)}
	got := upsertStreamSession(sessions, storedSession("a", 1, 0))
	if len(got[0].SnapshotURLs) != 6 {
		t.Fatalf("snapshots must not shrink: got %d want 6", len(got[0].SnapshotURLs))
	}
}
