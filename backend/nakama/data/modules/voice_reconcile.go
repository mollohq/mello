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

// sfuLookup is the outcome of asking the SFUs about one session.
type sfuLookup int

const (
	// sfuLookupFailed: a transport error or an unexpected status. Nothing is known.
	sfuLookupFailed sfuLookup = iota
	// sfuSessionAbsent: every SFU answered, and none has the session. An SFU
	// drops a session when its last peer leaves, so for an SFU-only participant
	// this is proof they are gone.
	sfuSessionAbsent
	// sfuSessionFound: one SFU has the session; the member set is valid.
	sfuSessionFound
)

// querySFUSession asks one SFU for the live member set of a session.
func querySFUSession(base, password, sessionID string) (map[string]bool, sfuLookup) {
	url := fmt.Sprintf("%s/admin/api/session/%s", base, sessionID)
	req, err := http.NewRequest(http.MethodGet, url, nil)
	if err != nil {
		return nil, sfuLookupFailed
	}
	req.SetBasicAuth("nakama", password)
	resp, err := sfuAdminHTTP.Do(req)
	if err != nil {
		return nil, sfuLookupFailed
	}
	defer resp.Body.Close()
	if resp.StatusCode == http.StatusNotFound {
		return nil, sfuSessionAbsent
	}
	if resp.StatusCode != http.StatusOK {
		return nil, sfuLookupFailed
	}
	var detail sfuSessionDetail
	if err := json.NewDecoder(resp.Body).Decode(&detail); err != nil {
		return nil, sfuLookupFailed
	}
	members := make(map[string]bool, len(detail.Peers))
	for _, p := range detail.Peers {
		if p.UserID != "" {
			members[p.UserID] = true
		}
	}
	return members, sfuSessionFound
}

// fetchSFUSessionMembers finds the SFU that owns sessionID and returns its live
// member set. The result is sfuSessionAbsent only when every configured SFU
// answered 404; one failed lookup makes it sfuLookupFailed.
func fetchSFUSessionMembers(sessionID string) (map[string]bool, sfuLookup) {
	return lookupSFUSession(sfuAdminBases, os.Getenv("SFU_ADMIN_PASSWORD"), sessionID)
}

func lookupSFUSession(bases map[string]string, password, sessionID string) (map[string]bool, sfuLookup) {
	result := sfuSessionAbsent
	for _, base := range bases {
		m, state := querySFUSession(base, password, sessionID)
		switch state {
		case sfuSessionFound:
			return m, sfuSessionFound
		case sfuLookupFailed:
			result = sfuLookupFailed
		}
	}
	if len(bases) == 0 {
		return nil, sfuLookupFailed
	}
	return nil, result
}

// absentFromSFU reports whether a seated user is missing from the SFU session.
// A session no SFU has is proof only for a guest: guests can only use the SFU,
// while a member's room may be P2P, which no SFU ever knows about.
func absentFromSFU(state sfuLookup, sfuMembers map[string]bool, uid string, isGuest bool) (absent, known bool) {
	switch state {
	case sfuSessionFound:
		return !sfuMembers[uid], true
	case sfuSessionAbsent:
		if isGuest {
			return true, true
		}
		return false, false
	default:
		return false, false
	}
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
			// Guests hold a time-boxed session and a closed browser tab never
			// sends a leave, so expire them before pruning SFU ghosts.
			ExpireGuestSessions(ctx, logger, nk)
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
		sfuMembers, state := fetchSFUSessionMembers(sessionID)
		if state == sfuLookupFailed {
			// Transient lookup failure: don't carry stale miss counts into
			// later successful polls.
			voiceReconcileMissesMu.Lock()
			for uid := range r.members {
				delete(voiceReconcileMisses, r.channelID+"|"+uid)
			}
			voiceReconcileMissesMu.Unlock()
			continue
		}
		for uid, joinedAt := range r.members {
			key := r.channelID + "|" + uid
			activeKeys[key] = struct{}{}
			// The last guest in a channel whose leave was lost (a closed tab)
			// leaves an SFU session that no longer exists. Counting that as a
			// miss is what lets the seat go.
			absent, known := absentFromSFU(state, sfuMembers, uid, IsGuestUser(uid))
			if !known || !absent {
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
