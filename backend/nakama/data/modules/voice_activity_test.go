package main

import (
	"context"
	"testing"
	"time"
)

// seatVoice puts members in one voice channel as joinVoiceRoom does, each with
// a join time that many minutes ago. The open ledger session starts with the
// earliest join.
func seatVoice(crewID, channelID string, joinedMinAgo map[string]int, guests ...string) {
	resetVoiceState()
	resetLedgerSessions()
	voiceGCMissesMu.Lock()
	voiceGCMisses = make(map[string]int)
	voiceGCMissesMu.Unlock()

	isGuest := map[string]bool{}
	for _, g := range guests {
		isGuest[g] = true
	}
	now := time.Now()
	oldest := 0
	room := &VoiceRoom{ChannelID: channelID, CrewID: crewID, Members: map[string]*VoiceMemberState{}}
	for uid, ago := range joinedMinAgo {
		room.Members[uid] = &VoiceMemberState{
			UserID: uid, Username: uid, IsGuest: isGuest[uid],
			JoinedAt: now.Add(-time.Duration(ago) * time.Minute).UnixMilli(),
		}
		voiceUserChannel[uid] = channelID
		if !isGuest[uid] {
			voiceSessionOnJoin(channelID, crewID, "General", uid, uid)
		}
		if ago > oldest {
			oldest = ago
		}
	}
	voiceRooms[channelID] = room
	voiceChannelCrew[channelID] = crewID
	if sess, ok := voiceSessions[channelID]; ok {
		sess.startTime = now.Add(-time.Duration(oldest) * time.Minute)
	}
}

func voiceSessionEvents(t *testing.T, nk *fakeGuestNk, crewID string) []CrewEvent {
	t.Helper()
	ledger, _ := readLedger(context.Background(), nk, crewID)
	var out []CrewEvent
	for _, e := range ledger.Events {
		if e.Type == "voice_session" {
			out = append(out, e)
		}
	}
	return out
}

func sessionDuration(t *testing.T, e CrewEvent) int {
	t.Helper()
	d, ok := decodeCardData[VoiceSessionData](e.Data)
	if !ok {
		t.Fatalf("voice_session data: %+v", e.Data)
	}
	return d.DurationMin
}

func TestVoiceLeaveWritesEachMembersOwnMinutes(t *testing.T) {
	nk := newFakeGuestNk()
	ctx, lg := context.Background(), testLogger()
	// A joined 80 minutes ago, B 60 minutes ago. B leaves first.
	seatVoice("crew", "ch", map[string]int{"a": 80, "b": 60})

	voiceLeaveInternal(ctx, lg, nk, "b")
	if got := readActivity(t, nk, "b").VoiceMin; got != 60 {
		t.Fatalf("b voice_min = %d, want 60", got)
	}
	if len(voiceSessionEvents(t, nk, "crew")) != 0 {
		t.Fatal("the crew session event is written when the last member leaves")
	}

	voiceLeaveInternal(ctx, lg, nk, "a")
	a := readActivity(t, nk, "a")
	if a.VoiceMin != 80 || dayOf(t, a, time.Now().UTC().Format("2006-01-02")).VoiceMin != 80 {
		t.Fatalf("a doc: %+v", a)
	}
	// The crew level event stays one event with one duration.
	evs := voiceSessionEvents(t, nk, "crew")
	if len(evs) != 1 {
		t.Fatalf("voice_session events = %d, want 1", len(evs))
	}
	if d := sessionDuration(t, evs[0]); d != 80 {
		t.Fatalf("session duration = %d, want 80", d)
	}
}

func TestVoiceLeaveAloneWritesNoMinutes(t *testing.T) {
	nk := newFakeGuestNk()
	seatVoice("crew", "ch", map[string]int{"a": 45})
	voiceLeaveInternal(context.Background(), testLogger(), nk, "a")
	if _, ok := nk.storage[fakeStorageKey(UserActivityStatsCollection, userActivityStatsKey, "a")]; ok {
		t.Fatal("time alone in a channel is not crew voice time")
	}
}

func TestVoiceLeaveGuestWritesNoMinutes(t *testing.T) {
	nk := newFakeGuestNk()
	seatVoice("crew", "ch", map[string]int{"a": 30, "b": 30, "g": 30}, "g")
	voiceLeaveInternal(context.Background(), testLogger(), nk, "g")
	if _, ok := nk.storage[fakeStorageKey(UserActivityStatsCollection, userActivityStatsKey, "g")]; ok {
		t.Fatal("a guest has no stats doc")
	}
}

func TestVoiceRejoinSameChannelKeepsOneStint(t *testing.T) {
	// A reconnect keeps JoinedAt (upsertVoiceMember), so the leave counts the
	// whole stint once.
	nk := newFakeGuestNk()
	seatVoice("crew", "ch", map[string]int{"a": 30, "b": 30})
	upsertVoiceMember("ch", "crew", "a", "a", false)
	voiceLeaveInternal(context.Background(), testLogger(), nk, "a")
	if got := readActivity(t, nk, "a").VoiceMin; got != 30 {
		t.Fatalf("voice_min = %d, want 30", got)
	}
}

