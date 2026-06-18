package main

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// SFU membership reconciliation (authority model: Option A).
//
// Nakama stays authoritative for the UI voice roster, but the in-memory rooms
// can drift from reality (ghost members after a missed disconnect, which the
// presence-based GC only catches after a 2h staleness window). To correct that,
// Nakama periodically PULLS live membership from the SFU that owns each voice
// session and prunes members the SFU has no record of.
//
// This is deliberately one-directional and optional:
//   - The SFU never needs to know Nakama exists (no webhook/coupling).
//   - Works with any number of SFUs: each session lives on exactly one SFU, so
//     we just ask each configured SFU until one recognizes the session.
//   - Disabled entirely unless SFU_ADMIN_PASSWORD and at least one
//     SFU_ADMIN_BASE_* are set, so self-hosters and P2P-only deployments are
//     unaffected.

// sfuAdminBases maps region -> SFU admin HTTP base URL (no trailing slash).
var sfuAdminBases = map[string]string{}
var (
	voiceReconcileMissesMu sync.Mutex
	voiceReconcileMisses   = map[string]int{} // channelID|userID -> consecutive SFU misses
)

func init() {
	if eu := os.Getenv("SFU_ADMIN_BASE_EU"); eu != "" {
		sfuAdminBases["eu-west"] = strings.TrimRight(eu, "/")
	}
	if us := os.Getenv("SFU_ADMIN_BASE_US"); us != "" {
		sfuAdminBases["us-east"] = strings.TrimRight(us, "/")
	}
}

func sfuReconcileEnabled() bool {
	return os.Getenv("SFU_ADMIN_PASSWORD") != "" && len(sfuAdminBases) > 0
}

// A Nakama member younger than this is never pruned, so we don't remove a user
// whose SFU connection is still being established.
const voiceReconcileGrace = 45 * time.Second
const voiceReconcileRequiredMisses = 2

var sfuAdminHTTP = &http.Client{Timeout: 5 * time.Second}

type sfuSessionDetail struct {
	Peers []struct {
		UserID string `json:"user_id"`
	} `json:"peers"`
}

// querySFUSession asks one SFU for the live member set of a session. Returns
// (members, true) only on a 200; any other status (notably 404 = session
// unknown to this SFU) or transport error returns (nil, false).
func querySFUSession(base, password, sessionID string) (map[string]bool, bool) {
	url := fmt.Sprintf("%s/admin/api/session/%s", base, sessionID)
	req, err := http.NewRequest(http.MethodGet, url, nil)
	if err != nil {
		return nil, false
	}
	req.SetBasicAuth("nakama", password)
	resp, err := sfuAdminHTTP.Do(req)
	if err != nil {
		return nil, false
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, false
	}
	var detail sfuSessionDetail
	if err := json.NewDecoder(resp.Body).Decode(&detail); err != nil {
		return nil, false
	}
	members := make(map[string]bool, len(detail.Peers))
	for _, p := range detail.Peers {
		if p.UserID != "" {
			members[p.UserID] = true
		}
	}
	return members, true
}

// fetchSFUSessionMembers finds the SFU that owns sessionID and returns its live
// member set. ok is false when no configured SFU recognizes the session (so the
// caller must NOT prune -- the room may be P2P or the lookup transiently failed).
func fetchSFUSessionMembers(sessionID string) (members map[string]bool, ok bool) {
	password := os.Getenv("SFU_ADMIN_PASSWORD")
	for _, base := range sfuAdminBases {
		if m, found := querySFUSession(base, password, sessionID); found {
			return m, true
		}
	}
	return nil, false
}

// StartVoiceReconcile runs the reconciliation loop until ctx is cancelled.
func StartVoiceReconcile(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, interval time.Duration) {
	logger.Info("Voice SFU reconcile started (interval=%s, bases=%d)", interval, len(sfuAdminBases))
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			reconcileVoiceRooms(ctx, logger, nk)
		}
	}
}

// reconcileVoiceRooms prunes Nakama voice members that the owning SFU has no
// record of (ghosts). It never adds members: the joining client's own
// voice_join and the client-side resync-on-reconnect cover missing members.
func reconcileVoiceRooms(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule) {
	// Snapshot rooms so we don't hold the lock across network calls.
	type roomInfo struct {
		channelID string
		crewID    string
		members   map[string]int64 // userID -> JoinedAt (ms)
	}
	var rooms []roomInfo
	voiceRoomsMu.RLock()
	for chID, room := range voiceRooms {
		m := make(map[string]int64, len(room.Members))
		for uid, st := range room.Members {
			m[uid] = st.JoinedAt
		}
		rooms = append(rooms, roomInfo{channelID: chID, crewID: room.CrewID, members: m})
	}
	voiceRoomsMu.RUnlock()

	nowMs := time.Now().UnixMilli()
	graceMs := voiceReconcileGrace.Milliseconds()
	activeKeys := make(map[string]struct{})

	for _, r := range rooms {
		sessionID := fmt.Sprintf("voice:%s:%s", r.crewID, r.channelID)
		sfuMembers, ok := fetchSFUSessionMembers(sessionID)
		if !ok {
			// Unknown session (P2P or transient lookup failure): don't carry
			// stale miss counts into later successful polls.
			voiceReconcileMissesMu.Lock()
			for uid := range r.members {
				delete(voiceReconcileMisses, r.channelID+"|"+uid)
			}
			voiceReconcileMissesMu.Unlock()
			continue // session unknown to any SFU (P2P or transient) -> don't prune
		}
		for uid, joinedAt := range r.members {
			key := r.channelID + "|" + uid
			activeKeys[key] = struct{}{}
			if sfuMembers[uid] {
				voiceReconcileMissesMu.Lock()
				delete(voiceReconcileMisses, key)
				voiceReconcileMissesMu.Unlock()
				continue
			}
			if nowMs-joinedAt < graceMs {
				voiceReconcileMissesMu.Lock()
				delete(voiceReconcileMisses, key)
				voiceReconcileMissesMu.Unlock()
				continue // too new; may still be connecting
			}
			// Only prune if the user is still mapped to this channel (avoid
			// racing a concurrent leave/switch).
			voiceUserChannelMu.RLock()
			stillHere := voiceUserChannel[uid] == r.channelID
			voiceUserChannelMu.RUnlock()
			if !stillHere {
				voiceReconcileMissesMu.Lock()
				delete(voiceReconcileMisses, key)
				voiceReconcileMissesMu.Unlock()
				continue
			}

			voiceReconcileMissesMu.Lock()
			voiceReconcileMisses[key]++
			misses := voiceReconcileMisses[key]
			voiceReconcileMissesMu.Unlock()
			if misses < voiceReconcileRequiredMisses {
				continue
			}

			logger.Info(
				"voice reconcile: pruning ghost user=%s channel=%s (absent from SFU session %s, misses=%d)",
				uid,
				r.channelID,
				sessionID,
				misses,
			)
			voiceLeaveInternal(ctx, logger, nk, uid)
			voiceReconcileMissesMu.Lock()
			delete(voiceReconcileMisses, key)
			voiceReconcileMissesMu.Unlock()
		}
	}

	voiceReconcileMissesMu.Lock()
	for key := range voiceReconcileMisses {
		if _, ok := activeKeys[key]; !ok {
			delete(voiceReconcileMisses, key)
		}
	}
	voiceReconcileMissesMu.Unlock()
}
