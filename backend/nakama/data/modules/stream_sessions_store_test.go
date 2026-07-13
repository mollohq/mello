package main

import (
	"encoding/json"
	"testing"
)

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

func TestStreamSessionNeedsDurableUpsertWhenMissing(t *testing.T) {
	sessions := []StoredStreamSession{storedSession("other", 1, 3)}
	if !streamSessionNeedsDurableUpsert(sessions, "a_evt", "a", 3) {
		t.Fatal("missing durable session should require upsert")
	}
}

func TestStreamSessionNeedsDurableUpsertWhenSnapshotCountGrows(t *testing.T) {
	sessions := []StoredStreamSession{storedSession("a", 1, 2)}
	if !streamSessionNeedsDurableUpsert(sessions, "a_evt", "a", 5) {
		t.Fatal("durable session with fewer snapshots should require upsert")
	}
}

func TestStreamSessionSkipsDurableUpsertWhenCurrent(t *testing.T) {
	sessions := []StoredStreamSession{storedSession("a", 1, 5)}
	if streamSessionNeedsDurableUpsert(sessions, "a_evt", "a", 5) {
		t.Fatal("current durable session should not require upsert")
	}
}

func TestStreamBitratePersistsInStoredMetadata(t *testing.T) {
	const bitrateKbps uint32 = 4500

	requestJSON, err := json.Marshal(StartStreamRequest{BitrateKbps: bitrateKbps})
	if err != nil {
		t.Fatalf("marshal start stream request: %v", err)
	}
	var request StartStreamRequest
	if err := json.Unmarshal(requestJSON, &request); err != nil {
		t.Fatalf("unmarshal start stream request: %v", err)
	}
	if request.BitrateKbps != bitrateKbps {
		t.Fatalf("start request bitrate: got %d want %d", request.BitrateKbps, bitrateKbps)
	}

	activeJSON, err := json.Marshal(ActiveStream{BitrateKbps: bitrateKbps})
	if err != nil {
		t.Fatalf("marshal active stream: %v", err)
	}
	var active ActiveStream
	if err := json.Unmarshal(activeJSON, &active); err != nil {
		t.Fatalf("unmarshal active stream: %v", err)
	}
	if active.BitrateKbps != bitrateKbps {
		t.Fatalf("active stream bitrate: got %d want %d", active.BitrateKbps, bitrateKbps)
	}

	metaJSON, err := json.Marshal(StreamMeta{BitrateKbps: bitrateKbps})
	if err != nil {
		t.Fatalf("marshal stream metadata: %v", err)
	}
	var meta StreamMeta
	if err := json.Unmarshal(metaJSON, &meta); err != nil {
		t.Fatalf("unmarshal stream metadata: %v", err)
	}
	if meta.BitrateKbps != bitrateKbps {
		t.Fatalf("stream metadata bitrate: got %d want %d", meta.BitrateKbps, bitrateKbps)
	}
}
