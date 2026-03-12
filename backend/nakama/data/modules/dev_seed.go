package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// DevSeedStateRPC populates transient dev state: presence, voice rooms,
// streams, and chat message previews.  Call after seed.sh creates users &
// crews.  Idempotent — safe to run repeatedly.
//
// Accepts no payload (uses hardcoded seed usernames / crew names).
func DevSeedStateRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {

	// ── resolve seed users ──────────────────────────────────────────
	type seedUser struct {
		id          string
		displayName string
	}
	seedUsernames := []string{"alice", "bob", "charlie", "diana"}
	users := make(map[string]*seedUser, len(seedUsernames))

	for _, uname := range seedUsernames {
		var id, display string
		err := db.QueryRowContext(ctx,
			"SELECT id, display_name FROM users WHERE username = $1", uname,
		).Scan(&id, &display)
		if err != nil {
			logger.Warn("dev_seed: user %s not found: %v", uname, err)
			continue
		}
		if display == "" {
			display = uname
		}
		users[uname] = &seedUser{id: id, displayName: display}
	}
	if len(users) < 4 {
		return "", runtime.NewError("not all seed users found — run seed.sh first", 9)
	}

	// ── resolve seed crews ──────────────────────────────────────────
	crewNames := []string{"Devs", "Gamers", "Music", "Design", "Ops", "Retro"}
	crewIDs := make(map[string]string, len(crewNames))

	for _, name := range crewNames {
		var id string
		err := db.QueryRowContext(ctx,
			"SELECT id FROM groups WHERE name = $1", name,
		).Scan(&id)
		if err != nil {
			logger.Warn("dev_seed: crew %s not found: %v", name, err)
			continue
		}
		crewIDs[name] = id
	}
	if len(crewIDs) < 4 {
		return "", runtime.NewError("not all seed crews found — run seed.sh first", 9)
	}

	now := time.Now().UTC().Format(time.RFC3339)

	// ── 1. presence ─────────────────────────────────────────────────
	presences := map[string]*UserPresence{
		"alice": {
			UserID: users["alice"].id, Status: StatusOnline,
			LastSeen: now, UpdatedAt: now,
			Activity: &Activity{Type: ActivityInVoice, CrewID: crewIDs["Gamers"]},
		},
		"bob": {
			UserID: users["bob"].id, Status: StatusOnline,
			LastSeen: now, UpdatedAt: now,
			Activity: &Activity{Type: ActivityInVoice, CrewID: crewIDs["Gamers"]},
		},
		"charlie": {
			UserID: users["charlie"].id, Status: StatusOnline,
			LastSeen: now, UpdatedAt: now,
			Activity: &Activity{
				Type:        ActivityStreaming,
				CrewID:      crewIDs["Devs"],
				StreamTitle: "Counter-Strike 2",
			},
		},
		"diana": {
			UserID: users["diana"].id, Status: StatusIdle,
			LastSeen: now, UpdatedAt: now,
			Activity: &Activity{Type: ActivityNone},
		},
	}
	for uname, p := range presences {
		if err := WritePresence(ctx, nk, p); err != nil {
			logger.Warn("dev_seed: presence write failed for %s: %v", uname, err)
		}
	}
	logger.Info("dev_seed: presence set for %d users", len(presences))

	// ── 2. voice rooms ──────────────────────────────────────────────
	// Gamers: alice + bob in voice, bob currently speaking
	if gid, ok := crewIDs["Gamers"]; ok {
		voiceRoomsMu.Lock()
		voiceRooms[gid] = &VoiceRoom{
			CrewID: gid,
			Members: map[string]*VoiceMemberState{
				users["alice"].id: {
					UserID: users["alice"].id, Username: users["alice"].displayName,
					Speaking: false,
				},
				users["bob"].id: {
					UserID: users["bob"].id, Username: users["bob"].displayName,
					Speaking: true,
				},
			},
		}
		voiceRoomsMu.Unlock()

		voiceUserCrewMu.Lock()
		voiceUserCrew[users["alice"].id] = gid
		voiceUserCrew[users["bob"].id] = gid
		voiceUserCrewMu.Unlock()
	}

	// Devs: charlie in voice (also streaming)
	if gid, ok := crewIDs["Devs"]; ok {
		voiceRoomsMu.Lock()
		voiceRooms[gid] = &VoiceRoom{
			CrewID: gid,
			Members: map[string]*VoiceMemberState{
				users["charlie"].id: {
					UserID: users["charlie"].id, Username: users["charlie"].displayName,
					Speaking: false,
				},
			},
		}
		voiceRoomsMu.Unlock()

		voiceUserCrewMu.Lock()
		voiceUserCrew[users["charlie"].id] = gid
		voiceUserCrewMu.Unlock()
	}
	logger.Info("dev_seed: voice rooms populated (Gamers: 2, Devs: 1)")

	// ── 3. stream in Devs (charlie → Counter-Strike 2) ──────────────
	if gid, ok := crewIDs["Devs"]; ok {
		streamID := fmt.Sprintf("stream_%s_seed", users["charlie"].id[:8])
		meta := StreamMeta{
			StreamID:        streamID,
			CrewID:          gid,
			StreamerID:      users["charlie"].id,
			StreamerUsername: users["charlie"].displayName,
			Title:           "Counter-Strike 2",
			StartedAt:       now,
		}
		metaJSON, _ := json.Marshal(meta)
		nk.StorageWrite(ctx, []*runtime.StorageWrite{{
			Collection:      StreamMetaCollection,
			Key:             gid,
			UserID:          SystemUserID,
			Value:           string(metaJSON),
			PermissionRead:  2,
			PermissionWrite: 0,
		}})

		stream := ActiveStream{
			HostID:    users["charlie"].id,
			HostName:  users["charlie"].displayName,
			Title:     "Counter-Strike 2",
			StartedAt: time.Now().Unix(),
		}
		sJSON, _ := json.Marshal(stream)
		nk.StorageWrite(ctx, []*runtime.StorageWrite{{
			Collection:      StreamCollection,
			Key:             gid,
			UserID:          users["charlie"].id,
			Value:           string(sJSON),
			PermissionRead:  2,
			PermissionWrite: 0,
		}})
		logger.Info("dev_seed: stream started in Devs by %s", users["charlie"].displayName)
	}

	// ── 4. chat message previews ────────────────────────────────────
	previews := map[string][]*MessagePreview{
		"Gamers": {
			{Username: users["bob"].displayName, Preview: "anyone down for ranked?", Timestamp: now},
			{Username: users["alice"].displayName, Preview: "let's go, warming up rn", Timestamp: now},
		},
		"Devs": {
			{Username: users["charlie"].displayName, Preview: "streaming some CS2, come watch", Timestamp: now},
			{Username: users["alice"].displayName, Preview: "nice, joining voice", Timestamp: now},
		},
		"Music": {
			{Username: users["diana"].displayName, Preview: "new beat dropping tomorrow", Timestamp: now},
		},
		"Design": {
			{Username: users["alice"].displayName, Preview: "updated the mockups, check figma", Timestamp: now},
			{Username: users["diana"].displayName, Preview: "looks fire", Timestamp: now},
		},
		"Retro": {
			{Username: users["bob"].displayName, Preview: "got the CRT calibrated finally", Timestamp: now},
		},
	}

	crewRecentMsgsMu.Lock()
	for crewName, msgs := range previews {
		if cid, ok := crewIDs[crewName]; ok {
			crewRecentMsgs[cid] = msgs
		}
	}
	crewRecentMsgsMu.Unlock()
	logger.Info("dev_seed: chat previews injected for %d crews", len(previews))

	// ── 5. invalidate caches ────────────────────────────────────────
	for _, cid := range crewIDs {
		InvalidateCrewState(cid)
	}

	resp, _ := json.Marshal(map[string]interface{}{
		"success":     true,
		"users":       len(users),
		"crews":       len(crewIDs),
		"voice_rooms": 2,
		"streams":     1,
	})
	return string(resp), nil
}
