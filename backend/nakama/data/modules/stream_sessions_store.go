package main

import (
	"context"
	"crypto/rand"
	"encoding/json"
	"fmt"
	"math/big"
	"sort"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// ---------------------------------------------------------------------------
// Durable stream-session store — separate per-crew document, outside the
// ledger trim. Stream replays (snapshot session-previews) are first-class
// memories and must survive the 7-day ledger window like clips and recaps.
// The ledger still owns the recent window (this_week); this store owns the
// long-term spine (memory).
// ---------------------------------------------------------------------------

const (
	// CrewStreamSessionsCollection is distinct from StreamSessionCol
	// ("stream_sessions"), which holds live, in-flight stream metadata.
	CrewStreamSessionsCollection = "crew_stream_sessions"

	// CrewStreamSessionsMaxRetained caps the per-crew document. Sessions live
	// in a single Nakama storage object hard-limited to 256KB. A
	// StoredStreamSession with up to 6 snapshot URLs serializes to roughly
	// 700-900 bytes, so 150 entries keeps a safe margin. Older sessions are
	// trimmed; unbounded history is future m3llo+ work.
	CrewStreamSessionsMaxRetained = 150
)

// StoredStreamSession is the durable, display-only projection of a stream
// session. ViewerIDs are intentionally dropped (not needed to render a replay
// card and unbounded in size); PeakViewers is kept.
type StoredStreamSession struct {
	EventID      string   `json:"event_id"` // ledger event ID, for de-dup vs this_week
	SessionID    string   `json:"session_id"`
	StreamerID   string   `json:"streamer_id"`
	StreamerName string   `json:"streamer_name"`
	Title        string   `json:"title"`
	Game         string   `json:"game,omitempty"`
	DurationMin  int      `json:"duration_min"`
	PeakViewers  int      `json:"peak_viewers"`
	SnapshotURLs []string `json:"snapshot_urls"`
	Ts           int64    `json:"ts"`
	Score        int      `json:"score"`
}

type CrewStreamSessionsDoc struct {
	CrewID    string                `json:"crew_id"`
	Sessions  []StoredStreamSession `json:"sessions"` // newest appended last
	UpdatedAt int64                 `json:"updated_at"`
}

func readStreamSessionsDoc(ctx context.Context, nk runtime.NakamaModule, crewID string) (*CrewStreamSessionsDoc, string) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: CrewStreamSessionsCollection, Key: crewID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return &CrewStreamSessionsDoc{CrewID: crewID, Sessions: []StoredStreamSession{}}, ""
	}

	var doc CrewStreamSessionsDoc
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &doc); err != nil {
		return &CrewStreamSessionsDoc{CrewID: crewID, Sessions: []StoredStreamSession{}}, ""
	}
	return &doc, objects[0].GetVersion()
}

func writeStreamSessionsDoc(ctx context.Context, nk runtime.NakamaModule, crewID string, doc *CrewStreamSessionsDoc, version string) error {
	data, err := json.Marshal(doc)
	if err != nil {
		return err
	}
	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      CrewStreamSessionsCollection,
			Key:             crewID,
			UserID:          SystemUserID,
			Value:           string(data),
			Version:         version,
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	})
	return err
}

// capStreamSessions trims to the most-recent CrewStreamSessionsMaxRetained by
// timestamp. Pure (no I/O) so the cap behavior is unit-testable.
func capStreamSessions(sessions []StoredStreamSession) []StoredStreamSession {
	if len(sessions) <= CrewStreamSessionsMaxRetained {
		return sessions
	}
	sort.Slice(sessions, func(i, j int) bool { return sessions[i].Ts < sessions[j].Ts })
	return sessions[len(sessions)-CrewStreamSessionsMaxRetained:]
}

// upsertStreamSession inserts s, or updates the existing entry with the same
// SessionID. Snapshot URLs only ever grow (the SFU backfills frames after the
// stream ends), so an update never drops a richer set. Pure for testability.
func upsertStreamSession(sessions []StoredStreamSession, s StoredStreamSession) []StoredStreamSession {
	for i := range sessions {
		if sessions[i].SessionID == s.SessionID {
			if len(s.SnapshotURLs) < len(sessions[i].SnapshotURLs) {
				s.SnapshotURLs = sessions[i].SnapshotURLs
			}
			sessions[i] = s
			return sessions
		}
	}
	return append(sessions, s)
}

// UpsertStreamSession persists (or refreshes) a stream session in the durable
// store, trims to the cap, and retries on version conflict. Called when a
// stream ends and again when snapshots backfill, so the durable copy stays in
// sync with the ledger's snapshot lifecycle.
func UpsertStreamSession(ctx context.Context, nk runtime.NakamaModule, crewID string, s StoredStreamSession) error {
	for attempt := 0; attempt < 3; attempt++ {
		doc, version := readStreamSessionsDoc(ctx, nk, crewID)
		doc.Sessions = capStreamSessions(upsertStreamSession(doc.Sessions, s))
		doc.UpdatedAt = time.Now().UnixMilli()
		if err := writeStreamSessionsDoc(ctx, nk, crewID, doc, version); err == nil {
			return nil
		}
		jitter, _ := rand.Int(rand.Reader, big.NewInt(50))
		time.Sleep(time.Duration(50*(attempt+1)+int(jitter.Int64())) * time.Millisecond)
	}
	return fmt.Errorf("crew_stream_sessions write failed after 3 retries for crew %s", crewID)
}

// storedStreamSessionFrom builds a durable projection from a ledger event ID
// and its decoded data.
func storedStreamSessionFrom(eventID string, ts int64, score int, d StreamSessionData) StoredStreamSession {
	return StoredStreamSession{
		EventID:      eventID,
		SessionID:    d.SessionID,
		StreamerID:   d.StreamerID,
		StreamerName: d.StreamerName,
		Title:        d.Title,
		Game:         d.Game,
		DurationMin:  d.DurationMin,
		PeakViewers:  d.PeakViewers,
		SnapshotURLs: d.SnapshotURLs,
		Ts:           ts,
		Score:        score,
	}
}
