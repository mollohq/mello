package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"os"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// DevFaultRPC injects voice-state drift for robustness testing: ghost members,
// forced leaves, channel clears, and dropped pushes. It lets the reconcile
// oracle / GC / client-resync paths be exercised deterministically.
//
// Gated behind MELLO_ENABLE_DEV_FAULT=1 because it mutates live voice state and
// can evict real users. Never enable in production.
//
// Payload: {"action": "...", "channel_id": "...", "crew_id": "...",
//           "user_id": "...", "username": "...", "count": N}
//
// Actions:
//   - ghost_member:   inject a member Nakama believes is present but the SFU
//                     isn't (JoinedAt backdated past the reconcile grace so it
//                     is immediately prunable). Requires channel_id, crew_id, user_id.
//   - force_leave:    force a voice leave for user_id (simulates a missed leave).
//   - clear_channel:  evict every member of channel_id.
//   - drop_next_push: drop the next `count` voice_update pushes for crew_id.
func DevFaultRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	if os.Getenv("MELLO_ENABLE_DEV_FAULT") != "1" {
		return "", runtime.NewError("dev_fault disabled (set MELLO_ENABLE_DEV_FAULT=1)", 9)
	}

	var req struct {
		Action    string `json:"action"`
		ChannelID string `json:"channel_id"`
		CrewID    string `json:"crew_id"`
		UserID    string `json:"user_id"`
		Username  string `json:"username"`
		Count     int    `json:"count"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid payload", 3)
	}

	switch req.Action {
	case "ghost_member":
		if req.ChannelID == "" || req.CrewID == "" || req.UserID == "" {
			return "", runtime.NewError("ghost_member requires channel_id, crew_id, user_id", 3)
		}
		username := req.Username
		if username == "" {
			username = "ghost-" + req.UserID
		}
		// Backdate JoinedAt past the reconcile grace so the oracle prunes it on
		// the next tick rather than treating it as still-connecting.
		backdated := time.Now().Add(-2 * voiceReconcileGrace).UnixMilli()

		voiceRoomsMu.Lock()
		room, ok := voiceRooms[req.ChannelID]
		if !ok {
			room = &VoiceRoom{
				ChannelID: req.ChannelID,
				CrewID:    req.CrewID,
				Members:   make(map[string]*VoiceMemberState),
			}
			voiceRooms[req.ChannelID] = room
		}
		room.Members[req.UserID] = &VoiceMemberState{
			UserID:   req.UserID,
			Username: username,
			JoinedAt: backdated,
		}
		voiceRoomsMu.Unlock()

		voiceUserChannelMu.Lock()
		voiceUserChannel[req.UserID] = req.ChannelID
		voiceUserChannelMu.Unlock()

		voiceChannelCrewMu.Lock()
		voiceChannelCrew[req.ChannelID] = req.CrewID
		voiceChannelCrewMu.Unlock()

		InvalidateCrewState(req.CrewID)
		PushVoiceUpdate(ctx, logger, nk, req.CrewID)
		logger.Warn("dev_fault: injected ghost member user=%s channel=%s crew=%s", req.UserID, req.ChannelID, req.CrewID)

	case "force_leave":
		if req.UserID == "" {
			return "", runtime.NewError("force_leave requires user_id", 3)
		}
		voiceLeaveInternal(ctx, logger, nk, req.UserID)
		logger.Warn("dev_fault: forced leave user=%s", req.UserID)

	case "clear_channel":
		if req.ChannelID == "" {
			return "", runtime.NewError("clear_channel requires channel_id", 3)
		}
		VoiceEvictChannel(ctx, logger, nk, req.ChannelID)
		logger.Warn("dev_fault: cleared channel=%s", req.ChannelID)

	case "drop_next_push":
		if req.CrewID == "" {
			return "", runtime.NewError("drop_next_push requires crew_id", 3)
		}
		n := req.Count
		if n <= 0 {
			n = 1
		}
		DropNextVoicePush(req.CrewID, n)
		logger.Warn("dev_fault: dropping next %d voice_update pushes for crew=%s", n, req.CrewID)

	default:
		return "", runtime.NewError("unknown action: "+req.Action, 3)
	}

	resp, _ := json.Marshal(map[string]interface{}{"success": true, "action": req.Action})
	return string(resp), nil
}