func presenceAt(t *testing.T, nk *fakeGuestNk, uid string, ago time.Duration) {
	t.Helper()
	nk.put(PresenceCollection, uid, uid, UserPresence{
		UserID: uid, Status: StatusOnline,
		UpdatedAt: time.Now().Add(-ago).UTC().Format(time.RFC3339),
	})
}

func TestVoiceGCEndsGhostTimeAtLastSignOfLife(t *testing.T) {
	nk := newFakeGuestNk()
	ctx, lg := context.Background(), testLogger()
	// b and the ghost have been in voice 180 minutes. The ghost last sent a
	// heartbeat 40 minutes ago, so its time is 140 minutes and not 180.
	seatVoice("crew", "ch", map[string]int{"ghost": 180, "b": 180})
	presenceAt(t, nk, "ghost", 40*time.Minute)
	presenceAt(t, nk, "b", 10*time.Second)

	voiceRoomGC(ctx, lg, nk) // first detection only counts a miss
	if readNoActivity(nk, "ghost") != true {
		t.Fatal("one detection must not remove the member")
	}
	voiceRoomGC(ctx, lg, nk)

	g := readActivity(t, nk, "ghost")
	if g.VoiceMin != 140 && g.VoiceMin != 139 {
		t.Fatalf("ghost voice_min = %d, want 140 (the GC moment would give 180)", g.VoiceMin)
	}
	if _, in := voiceUserChannel["ghost"]; in {
		t.Fatal("ghost still mapped to a channel")
	}
}

func readNoActivity(nk *fakeGuestNk, uid string) bool {
	_, ok := nk.storage[fakeStorageKey(UserActivityStatsCollection, userActivityStatsKey, uid)]
	return !ok
}

func TestVoiceGCCapsTheCrewSessionDurationAtTheGhost(t *testing.T) {
	nk := newFakeGuestNk()
	ctx, lg := context.Background(), testLogger()
	// b left a while ago. The ghost keeps the room open, with its last
	// heartbeat 40 minutes ago, and the room started 180 minutes ago.
	seatVoice("crew", "ch", map[string]int{"ghost": 180, "b": 180})
	voiceLeaveInternal(ctx, lg, nk, "b")
	presenceAt(t, nk, "ghost", 40*time.Minute)

	voiceRoomGC(ctx, lg, nk)
	voiceRoomGC(ctx, lg, nk)

	evs := voiceSessionEvents(t, nk, "crew")
	if len(evs) != 1 {
		t.Fatalf("voice_session events = %d, want 1", len(evs))
	}
	d := sessionDuration(t, evs[0])
	if d < 139 || d > 140 {
		t.Fatalf("session duration = %d, want 140: the ghost must not stretch it to 180", d)
	}
}

func TestVoiceGCRemovesAMemberWithoutAChannelEntry(t *testing.T) {
	nk := newFakeGuestNk()
	ctx, lg := context.Background(), testLogger()
	seatVoice("crew", "ch", map[string]int{"ghost": 60, "b": 60})
	// The user->channel entry was lost, so the old leave path returned at once
	// and the GC could never remove the member.
	voiceUserChannelMu.Lock()
	delete(voiceUserChannel, "ghost")
	voiceUserChannelMu.Unlock()
	presenceAt(t, nk, "ghost", 20*time.Minute)
	presenceAt(t, nk, "b", 5*time.Second)

	voiceRoomGC(ctx, lg, nk)
	voiceRoomGC(ctx, lg, nk)

	voiceRoomsMu.RLock()
	_, still := voiceRooms["ch"].Members["ghost"]
	voiceRoomsMu.RUnlock()
	if still {
		t.Fatal("the GC left the orphan member in the room")
	}
	if g := readActivity(t, nk, "ghost").VoiceMin; g < 39 || g > 40 {
		t.Fatalf("ghost voice_min = %d, want 40", g)
	}
}

func TestVoiceMemberMinutes(t *testing.T) {
	now := time.Now()
	cases := []struct {
		name   string
		joined int64
		want   int
	}{
		{"zero join", 0, 0},
		{"future join", now.Add(time.Minute).UnixMilli(), 0},
		{"under a minute", now.Add(-50 * time.Second).UnixMilli(), 0},
		{"90 minutes", now.Add(-90 * time.Minute).UnixMilli(), 90},
		{"capped at a day", now.Add(-72 * time.Hour).UnixMilli(), maxSessionDurationMin},
	}
	for _, c := range cases {
		if got := voiceMemberMinutes(c.joined, now); got != c.want {
			t.Errorf("%s: %d, want %d", c.name, got, c.want)
		}
	}
}
