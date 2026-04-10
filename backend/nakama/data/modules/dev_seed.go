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
	// NOTE: ChannelID/ChannelName on voice activities are set after
	// channels are created in step 2.  We write presence twice for
	// voice users: once here (basic), then patched in step 3.
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
			UserID: users["diana"].id, Status: StatusOnline,
			LastSeen: now, UpdatedAt: now,
			Activity: &Activity{Type: ActivityInVoice, CrewID: crewIDs["Gamers"]},
		},
	}
	for uname, p := range presences {
		if err := WritePresence(ctx, nk, p); err != nil {
			logger.Warn("dev_seed: presence write failed for %s: %v", uname, err)
		}
	}
	logger.Info("dev_seed: presence set for %d users", len(presences))

	// ── 2. voice channels per crew ─────────────────────────────────
	// Rich channel layouts for Gamers + Devs; default-only for the rest.
	type channelSeed struct {
		Name      string
		IsDefault bool
	}
	crewChannelSeeds := map[string][]channelSeed{
		"Gamers": {
			{Name: "General", IsDefault: true},
			{Name: "Strategy", IsDefault: false},
			{Name: "AFK", IsDefault: false},
		},
		"Devs": {
			{Name: "General", IsDefault: true},
			{Name: "Code Review", IsDefault: false},
		},
	}

	// channelIDs[crewName][channelName] = generated ID
	channelIDs := make(map[string]map[string]string)

	for crewName, gid := range crewIDs {
		seeds, hasCustom := crewChannelSeeds[crewName]
		if !hasCustom {
			// Just ensure a default General channel
			if err := InitDefaultChannel(ctx, nk, gid); err != nil {
				logger.Warn("dev_seed: default channel for %s: %v", crewName, err)
			}
			continue
		}

		defs := make([]*VoiceChannelDef, len(seeds))
		nameMap := make(map[string]string, len(seeds))
		for i, s := range seeds {
			id := generateChannelID()
			defs[i] = &VoiceChannelDef{
				ID:        id,
				Name:      s.Name,
				IsDefault: s.IsDefault,
				SortOrder: i,
			}
			nameMap[s.Name] = id
		}
		list := &VoiceChannelList{Channels: defs}
		if err := saveVoiceChannels(ctx, nk, gid, list); err != nil {
			logger.Warn("dev_seed: save channels for %s: %v", crewName, err)
		}
		channelIDs[crewName] = nameMap
	}
	logger.Info("dev_seed: voice channels created (Gamers: 3, Devs: 2, others: default)")

	// ── 3. voice rooms ──────────────────────────────────────────────
	// Helper to populate a voice room + reverse maps
	seedVoiceRoom := func(crewName, channelName string, memberPairs []struct {
		user     string
		speaking bool
	}) {
		gid, ok := crewIDs[crewName]
		if !ok {
			return
		}
		chMap, ok := channelIDs[crewName]
		if !ok {
			return
		}
		chID, ok := chMap[channelName]
		if !ok {
			return
		}

		members := make(map[string]*VoiceMemberState, len(memberPairs))
		for _, mp := range memberPairs {
			u := users[mp.user]
			if u == nil {
				continue
			}
			members[u.id] = &VoiceMemberState{
				UserID:   u.id,
				Username: u.displayName,
				Speaking: mp.speaking,
			}
		}

		voiceRoomsMu.Lock()
		voiceRooms[chID] = &VoiceRoom{
			ChannelID: chID,
			CrewID:    gid,
			Members:   members,
		}
		voiceRoomsMu.Unlock()

		voiceUserChannelMu.Lock()
		for _, mp := range memberPairs {
			if u := users[mp.user]; u != nil {
				voiceUserChannel[u.id] = chID
			}
		}
		voiceUserChannelMu.Unlock()

		voiceChannelCrewMu.Lock()
		voiceChannelCrew[chID] = gid
		voiceChannelCrewMu.Unlock()
	}

	// Gamers → General: alice + bob (bob speaking)
	seedVoiceRoom("Gamers", "General", []struct {
		user     string
		speaking bool
	}{
		{user: "alice", speaking: false},
		{user: "bob", speaking: true},
	})

	// Gamers → Strategy: diana hanging out (idle)
	seedVoiceRoom("Gamers", "Strategy", []struct {
		user     string
		speaking bool
	}{
		{user: "diana", speaking: false},
	})
	// (AFK channel left empty on purpose)

	// Devs → General: charlie in voice (also streaming)
	seedVoiceRoom("Devs", "General", []struct {
		user     string
		speaking bool
	}{
		{user: "charlie", speaking: false},
	})
	// (Code Review channel left empty on purpose)

	logger.Info("dev_seed: voice rooms populated (Gamers General: 2, Gamers Strategy: 1, Devs General: 1)")

	// Patch presence with channel IDs now that channels exist
	voicePresence := []struct {
		user        string
		crewName    string
		channelName string
	}{
		{"alice", "Gamers", "General"},
		{"bob", "Gamers", "General"},
		{"diana", "Gamers", "Strategy"},
		{"charlie", "Devs", "General"},
	}
	for _, vp := range voicePresence {
		u := users[vp.user]
		if u == nil {
			continue
		}
		gid := crewIDs[vp.crewName]
		chMap := channelIDs[vp.crewName]
		if chMap == nil {
			continue
		}
		chID := chMap[vp.channelName]

		activity := &Activity{
			Type:        ActivityInVoice,
			CrewID:      gid,
			ChannelID:   chID,
			ChannelName: vp.channelName,
		}
		// charlie is also streaming
		if vp.user == "charlie" {
			activity.Type = ActivityStreaming
			activity.StreamTitle = "Counter-Strike 2"
		}
		_ = WritePresence(ctx, nk, &UserPresence{
			UserID: u.id, Status: StatusOnline,
			LastSeen: now, UpdatedAt: now,
			Activity: activity,
		})
	}
	logger.Info("dev_seed: presence patched with channel IDs")

	// ── 4. stream in Devs (charlie → Counter-Strike 2) ──────────────
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

	// ── 5. chat message previews ────────────────────────────────────
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

	// ── 6. invalidate caches ────────────────────────────────────────
	for _, cid := range crewIDs {
		InvalidateCrewState(cid)
	}

	// ── 7. crew event ledger + stale last_seen ─────────────────────
	// Populate the event ledger for Gamers and Devs with a rich set of
	// events covering every card type the crew feed can display:
	// clips, sessions, recaps, moments, game sessions, and member joins.
	nowMs := time.Now().UnixMilli()
	staleLastSeen := nowMs - 24*60*60*1000 // 24 hours ago

	hour := int64(60 * 60 * 1000)
	min := int64(60 * 1000)

	weekStart := time.Now().Add(-7 * 24 * time.Hour).UnixMilli()

	seedEvents := map[string][]CrewEvent{
		"Gamers": {
			// ── CLIPS ─────────────────────────────────────────
			// Hero clip: newest, bob clipped a clutch (30min ago)
			{
				ID: generateEventID(), CrewID: crewIDs["Gamers"],
				Type: "clip", ActorID: users["bob"].id,
				Timestamp: nowMs - 30*min, Score: 50,
				Data: ClipData{
					ClipID:           "clip_seed_hero",
					ClipType:         "voice",
					ClipperName:      users["bob"].displayName,
					DurationSeconds:  28.5,
					Participants:     []string{users["bob"].id, users["alice"].id, users["diana"].id},
					ParticipantNames: []string{users["bob"].displayName, users["alice"].displayName, users["diana"].displayName},
					Game:             "Counter-Strike 2",
					MediaURL:         fmt.Sprintf("http://localhost:9000/mello-clips/crews/%s/clip_seed_hero.mp4", crewIDs["Gamers"]),
				},
			},
			// Standard clip: alice caught a funny moment (2h ago)
			{
				ID: generateEventID(), CrewID: crewIDs["Gamers"],
				Type: "clip", ActorID: users["alice"].id,
				Timestamp: nowMs - 2*hour, Score: 50,
				Data: ClipData{
					ClipID:           "clip_seed_alice_1",
					ClipType:         "voice",
					ClipperName:      users["alice"].displayName,
					DurationSeconds:  15.0,
					Participants:     []string{users["alice"].id, users["bob"].id},
					ParticipantNames: []string{users["alice"].displayName, users["bob"].displayName},
					MediaURL:         fmt.Sprintf("http://localhost:9000/mello-clips/crews/%s/clip_seed_alice_1.mp4", crewIDs["Gamers"]),
				},
			},
			// Older clip: diana in a game session (6h ago)
			{
				ID: generateEventID(), CrewID: crewIDs["Gamers"],
				Type: "clip", ActorID: users["diana"].id,
				Timestamp: nowMs - 6*hour, Score: 50,
				Data: ClipData{
					ClipID:           "clip_seed_diana_1",
					ClipType:         "voice",
					ClipperName:      users["diana"].displayName,
					DurationSeconds:  30.0,
					Participants:     []string{users["diana"].id},
					ParticipantNames: []string{users["diana"].displayName},
					Game:             "Valorant",
					MediaURL:         fmt.Sprintf("http://localhost:9000/mello-clips/crews/%s/clip_seed_diana_1.mp4", crewIDs["Gamers"]),
				},
			},

			// ── SESSIONS ──────────────────────────────────────
			// Voice session (1h ago) — alice, bob, diana hung out
			{
				ID: generateEventID(), CrewID: crewIDs["Gamers"],
				Type: "voice_session", ActorID: "",
				Timestamp: nowMs - 1*hour, Score: 20,
				Data: VoiceSessionData{
					ChannelName:      "General",
					ParticipantIDs:   []string{users["alice"].id, users["bob"].id, users["diana"].id},
					ParticipantNames: []string{users["alice"].displayName, users["bob"].displayName, users["diana"].displayName},
					DurationMin:      93, PeakCount: 3,
				},
			},
			// Game session (4h ago) — bob + diana played Valorant
			{
				ID: generateEventID(), CrewID: crewIDs["Gamers"],
				Type: "game_session", ActorID: users["bob"].id,
				Timestamp: nowMs - 4*hour, Score: 15,
				Data: GameSessionData{
					GameName:    "Valorant",
					PlayerIDs:   []string{users["bob"].id, users["diana"].id},
					PlayerNames: []string{users["bob"].displayName, users["diana"].displayName},
					DurationMin: 65,
				},
			},
			// Older voice session (8h ago) — bob + diana
			{
				ID: generateEventID(), CrewID: crewIDs["Gamers"],
				Type: "voice_session", ActorID: "",
				Timestamp: nowMs - 8*hour, Score: 20,
				Data: VoiceSessionData{
					ChannelName:      "General",
					ParticipantIDs:   []string{users["bob"].id, users["diana"].id},
					ParticipantNames: []string{users["bob"].displayName, users["diana"].displayName},
					DurationMin:      47, PeakCount: 2,
				},
			},

			// ── MOMENTS (show as catchup cards) ───────────────
			{
				ID: generateEventID(), CrewID: crewIDs["Gamers"],
				Type: "moment", ActorID: users["bob"].id,
				Timestamp: nowMs - 3*hour, Score: 40,
				Data: MomentData{
					Text: "40-bomb on Dust2 lets goooo", Sentiment: "highlight",
					GameName: "Counter-Strike 2",
				},
			},
			{
				ID: generateEventID(), CrewID: crewIDs["Gamers"],
				Type: "moment", ActorID: users["diana"].id,
				Timestamp: nowMs - 5*hour, Score: 35,
				Data: MomentData{
					Text: "first ace ever in ranked", Sentiment: "highlight",
					GameName: "Valorant",
				},
			},

			// ── WEEKLY RECAP ──────────────────────────────────
			{
				ID: generateEventID(), CrewID: crewIDs["Gamers"],
				Type: "weekly_recap", ActorID: "",
				Timestamp: nowMs - 12*hour, Score: 30,
			Data: WeeklyRecapData{
				CrewID:            crewIDs["Gamers"],
				WeekStart:         weekStart,
				WeekEnd:           nowMs,
				TotalHangoutMin:   420,
				TopGame:           "Counter-Strike 2",
				LongestSession:    "bob, diana in General (93m)",
				LongestSessionMin: 93,
				ClipCount:         7,
				MostActive:        users["bob"].displayName,
				MostClipped:       users["alice"].displayName,
				TopMembers: []RecapMember{
					{DisplayName: users["bob"].displayName, HangoutMin: 185},
					{DisplayName: users["alice"].displayName, HangoutMin: 142},
					{DisplayName: users["diana"].displayName, HangoutMin: 93},
				},
				GeneratedAt: nowMs - 12*hour,
			},
			},
		},
		"Devs": {
			// ── CLIPS ─────────────────────────────────────────
			// charlie clipped a stream highlight (1h ago)
			{
				ID: generateEventID(), CrewID: crewIDs["Devs"],
				Type: "clip", ActorID: users["charlie"].id,
				Timestamp: nowMs - 1*hour, Score: 50,
				Data: ClipData{
					ClipID:           "clip_seed_charlie_1",
					ClipType:         "voice",
					ClipperName:      users["charlie"].displayName,
					DurationSeconds:  22.0,
					Participants:     []string{users["charlie"].id, users["alice"].id},
					ParticipantNames: []string{users["charlie"].displayName, users["alice"].displayName},
					Game:             "Counter-Strike 2",
					MediaURL:         fmt.Sprintf("http://localhost:9000/mello-clips/crews/%s/clip_seed_charlie_1.mp4", crewIDs["Devs"]),
				},
			},
			// alice clipped a code review discussion (4h ago)
			{
				ID: generateEventID(), CrewID: crewIDs["Devs"],
				Type: "clip", ActorID: users["alice"].id,
				Timestamp: nowMs - 4*hour, Score: 50,
				Data: ClipData{
					ClipID:           "clip_seed_alice_dev",
					ClipType:         "voice",
					ClipperName:      users["alice"].displayName,
					DurationSeconds:  18.5,
					Participants:     []string{users["alice"].id, users["bob"].id, users["charlie"].id},
					ParticipantNames: []string{users["alice"].displayName, users["bob"].displayName, users["charlie"].displayName},
					MediaURL:         fmt.Sprintf("http://localhost:9000/mello-clips/crews/%s/clip_seed_alice_dev.mp4", crewIDs["Devs"]),
				},
			},

			// ── SESSIONS ──────────────────────────────────────
			// Stream session (5h ago) — charlie streamed CS2
			{
				ID: generateEventID(), CrewID: crewIDs["Devs"],
				Type: "stream_session", ActorID: users["charlie"].id,
				Timestamp: nowMs - 5*hour, Score: 30,
				Data: StreamSessionData{
					StreamerID: users["charlie"].id, StreamerName: users["charlie"].displayName,
					Title: "Counter-Strike 2", DurationMin: 120, PeakViewers: 3,
					ViewerIDs: []string{users["alice"].id, users["bob"].id},
				},
			},
			// Voice session (3h ago) — alice + bob code review
			{
				ID: generateEventID(), CrewID: crewIDs["Devs"],
				Type: "voice_session", ActorID: "",
				Timestamp: nowMs - 3*hour, Score: 20,
				Data: VoiceSessionData{
					ChannelName:      "Code Review",
					ParticipantIDs:   []string{users["alice"].id, users["bob"].id},
					ParticipantNames: []string{users["alice"].displayName, users["bob"].displayName},
					DurationMin:      35, PeakCount: 2,
				},
			},

			// ── MEMBER JOINED ─────────────────────────────────
			{
				ID: generateEventID(), CrewID: crewIDs["Devs"],
				Type: "member_joined", ActorID: users["diana"].id,
				Timestamp: nowMs - 10*hour, Score: 15,
				Data: MemberJoinedData{
					Username: users["diana"].displayName, DisplayName: users["diana"].displayName,
				},
			},
		},
		// Music crew gets a couple clips so the sidebar shows FOMO badges
		"Music": {
			{
				ID: generateEventID(), CrewID: crewIDs["Music"],
				Type: "clip", ActorID: users["charlie"].id,
				Timestamp: nowMs - 2*hour, Score: 50,
				Data: ClipData{
					ClipID:           "clip_seed_music_1",
					ClipType:         "voice",
					ClipperName:      users["charlie"].displayName,
					DurationSeconds:  25.0,
					Participants:     []string{users["charlie"].id, users["diana"].id},
					ParticipantNames: []string{users["charlie"].displayName, users["diana"].displayName},
					MediaURL:         fmt.Sprintf("http://localhost:9000/mello-clips/crews/%s/clip_seed_music_1.mp4", crewIDs["Music"]),
				},
			},
			{
				ID: generateEventID(), CrewID: crewIDs["Music"],
				Type: "clip", ActorID: users["diana"].id,
				Timestamp: nowMs - 7*hour, Score: 50,
				Data: ClipData{
					ClipID:           "clip_seed_music_2",
					ClipType:         "voice",
					ClipperName:      users["diana"].displayName,
					DurationSeconds:  12.0,
					Participants:     []string{users["diana"].id},
					ParticipantNames: []string{users["diana"].displayName},
					MediaURL:         fmt.Sprintf("http://localhost:9000/mello-clips/crews/%s/clip_seed_music_2.mp4", crewIDs["Music"]),
				},
			},
		},
	}

	eventsWritten := 0
	clipCount := make(map[string]int)
	for crewName, events := range seedEvents {
		cid, ok := crewIDs[crewName]
		if !ok {
			continue
		}
		for _, ev := range events {
			if err := AppendCrewEvent(ctx, nk, cid, ev); err != nil {
				logger.Warn("dev_seed: append event failed for %s: %v", crewName, err)
			} else {
				eventsWritten++
				if ev.Type == "clip" {
					clipCount[crewName]++
				}
			}
		}
	}
	logger.Info("dev_seed: %d crew events written (clips: Gamers=%d, Devs=%d, Music=%d)",
		eventsWritten, clipCount["Gamers"], clipCount["Devs"], clipCount["Music"])

	// Set stale last_seen for all users in seeded crews so catch-up triggers
	lastSeenCrews := []string{"Gamers", "Devs", "Music"}
	for _, crewName := range lastSeenCrews {
		cid, ok := crewIDs[crewName]
		if !ok {
			continue
		}
		for _, u := range users {
			ls := crewLastSeen{CrewID: cid, LastSeen: staleLastSeen}
			data, _ := json.Marshal(ls)
			nk.StorageWrite(ctx, []*runtime.StorageWrite{{
				Collection:      CrewLastSeenCollection,
				Key:             cid,
				UserID:          u.id,
				Value:           string(data),
				PermissionRead:  1,
				PermissionWrite: 1,
			}})
		}
	}
	logger.Info("dev_seed: stale last_seen set for %d users in %d crews", len(users), len(lastSeenCrews))

	resp, _ := json.Marshal(map[string]interface{}{
		"success":        true,
		"users":          len(users),
		"crews":          len(crewIDs),
		"voice_rooms":    3,
		"voice_channels": 5 + (len(crewIDs) - 2),
		"streams":        1,
		"crew_events":    eventsWritten,
		"clips_seeded":   clipCount["Gamers"] + clipCount["Devs"] + clipCount["Music"],
	})
	return string(resp), nil
}
