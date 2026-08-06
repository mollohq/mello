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

	if sfuAuthEnabled() {
		adminID := users["alice"].id
		for crewName, crewID := range crewIDs {
			enableSfuForCrew(ctx, nk, logger, adminID, crewID)
			logger.Info("dev_seed: sfu_enabled for crew %s", crewName)
		}
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

	// ── 6b. reset accumulating stores ───────────────────────────────
	// Everything above is keyed writes (overwrite in place), but the two
	// stores below *append*: AppendCrewEvent mints a fresh event ID per call
	// and UpdateUserGameStats adds to running totals. Without this reset a
	// second run doubles every feed card and inflates every W/L record —
	// which breaks the "idempotent" promise in this RPC's doc comment.
	resetDeletes := make([]*runtime.StorageDelete, 0, len(crewIDs)+len(users)*8)
	for _, cid := range crewIDs {
		resetDeletes = append(resetDeletes, &runtime.StorageDelete{
			Collection: CrewEventsCollection, Key: cid, UserID: SystemUserID,
		})
	}
	// Every game id this seed writes stats for (plus the caller's own set).
	seededGameIDs := []string{
		"counter-strike-2", "league-of-legends", "valorant", "dota-2",
		"rocket-league", "hearthstone", "starcraft-2", "custom-night-stones",
	}
	statsOwners := make([]string, 0, len(users)+1)
	for _, u := range users {
		statsOwners = append(statsOwners, u.id)
	}
	if callerID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string); ok && callerID != "" {
		statsOwners = append(statsOwners, callerID)
	}
	for _, ownerID := range statsOwners {
		for _, gameID := range seededGameIDs {
			resetDeletes = append(resetDeletes, &runtime.StorageDelete{
				Collection: UserGameStatsCollection, Key: gameID, UserID: ownerID,
			})
		}
	}
	if err := nk.StorageDelete(ctx, resetDeletes); err != nil {
		// Deleting a non-existent object is fine (first run); log and continue.
		logger.Debug("dev_seed: reset delete partial: %v", err)
	}
	logger.Info("dev_seed: reset %d ledgers + stats for %d users (idempotent re-seed)",
		len(crewIDs), len(statsOwners))

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
			// Game session with no telemetry outcomes (4h ago) — scores 0 in
			// gameSessionQuality, so it never earns a feed card (recap only).
			{
				ID: generateEventID(), CrewID: crewIDs["Gamers"],
				Type: "game_session", ActorID: users["bob"].id,
				Timestamp: nowMs - 4*hour, Score: 15,
				Data: GameSessionData{
					GameName:    "Valorant",
					GameID:      "valorant",
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

			// ── MEMBER JOINED (catch-up card) ─────────────────
			// Seeded explicitly: the equivalent hook events fire once when
			// seed.sh/.ps1 joins users, and the idempotent ledger reset
			// clears them, so re-runs would otherwise lose this card type.
			{
				ID: generateEventID(), CrewID: crewIDs["Gamers"],
				Type: "member_joined", ActorID: users["diana"].id,
				Timestamp: nowMs - 20*hour, Score: 15,
				Data: MemberJoinedData{
					Username: users["diana"].displayName, DisplayName: users["diana"].displayName,
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
					Title: "ranked grind", Game: "Counter-Strike 2",
					DurationMin: 120, PeakViewers: 3,
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

	// ── Game outcomes (specs 18+19) ─────────────────────────────────
	// Seed game_session outcomes + per-user stats + a recap so the personal
	// "You strip", the feed notability budget, and the weekly-recap game section
	// are all testable locally (the real recap job only runs Monday 00:00 UTC).
	//
	// The sessions exercise the whole surface: several adapters (real GameIDs so
	// bundled icons render), one Riot-verified LoL session, every notability
	// archetype (heater, flawless, sympathy, big night), draws, and routine
	// sessions that must be pruned from the feed into the recap. With the
	// 2-card budget, only the top two (bob's heater 210, diana's skid 150)
	// earn feed cards; everything else folds into the recap.
	if gid, ok := crewIDs["Gamers"]; ok {
		gameSessions := []CrewEvent{
			// NOTABLE — bob's CS2 heater: 5-win streak + flawless night
			// (quality 120+70+20 = 210 → top card).
			{
				ID: generateEventID(), CrewID: gid,
				Type: "game_session", ActorID: users["bob"].id,
				Timestamp: nowMs - 2*hour, Score: 30,
				Data: GameSessionData{
					GameName: "Counter-Strike 2", GameID: "counter-strike-2",
					PlayerIDs:   []string{users["bob"].id},
					PlayerNames: []string{users["bob"].displayName},
					DurationMin: 95, Wins: 5, Losses: 0, Result: "win", StreakAfter: 5,
				},
			},
			// NOTABLE — diana's rough LoL night (sympathy card), server-verified
			// via the Riot proxy → shows the VERIFIED badge (quality 80+50+20 = 150).
			{
				ID: generateEventID(), CrewID: gid,
				Type: "game_session", ActorID: users["diana"].id,
				Timestamp: nowMs - 3*hour, Score: 30,
				Data: GameSessionData{
					GameName: "League of Legends", GameID: "league-of-legends",
					PlayerIDs:   []string{users["diana"].id},
					PlayerNames: []string{users["diana"].displayName},
					DurationMin: 210, Wins: 0, Losses: 5, Result: "loss", StreakAfter: -3,
					Verified: true,
				},
			},
			// NOTABLE but over budget — alice's big Dota 2 night (8 matches,
			// quality 80+50 = 130) → third place, pruned by the 2-card cap,
			// still counts in the recap.
			{
				ID: generateEventID(), CrewID: gid,
				Type: "game_session", ActorID: users["alice"].id,
				Timestamp: nowMs - 9*hour, Score: 30,
				Data: GameSessionData{
					GameName: "Dota 2", GameID: "dota-2",
					PlayerIDs:   []string{users["alice"].id},
					PlayerNames: []string{users["alice"].displayName},
					DurationMin: 300, Wins: 5, Losses: 3, Result: "win", StreakAfter: 3,
				},
			},
			// ROUTINE — charlie's even Rocket League session with a draw
			// (streak 1, 5 matches → quality 20, below the 50 floor).
			{
				ID: generateEventID(), CrewID: gid,
				Type: "game_session", ActorID: users["charlie"].id,
				Timestamp: nowMs - 6*hour, Score: 15,
				Data: GameSessionData{
					GameName: "Rocket League", GameID: "rocket-league",
					PlayerIDs:   []string{users["charlie"].id},
					PlayerNames: []string{users["charlie"].displayName},
					DurationMin: 55, Wins: 2, Losses: 2, Draws: 1, Result: "even", StreakAfter: 1,
				},
			},
			// ROUTINE — bob's short Hearthstone session earlier in the week.
			{
				ID: generateEventID(), CrewID: gid,
				Type: "game_session", ActorID: users["bob"].id,
				Timestamp: nowMs - 30*hour, Score: 15,
				Data: GameSessionData{
					GameName: "Hearthstone", GameID: "hearthstone",
					PlayerIDs:   []string{users["bob"].id},
					PlayerNames: []string{users["bob"].displayName},
					DurationMin: 40, Wins: 2, Losses: 1, Result: "win", StreakAfter: 1,
				},
			},
			// ROUTINE — diana's draw-only StarCraft II night (the "draw-only
			// session showed nothing" regression case: quality 0, recap only).
			{
				ID: generateEventID(), CrewID: gid,
				Type: "game_session", ActorID: users["diana"].id,
				Timestamp: nowMs - 50*hour, Score: 15,
				Data: GameSessionData{
					GameName: "StarCraft II", GameID: "starcraft-2",
					PlayerIDs:   []string{users["diana"].id},
					PlayerNames: []string{users["diana"].displayName},
					DurationMin: 70, Draws: 2, Result: "even", StreakAfter: 0,
				},
			},
		}
		for _, ev := range gameSessions {
			if err := AppendCrewEvent(ctx, nk, gid, ev); err != nil {
				logger.Warn("dev_seed: game_session append failed: %v", err)
			}
		}
		logger.Info("dev_seed: %d game sessions seeded (2 notable in budget, 1 pruned, 3 routine)", len(gameSessions))
	}

	// ── Session card variants (voice + stream redesign) ─────────────
	// The busy crews (Gamers/Devs) are realistic but their filler budget hides
	// most cards, so the quiet crews double as variant galleries: everything
	// seeded here actually renders. Covers each branch of the redesigned
	// SessionCard — typed header, icon vs badge vs channel tile, participants
	// line overflow, humanized durations, peak stats.
	//
	// Design → VOICE variants.
	if gid, ok := crewIDs["Design"]; ok {
		voiceVariants := []CrewEvent{
			// Marathon session → "9h 7m" (the case the old "3:00" format
			// could not express) with a 4-person overflow line ("+2").
			{
				ID: generateEventID(), CrewID: gid,
				Type: "voice_session", ActorID: "",
				Timestamp: nowMs - 2*hour, Score: 20,
				Data: VoiceSessionData{
					ChannelName: "General",
					ParticipantIDs: []string{users["alice"].id, users["bob"].id,
						users["diana"].id, users["charlie"].id},
					ParticipantNames: []string{users["alice"].displayName, users["bob"].displayName,
						users["diana"].displayName, users["charlie"].displayName},
					DurationMin: 547, PeakCount: 4,
				},
			},
			// Two-person session, no overflow, ordinary duration.
			{
				ID: generateEventID(), CrewID: gid,
				Type: "voice_session", ActorID: "",
				Timestamp: nowMs - 7*hour, Score: 20,
				Data: VoiceSessionData{
					ChannelName:      "Critique",
					ParticipantIDs:   []string{users["alice"].id, users["diana"].id},
					ParticipantNames: []string{users["alice"].displayName, users["diana"].displayName},
					DurationMin:      42, PeakCount: 2,
				},
			},
			// Sub-hour edge: 1 minute — must read "1m", never "1:00".
			{
				ID: generateEventID(), CrewID: gid,
				Type: "voice_session", ActorID: "",
				Timestamp: nowMs - 26*hour, Score: 20,
				Data: VoiceSessionData{
					ChannelName:      "General",
					ParticipantIDs:   []string{users["bob"].id, users["alice"].id},
					ParticipantNames: []string{users["bob"].displayName, users["alice"].displayName},
					DurationMin:      1, PeakCount: 2,
				},
			},
			// No peak recorded (older event shape) → the peak row hides.
			{
				ID: generateEventID(), CrewID: gid,
				Type: "voice_session", ActorID: "",
				Timestamp: nowMs - 32*hour, Score: 20,
				Data: VoiceSessionData{
					ChannelName:      "Critique",
					ParticipantIDs:   []string{users["diana"].id, users["bob"].id},
					ParticipantNames: []string{users["diana"].displayName, users["bob"].displayName},
					DurationMin:      18,
				},
			},
			// Catch-up card alongside the voice variants.
			{
				ID: generateEventID(), CrewID: gid,
				Type: "member_joined", ActorID: users["bob"].id,
				Timestamp: nowMs - 44*hour, Score: 15,
				Data: MemberJoinedData{
					Username: users["bob"].displayName, DisplayName: users["bob"].displayName,
				},
			},
		}
		for _, ev := range voiceVariants {
			if err := AppendCrewEvent(ctx, nk, gid, ev); err != nil {
				logger.Warn("dev_seed: voice variant append failed: %v", err)
			}
		}
		logger.Info("dev_seed: %d voice-session variants seeded in Design", len(voiceVariants))
	}

	// Retro → STREAM variants (game icon resolution is the interesting axis).
	if gid, ok := crewIDs["Retro"]; ok {
		streamVariants := []CrewEvent{
			// Known game → bundled icon + streamer avatar badge + viewers.
			{
				ID: generateEventID(), CrewID: gid,
				Type: "stream_session", ActorID: users["bob"].id,
				Timestamp: nowMs - 3*hour, Score: 30,
				Data: StreamSessionData{
					StreamerID: users["bob"].id, StreamerName: users["bob"].displayName,
					Title: "building a megabase", Game: "Minecraft",
					DurationMin: 138, PeakViewers: 4,
					ViewerIDs: []string{users["alice"].id, users["diana"].id, users["charlie"].id},
				},
			},
			// Game outside the bundled DB → short-name badge fallback, and the
			// crew-shared icon fetch path if someone uploaded one.
			{
				ID: generateEventID(), CrewID: gid,
				Type: "stream_session", ActorID: users["diana"].id,
				Timestamp: nowMs - 11*hour, Score: 30,
				Data: StreamSessionData{
					StreamerID: users["diana"].id, StreamerName: users["diana"].displayName,
					Title: "indie night", Game: "Night Stones",
					DurationMin: 64, PeakViewers: 2,
					ViewerIDs: []string{users["alice"].id},
				},
			},
			// No game at all (desktop/just-chatting stream) → title leads.
			{
				ID: generateEventID(), CrewID: gid,
				Type: "stream_session", ActorID: users["charlie"].id,
				Timestamp: nowMs - 28*hour, Score: 30,
				Data: StreamSessionData{
					StreamerID: users["charlie"].id, StreamerName: users["charlie"].displayName,
					Title: "CRT calibration deep dive",
					DurationMin: 51, PeakViewers: 3,
					ViewerIDs: []string{users["bob"].id, users["alice"].id},
				},
			},
			// Nobody watched → peak row hides; 1-minute humanization.
			{
				ID: generateEventID(), CrewID: gid,
				Type: "stream_session", ActorID: users["alice"].id,
				Timestamp: nowMs - 40*hour, Score: 30,
				Data: StreamSessionData{
					StreamerID: users["alice"].id, StreamerName: users["alice"].displayName,
					Title: "oops wrong window", Game: "Dota 2",
					DurationMin: 1,
				},
			},
		}
		// Notable session for a user-confirmed custom game: the rich card with
		// no bundled art, so it exercises the runtime/crew-shared icon path and
		// the short-name badge fallback. Also proves the dedicated game-session
		// filler slot — it must survive alongside the four streams above.
		customGame := CrewEvent{
			ID: generateEventID(), CrewID: gid,
			Type: "game_session", ActorID: users["bob"].id,
			Timestamp: nowMs - 5*hour, Score: 30,
			Data: GameSessionData{
				GameName: "Night Stones", GameID: "custom-night-stones",
				PlayerIDs:   []string{users["bob"].id, users["charlie"].id},
				PlayerNames: []string{users["bob"].displayName, users["charlie"].displayName},
				DurationMin: 128, Wins: 4, Losses: 0, Result: "win", StreakAfter: 4,
			},
		}
		if err := AppendCrewEvent(ctx, nk, gid, customGame); err != nil {
			logger.Warn("dev_seed: custom-game session append failed: %v", err)
		}

		for _, ev := range streamVariants {
			if err := AppendCrewEvent(ctx, nk, gid, ev); err != nil {
				logger.Warn("dev_seed: stream variant append failed: %v", err)
			}
		}
		logger.Info("dev_seed: %d stream-session variants + 1 custom-game session seeded in Retro", len(streamVariants))
	}

	// Per-user stats (You strip + profile): multi-game histories with varied
	// form so streaks, win rates, draws, and the "most recently played" pick
	// all have something to show. Each entry is one session (w, l, d);
	// last entry decides the current streak direction.
	type statsSeed struct {
		gameID   string
		sessions [][3]int
	}
	seedStats := func(userID string, games []statsSeed) {
		if userID == "" {
			return
		}
		for _, g := range games {
			for _, s := range g.sessions {
				if _, _, err := UpdateUserGameStats(ctx, nk, userID, g.gameID, s[0], s[1], s[2]); err != nil {
					logger.Warn("dev_seed: user_game_stats update failed for %s/%s: %v", userID, g.gameID, err)
				}
			}
		}
	}
	// bob: CS2 heater (active game) + an older Hearthstone record.
	seedStats(users["bob"].id, []statsSeed{
		{gameID: "hearthstone", sessions: [][3]int{{2, 1, 0}, {1, 2, 0}}},
		{gameID: "counter-strike-2", sessions: [][3]int{{3, 2, 0}, {2, 2, 1}, {4, 1, 0}, {5, 0, 0}}},
	})
	// diana: LoL skid (active) after a decent Valorant stretch.
	seedStats(users["diana"].id, []statsSeed{
		{gameID: "valorant", sessions: [][3]int{{4, 2, 0}, {3, 1, 0}}},
		{gameID: "league-of-legends", sessions: [][3]int{{2, 3, 0}, {1, 4, 0}, {0, 5, 0}}},
	})
	// alice: steady Dota 2 grinder with draws in the form.
	seedStats(users["alice"].id, []statsSeed{
		{gameID: "dota-2", sessions: [][3]int{{3, 2, 0}, {2, 2, 1}, {5, 3, 0}}},
	})
	// charlie: even Rocket League record.
	seedStats(users["charlie"].id, []statsSeed{
		{gameID: "rocket-league", sessions: [][3]int{{2, 2, 1}, {3, 2, 0}}},
	})
	// Caller (the local tester) gets a two-game history so their own You
	// strip shows the most recently played game with a healthy streak.
	if callerID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string); ok {
		seedStats(callerID, []statsSeed{
			{gameID: "league-of-legends", sessions: [][3]int{{2, 2, 0}, {3, 1, 0}}},
			{gameID: "counter-strike-2", sessions: [][3]int{{5, 2, 0}, {4, 1, 0}, {3, 3, 1}, {6, 0, 0}}},
		})
	}
	logger.Info("dev_seed: user_game_stats seeded (bob, diana, alice, charlie, caller — multi-game)")

	// Generate the weekly recap now so the game leaderboard + awards are visible
	// immediately instead of waiting for the scheduled job. Design/Retro are
	// included so their variant galleries also get a recap card (Retro's has a
	// games section from the custom-game session; Design's is voice-only).
	recapCrews := []string{"Gamers", "Devs", "Design", "Retro"}
	for _, crewName := range recapCrews {
		if cid, ok := crewIDs[crewName]; ok {
			generateWeeklyRecap(ctx, nk, logger, cid)
		}
	}
	logger.Info("dev_seed: weekly recaps generated (%d crews)", len(recapCrews))

	// Set stale last_seen for all users in seeded crews so catch-up triggers
	lastSeenCrews := []string{"Gamers", "Devs", "Music", "Design", "Retro"}
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

	// ── invite codes for all seed crews ────────────────────────────
	inviteCodes := map[string]string{
		"Devs":   "DEVS-0001",
		"Gamers": "GAME-0001",
		"Music":  "MUSC-0001",
		"Design": "DSGN-0001",
		"Ops":    "OPS0-0001",
		"Retro":  "RETR-0001",
	}
	inviteCount := 0
	for crewName, code := range inviteCodes {
		cid, ok := crewIDs[crewName]
		if !ok {
			continue
		}
		fwdMap := map[string]string{"crew_id": cid}
		if bob, ok := users["bob"]; ok {
			fwdMap["inviter_user_id"] = bob.id
		}
		fwdValue, _ := json.Marshal(fwdMap)
		_, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{
			{
				Collection:      InviteCodeCollection,
				Key:             code,
				UserID:          SystemUserID,
				Value:           string(fwdValue),
				PermissionRead:  2,
				PermissionWrite: 0,
			},
		})
		if err != nil {
			logger.Warn("dev_seed: invite code for %s: %v", crewName, err)
		} else {
			inviteCount++
		}
	}
	logger.Info("dev_seed: %d invite codes seeded", inviteCount)

	resp, _ := json.Marshal(map[string]interface{}{
		"success":        true,
		"users":          len(users),
		"crews":          len(crewIDs),
		"voice_rooms":    3,
		"voice_channels": 5 + (len(crewIDs) - 2),
		"streams":        1,
		"crew_events":    eventsWritten,
		"clips_seeded":   clipCount["Gamers"] + clipCount["Devs"] + clipCount["Music"],
		"invite_codes":   inviteCount,
	})
	return string(resp), nil
}
