package main

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"testing"
	"time"

	"github.com/heroiclabs/nakama-common/api"
	"github.com/heroiclabs/nakama-common/runtime"
)

func resetGuestState() {
	guestSessionsMu.Lock()
	guestSessions = make(map[string]*guestSession)
	guestSessionsMu.Unlock()

	guestLastJoinMu.Lock()
	guestLastJoin = make(map[string]time.Time)
	guestLastJoinMu.Unlock()
}

// ---------------------------------------------------------------------------
// Nickname sanitising — the guest name is shown to every crew member, so it is
// untrusted input rendered in someone else's client.
// ---------------------------------------------------------------------------

func TestSanitizeGuestNickname(t *testing.T) {
	cases := []struct {
		name string
		in   string
		want string
	}{
		{"plain", "mikkel", "mikkel"},
		{"trims", "  mikkel  ", "mikkel"},
		{"empty falls back", "", "guest"},
		{"whitespace only falls back", "   \t ", "guest"},
		{"strips control chars", "mik\x00kel\x07", "mikkel"},
		{"strips newlines that would break a roster row", "mik\nkel", "mikkel"},
		{"collapses inner whitespace", "mik    kel", "mik kel"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := sanitizeGuestNickname(tc.in); got != tc.want {
				t.Errorf("sanitizeGuestNickname(%q) = %q, want %q", tc.in, got, tc.want)
			}
		})
	}
}

func TestSanitizeGuestNickname_CapsLength(t *testing.T) {
	got := sanitizeGuestNickname(strings.Repeat("a", 200))
	if len([]rune(got)) != maxGuestNicknameLen {
		t.Errorf("expected %d runes, got %d", maxGuestNicknameLen, len([]rune(got)))
	}
}

func TestSanitizeGuestNickname_TruncatesOnRunesNotBytes(t *testing.T) {
	// Truncating a multi-byte name by bytes would emit invalid UTF-8 into
	// every crew member's roster.
	got := sanitizeGuestNickname(strings.Repeat("ö", 40))
	if len([]rune(got)) != maxGuestNicknameLen {
		t.Errorf("expected %d runes, got %d", maxGuestNicknameLen, len([]rune(got)))
	}
	for _, r := range got {
		if r != 'ö' {
			t.Fatalf("truncation corrupted the string: %q", got)
		}
	}
}

// ---------------------------------------------------------------------------
// Guest cap
// ---------------------------------------------------------------------------

func TestCountGuestsInChannel_IgnoresMembers(t *testing.T) {
	resetVoiceState()

	voiceRoomsMu.Lock()
	voiceRooms["ch_1"] = &VoiceRoom{
		ChannelID: "ch_1",
		CrewID:    "crew_1",
		Members: map[string]*VoiceMemberState{
			"member_a": {UserID: "member_a", Username: "alice"},
			"member_b": {UserID: "member_b", Username: "bob"},
			"guest_a":  {UserID: "guest_a", Username: "visitor", IsGuest: true},
		},
	}
	voiceRoomsMu.Unlock()

	if got := countGuestsInChannel("ch_1", ""); got != 1 {
		t.Errorf("expected 1 guest among 3 participants, got %d", got)
	}
}

func TestCountGuestsInChannel_ExcludesCaller(t *testing.T) {
	resetVoiceState()

	voiceRoomsMu.Lock()
	voiceRooms["ch_1"] = &VoiceRoom{
		ChannelID: "ch_1",
		CrewID:    "crew_1",
		Members: map[string]*VoiceMemberState{
			"guest_a": {UserID: "guest_a", IsGuest: true},
			"guest_b": {UserID: "guest_b", IsGuest: true},
		},
	}
	voiceRoomsMu.Unlock()

	// A guest reconnecting must not be blocked by their own stale seat.
	if got := countGuestsInChannel("ch_1", "guest_a"); got != 1 {
		t.Errorf("expected caller to be excluded, got %d", got)
	}
}

func TestCountGuestsInChannel_UnknownChannel(t *testing.T) {
	resetVoiceState()
	if got := countGuestsInChannel("nope", ""); got != 0 {
		t.Errorf("expected 0 for unknown channel, got %d", got)
	}
}

// ---------------------------------------------------------------------------
// Session lifetime
// ---------------------------------------------------------------------------

func TestGuestSessionLifecycle(t *testing.T) {
	resetGuestState()

	if IsGuestUser("u1") {
		t.Error("unknown user should not be a guest")
	}
	rememberGuestSession("u1", "crew_1", "ch_1")
	if !IsGuestUser("u1") {
		t.Error("expected u1 to be a guest after joining")
	}
	forgetGuestSession("u1")
	if IsGuestUser("u1") {
		t.Error("expected u1 to stop being a guest after leaving")
	}
}

func TestExpiredGuestUserIDs(t *testing.T) {
	resetGuestState()

	now := time.Now()
	guestSessionsMu.Lock()
	guestSessions["fresh"] = &guestSession{JoinedAt: now.Add(-1 * time.Minute)}
	guestSessions["stale"] = &guestSession{JoinedAt: now.Add(-GuestSessionTTL - time.Minute)}
	guestSessionsMu.Unlock()

	expired := expiredGuestUserIDs(now)
	if len(expired) != 1 || expired[0] != "stale" {
		t.Errorf("expected only the stale session to expire, got %v", expired)
	}
}

func TestExpiredGuestUserIDs_BoundaryIsInclusive(t *testing.T) {
	resetGuestState()

	now := time.Now()
	guestSessionsMu.Lock()
	guestSessions["exactly_ttl"] = &guestSession{JoinedAt: now.Add(-GuestSessionTTL)}
	guestSessionsMu.Unlock()

	// Exactly at the TTL is still inside the session; expiry is strictly past it.
	if got := expiredGuestUserIDs(now); len(got) != 0 {
		t.Errorf("expected no expiry exactly at the TTL, got %v", got)
	}
}

// ---------------------------------------------------------------------------
// Guest policy
// ---------------------------------------------------------------------------

func TestParseGuestPolicy(t *testing.T) {
	cases := []struct {
		name string
		meta string
		want string
	}{
		{"absent metadata defaults open", "", GuestPolicyOpen},
		{"empty object defaults open", `{}`, GuestPolicyOpen},
		{"malformed json defaults open", `{not json`, GuestPolicyOpen},
		{"unrelated keys default open", `{"invite_policy":"admins"}`, GuestPolicyOpen},
		{"explicit off", `{"guest_policy":"off"}`, GuestPolicyOff},
		{"explicit open", `{"guest_policy":"open"}`, GuestPolicyOpen},
		{"unknown value defaults open", `{"guest_policy":"maybe"}`, GuestPolicyOpen},
		{"wrong type defaults open", `{"guest_policy":true}`, GuestPolicyOpen},
		{"preserves sibling policy", `{"invite_policy":"admins","guest_policy":"off"}`, GuestPolicyOff},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := parseGuestPolicy(tc.meta); got != tc.want {
				t.Errorf("parseGuestPolicy(%q) = %q, want %q", tc.meta, got, tc.want)
			}
		})
	}
}

// ---------------------------------------------------------------------------
// Feed projection — the guarantee that playable media never reaches a guest
// ---------------------------------------------------------------------------

func TestCollectGuestClips_WithholdsMedia(t *testing.T) {
	doc := &CrewClipsDoc{Clips: []StoredClip{
		{
			ClipID:          "c1",
			ClipType:        "voice",
			ClipperName:     "alice",
			DurationSeconds: 18.5,
			Game:            "Counter-Strike 2",
			MediaURL:        "https://cdn.example/secret-clip.mp4",
			LocalPath:       "/Users/alice/clips/secret.mp4",
			ActorID:         "user-uuid-alice",
		},
	}}

	out, total := collectGuestClips(doc, nil, 6)
	if len(out) != 1 || total != 1 {
		t.Fatalf("expected 1 clip, got %d (total %d)", len(out), total)
	}
	if out[0].ClipperName != "alice" || out[0].DurationSeconds != 18.5 {
		t.Errorf("metadata lost in projection: %+v", out[0])
	}

	// Serialise the way the RPC does and assert nothing sensitive survives.
	encoded, err := json.Marshal(out)
	if err != nil {
		t.Fatalf("marshal failed: %v", err)
	}
	for _, forbidden := range []string{"secret-clip.mp4", "/Users/alice", "user-uuid-alice", "media_url", "local_path", "actor_id"} {
		if strings.Contains(string(encoded), forbidden) {
			t.Errorf("guest clip payload leaked %q: %s", forbidden, encoded)
		}
	}
}

func TestCollectGuestClips_NewestFirstAndLimited(t *testing.T) {
	doc := &CrewClipsDoc{Clips: []StoredClip{
		{ClipID: "a", ClipperName: "oldest", Ts: 100},
		{ClipID: "b", ClipperName: "middle", Ts: 200},
		{ClipID: "c", ClipperName: "newest", Ts: 300},
	}}
	out, total := collectGuestClips(doc, nil, 2)
	if len(out) != 2 {
		t.Fatalf("expected the limit to apply, got %d", len(out))
	}
	if total != 3 {
		t.Errorf("expected the total to count every clip, got %d", total)
	}
	if out[0].ClipperName != "newest" || out[1].ClipperName != "middle" {
		t.Errorf("expected newest first, got %q then %q", out[0].ClipperName, out[1].ClipperName)
	}
}

// Clips live in two places. Reading only the durable document silently
// under-reports for crews whose clips are still in the event ledger — which is
// what the dev seed produces.
func TestCollectGuestClips_MergesLedgerAndDocument(t *testing.T) {
	doc := &CrewClipsDoc{Clips: []StoredClip{
		{ClipID: "durable", ClipperName: "alice", Ts: 100},
	}}
	ledger := &CrewEventLedger{Events: []CrewEvent{
		{Type: "clip", Timestamp: 300, Data: ClipData{ClipID: "ledgered", ClipperName: "charlie"}},
		{Type: "stream_session", Timestamp: 400, Data: StreamSessionData{StreamerName: "ignored"}},
	}}

	out, total := collectGuestClips(doc, ledger, 6)
	if total != 2 {
		t.Fatalf("expected clips from both sources, got %d", total)
	}
	if out[0].ClipperName != "charlie" || out[1].ClipperName != "alice" {
		t.Errorf("expected newest first across sources, got %q then %q", out[0].ClipperName, out[1].ClipperName)
	}
}

func TestCollectGuestClips_DeduplicatesByClipID(t *testing.T) {
	// The same clip can be in the ledger and the durable document at once.
	doc := &CrewClipsDoc{Clips: []StoredClip{{ClipID: "same", ClipperName: "alice", Ts: 100}}}
	ledger := &CrewEventLedger{Events: []CrewEvent{
		{Type: "clip", Timestamp: 100, Data: ClipData{ClipID: "same", ClipperName: "alice"}},
	}}

	out, total := collectGuestClips(doc, ledger, 6)
	if total != 1 || len(out) != 1 {
		t.Errorf("expected the duplicate to collapse, got %d (total %d)", len(out), total)
	}
}

func TestProjectGuestSessions_ReducesSnapshotsToABoolean(t *testing.T) {
	ledger := &CrewEventLedger{
		Events: []CrewEvent{
			{Type: "clip", Data: map[string]interface{}{"clip_id": "x"}},
			{
				Type:      "stream_session",
				Timestamp: 1234,
				Data: StreamSessionData{
					StreamerName: "b0bben",
					Title:        "Counter-Strike 2",
					DurationMin:  79,
					SnapshotURLs: []string{"https://cdn.example/shot1.jpg", "https://cdn.example/shot2.jpg"},
				},
			},
		},
	}

	out := projectGuestSessions(ledger, 4)
	if len(out) != 1 {
		t.Fatalf("expected 1 stream session, got %d", len(out))
	}
	if !out[0].HasSnapshots {
		t.Error("expected has_snapshots to be true")
	}
	if out[0].DurationMin != 79 || out[0].StreamerName != "b0bben" {
		t.Errorf("metadata lost: %+v", out[0])
	}

	encoded, _ := json.Marshal(out)
	if strings.Contains(string(encoded), "shot1.jpg") || strings.Contains(string(encoded), "cdn.example") {
		t.Errorf("guest session payload leaked snapshot URLs: %s", encoded)
	}
}

func TestProjectGuestSessions_NilLedger(t *testing.T) {
	if got := projectGuestSessions(nil, 4); got != nil {
		t.Errorf("expected nil for a nil ledger, got %v", got)
	}
}

// ---------------------------------------------------------------------------
// Ledger exclusion — a visitor must not turn up in the crew's weekly recap
// ---------------------------------------------------------------------------

func ledgerParticipantCount(channelID string) int {
	voiceSessionsMu.Lock()
	defer voiceSessionsMu.Unlock()
	sess, ok := voiceSessions[channelID]
	if !ok {
		return 0
	}
	return len(sess.participants)
}

func resetLedgerSessions() {
	voiceSessionsMu.Lock()
	voiceSessions = make(map[string]*voiceSessionInfo)
	voiceSessionsMu.Unlock()
}

func TestRecordLedgerSession_RecordsMembers(t *testing.T) {
	resetLedgerSessions()

	recordLedgerSession(voiceJoinParams{
		CrewID: "crew_1", ChannelID: "ch_1", ChannelName: "General",
		UserID: "member_a", Username: "alice",
	})

	if got := ledgerParticipantCount("ch_1"); got != 1 {
		t.Errorf("expected the member to be recorded, got %d participants", got)
	}
}

func TestRecordLedgerSession_SkipsGuests(t *testing.T) {
	resetLedgerSessions()

	// A visitor who sits in voice for 40 minutes must not be able to become the
	// crew's "most active" member in the weekly recap.
	recordLedgerSession(voiceJoinParams{
		CrewID: "crew_1", ChannelID: "ch_1", ChannelName: "General",
		UserID: "guest_a", Username: "visitor", IsGuest: true,
	})

	if got := ledgerParticipantCount("ch_1"); got != 0 {
		t.Errorf("guest must not open a ledger session, got %d participants", got)
	}
}

func TestRecordLedgerSession_GuestDoesNotJoinAMembersSession(t *testing.T) {
	resetLedgerSessions()

	recordLedgerSession(voiceJoinParams{
		CrewID: "crew_1", ChannelID: "ch_1", ChannelName: "General",
		UserID: "member_a", Username: "alice",
	})
	recordLedgerSession(voiceJoinParams{
		CrewID: "crew_1", ChannelID: "ch_1", ChannelName: "General",
		UserID: "guest_a", Username: "visitor", IsGuest: true,
	})

	if got := ledgerParticipantCount("ch_1"); got != 1 {
		t.Errorf("expected only the member in the ledger session, got %d participants", got)
	}
}

// ---------------------------------------------------------------------------
// guest_crew_feed voice channels
// ---------------------------------------------------------------------------

const (
	testGuestCode = "ABCD-EFGH"
	testGuestCrew = "crew-uuid-1"
	testInviterID = "inviter-uuid-1"
	testAliceID   = "member-uuid-alice"
	testBobID     = "member-uuid-bob"
	testGuestID   = "guest-uuid-visitor"
	testChLounge  = "ch_lounge"
	testChGeneral = "ch_general"
)

// seedGuestCrew builds a crew with an invite code, two members and two voice
// channels. The channels are stored out of sort order on purpose.
func seedGuestCrew() *fakeGuestNk {
	nk := newFakeGuestNk()
	nk.put(InviteCodeCollection, testGuestCode, SystemUserID, map[string]string{
		"crew_id": testGuestCrew, "inviter_user_id": testInviterID,
	})
	nk.groups[testGuestCrew] = &api.Group{Id: testGuestCrew, Name: "Night Owls", EdgeCount: 2}
	nk.addMember(testGuestCrew, testAliceID, "alice")
	nk.addMember(testGuestCrew, testBobID, "bob")
	nk.users[testInviterID] = &api.User{Id: testInviterID, DisplayName: "inviter"}
	nk.put(VoiceChannelCollection, testGuestCrew, SystemUserID, VoiceChannelList{Channels: []*VoiceChannelDef{
		{ID: testChLounge, Name: "Lounge", SortOrder: 1},
		{ID: testChGeneral, Name: "General", IsDefault: true, SortOrder: 0},
	}})
	return nk
}

// seatInVoice puts a participant in a voice room the way joinVoiceRoom does,
// without the presence writes and pushes.
func seatInVoice(channelID, crewID string, m VoiceMemberState) {
	voiceRoomsMu.Lock()
	room, ok := voiceRooms[channelID]
	if !ok {
		room = &VoiceRoom{ChannelID: channelID, CrewID: crewID, Members: map[string]*VoiceMemberState{}}
		voiceRooms[channelID] = room
	}
	member := m
	room.Members[m.UserID] = &member
	voiceRoomsMu.Unlock()

	voiceUserChannelMu.Lock()
	voiceUserChannel[m.UserID] = channelID
	voiceUserChannelMu.Unlock()

	voiceChannelCrewMu.Lock()
	voiceChannelCrew[channelID] = crewID
	voiceChannelCrewMu.Unlock()
}

// seatGuestCrewVoice seats alice and bob in General and a guest in Lounge.
func seatGuestCrewVoice() {
	seatInVoice(testChGeneral, testGuestCrew, VoiceMemberState{UserID: testBobID, Username: "bob", JoinedAt: 200, Speaking: true})
	seatInVoice(testChGeneral, testGuestCrew, VoiceMemberState{UserID: testAliceID, Username: "alice", JoinedAt: 100, Muted: true})
	seatInVoice(testChLounge, testGuestCrew, VoiceMemberState{UserID: testGuestID, Username: "visitor", JoinedAt: 300, Deafened: true, IsGuest: true})
}

func callGuestCrewFeed(t *testing.T, nk *fakeGuestNk) string {
	t.Helper()
	out, err := GuestCrewFeedRPC(context.Background(), testLogger(), nil, nk, `{"code":"abcd-efgh"}`)
	if err != nil {
		t.Fatalf("guest_crew_feed failed: %v", err)
	}
	return out
}

// findJSONKey reports the path of the first object key named key, at any depth.
func findJSONKey(v interface{}, key, path string) (string, bool) {
	switch x := v.(type) {
	case map[string]interface{}:
		for k, child := range x {
			if k == key {
				return path + "." + k, true
			}
			if p, ok := findJSONKey(child, key, path+"."+k); ok {
				return p, true
			}
		}
	case []interface{}:
		for i, child := range x {
			if p, ok := findJSONKey(child, key, fmt.Sprintf("%s[%d]", path, i)); ok {
				return p, true
			}
		}
	}
	return "", false
}

func TestGuestCrewFeed_VoiceChannelsInSortOrderWithOccupants(t *testing.T) {
	resetVoiceState()
	seatGuestCrewVoice()

	var resp guestCrewFeedResponse
	if err := json.Unmarshal([]byte(callGuestCrewFeed(t, seedGuestCrew())), &resp); err != nil {
		t.Fatalf("decode: %v", err)
	}

	if len(resp.VoiceChannels) != 2 {
		t.Fatalf("expected 2 voice channels, got %d", len(resp.VoiceChannels))
	}
	general, lounge := resp.VoiceChannels[0], resp.VoiceChannels[1]
	if general.ID != testChGeneral || general.Name != "General" || !general.IsDefault {
		t.Errorf("expected General first and default, got %+v", general)
	}
	if lounge.ID != testChLounge || lounge.IsDefault {
		t.Errorf("expected Lounge second and not default, got %+v", lounge)
	}

	want := []guestVoiceMember{
		{DisplayName: "alice", Muted: true},
		{DisplayName: "bob"},
	}
	if len(general.Members) != len(want) {
		t.Fatalf("expected %d members in General, got %+v", len(want), general.Members)
	}
	for i := range want {
		if general.Members[i] != want[i] {
			t.Errorf("General member %d = %+v, want %+v (join order)", i, general.Members[i], want[i])
		}
	}
	if len(lounge.Members) != 1 || lounge.Members[0] != (guestVoiceMember{DisplayName: "visitor", Deafened: true, IsGuest: true}) {
		t.Errorf("expected the guest in Lounge, got %+v", lounge.Members)
	}
}

func TestGuestCrewFeed_EmptyChannelHasEmptyMemberArray(t *testing.T) {
	resetVoiceState()

	out := callGuestCrewFeed(t, seedGuestCrew())
	// The lounge iterates members directly, so an empty room must be [] not null.
	if !strings.Contains(out, `"members":[]`) {
		t.Errorf("expected an empty members array for an empty channel: %s", out)
	}
	if strings.Contains(out, `"voice_channels":null`) {
		t.Errorf("voice_channels must be an array: %s", out)
	}
}

// guest_crew_feed is readable by anyone with an invite code. No user ID may
// reach it, from the voice rooms or from any other part of the payload.
func TestGuestCrewFeed_PayloadHasNoUserIDs(t *testing.T) {
	resetVoiceState()
	seatGuestCrewVoice()

	nk := seedGuestCrew()
	seedLiveStream(nk)
	out := callGuestCrewFeed(t, nk)

	var generic interface{}
	if err := json.Unmarshal([]byte(out), &generic); err != nil {
		t.Fatalf("decode: %v", err)
	}
	for _, key := range []string{"user_id", "member_ids", "speaking"} {
		if path, found := findJSONKey(generic, key, "$"); found {
			t.Errorf("guest_crew_feed has a %q key at %s: %s", key, path, out)
		}
	}
	for _, id := range []string{testAliceID, testBobID, testGuestID, testInviterID, testGuestCrew, testStreamID, "thumb.jpg", "viewer-uuid"} {
		if strings.Contains(out, id) {
			t.Errorf("guest_crew_feed leaked the ID %q: %s", id, out)
		}
	}
}

// ---------------------------------------------------------------------------
// guest_crew_feed live streams
// ---------------------------------------------------------------------------

const testStreamID = "stream_member-u_1700000000000"

// seedLiveStream makes alice stream Counter-Strike 2 in the test crew.
func seedLiveStream(nk *fakeGuestNk) {
	nk.put(StreamMetaCollection, testGuestCrew, SystemUserID, StreamMeta{
		StreamID:         testStreamID,
		CrewID:           testGuestCrew,
		StreamerID:       testAliceID,
		StreamerUsername: "alice",
		Title:            "ranked grind",
		StartedAt:        "2026-10-03T18:00:00Z",
		ThumbnailURL:     "https://cdn.example/thumb.jpg",
		ViewerIDs:        []string{"viewer-uuid-1"},
	})
	nk.put(PresenceCollection, testAliceID, testAliceID, UserPresence{
		UserID: testAliceID,
		Status: StatusOnline,
		Game:   &GamePresence{GameName: "Counter-Strike 2", GameID: "counter-strike-2"},
	})
}

func TestGuestCrewFeed_LiveStreamNameAndGame(t *testing.T) {
	resetVoiceState()
	nk := seedGuestCrew()
	seedLiveStream(nk)

	var resp guestCrewFeedResponse
	if err := json.Unmarshal([]byte(callGuestCrewFeed(t, nk)), &resp); err != nil {
		t.Fatalf("decode: %v", err)
	}
	want := guestLiveStream{StreamerName: "alice", Game: "Counter-Strike 2"}
	if len(resp.LiveStreams) != 1 || resp.LiveStreams[0] != want {
		t.Errorf("live_streams = %+v, want [%+v]", resp.LiveStreams, want)
	}
}

func TestGuestCrewFeed_NoLiveStreamIsAnEmptyArray(t *testing.T) {
	resetVoiceState()

	out := callGuestCrewFeed(t, seedGuestCrew())
	if !strings.Contains(out, `"live_streams":[]`) {
		t.Errorf("expected an empty live_streams array: %s", out)
	}
}

func TestProjectGuestLiveStreams(t *testing.T) {
	if got := projectGuestLiveStreams(&CrewStreamState{Active: false, StreamerUsername: "ghost"}, nil); len(got) != 0 {
		t.Errorf("an inactive stream must not be listed, got %+v", got)
	}
	got := projectGuestLiveStreams(&CrewStreamState{Active: true, StreamerUsername: "bob"}, nil)
	if len(got) != 1 || got[0] != (guestLiveStream{StreamerName: "bob"}) {
		t.Errorf("a streamer with no game presence keeps an empty game, got %+v", got)
	}
}

// ---------------------------------------------------------------------------
// guest_voice_roster
// ---------------------------------------------------------------------------

func callGuestVoiceRoster(ctx context.Context, nk *fakeGuestNk) (string, error) {
	return GuestVoiceRosterRPC(ctx, testLogger(), nil, nk, "{}")
}

func assertRuntimeErrorCode(t *testing.T, err error, want int) {
	t.Helper()
	if err == nil {
		t.Fatalf("expected an error with code %d, got success", want)
	}
	rerr, ok := err.(*runtime.Error)
	if !ok {
		t.Fatalf("expected a *runtime.Error, got %T: %v", err, err)
	}
	if rerr.Code != want {
		t.Errorf("error code = %d (%q), want %d", rerr.Code, rerr.Message, want)
	}
}

// seatGuestInGeneral seats the test guest in General next to alice and bob,
// and records the guest session as guest_voice_join does.
func seatGuestInGeneral() {
	seatInVoice(testChGeneral, testGuestCrew, VoiceMemberState{UserID: testBobID, Username: "bob", JoinedAt: 200, Deafened: true})
	seatInVoice(testChGeneral, testGuestCrew, VoiceMemberState{UserID: testAliceID, Username: "alice", JoinedAt: 100, Muted: true})
	seatInVoice(testChGeneral, testGuestCrew, VoiceMemberState{UserID: testGuestID, Username: "visitor", JoinedAt: 300, IsGuest: true})
	rememberGuestSession(testGuestID, testGuestCrew, testChGeneral)
}

func TestGuestVoiceRoster_ReturnsTheGuestsChannel(t *testing.T) {
	resetVoiceState()
	resetGuestState()
	seatGuestInGeneral()
	// Someone in another channel must not appear in the guest's roster.
	seatInVoice(testChLounge, testGuestCrew, VoiceMemberState{UserID: "member-uuid-carol", Username: "carol", JoinedAt: 50})

	out, err := callGuestVoiceRoster(ctxWithUser(testGuestID), seedGuestCrew())
	if err != nil {
		t.Fatalf("roster failed: %v", err)
	}
	var resp guestVoiceRosterResponse
	if err := json.Unmarshal([]byte(out), &resp); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if resp.ChannelID != testChGeneral || resp.ChannelName != "General" {
		t.Errorf("expected General, got id=%q name=%q", resp.ChannelID, resp.ChannelName)
	}
	want := []guestRosterMember{
		{UserID: testAliceID, DisplayName: "alice", Muted: true},
		{UserID: testBobID, DisplayName: "bob", Deafened: true},
		{UserID: testGuestID, DisplayName: "visitor", IsGuest: true},
	}
	if len(resp.Members) != len(want) {
		t.Fatalf("expected %d members, got %+v", len(want), resp.Members)
	}
	for i := range want {
		if resp.Members[i] != want[i] {
			t.Errorf("member %d = %+v, want %+v", i, resp.Members[i], want[i])
		}
	}
	if strings.Contains(out, "speaking") {
		t.Errorf("roster must not carry speaking: %s", out)
	}
}

func TestGuestVoiceRoster_RequiresASession(t *testing.T) {
	resetVoiceState()
	resetGuestState()
	seatGuestInGeneral()

	// The HTTP key path has no user in the context.
	_, err := callGuestVoiceRoster(context.Background(), seedGuestCrew())
	assertRuntimeErrorCode(t, err, 16)
}

func TestGuestVoiceRoster_RejectsCrewMembers(t *testing.T) {
	resetVoiceState()
	resetGuestState()
	seatGuestInGeneral()

	// alice sits in the same channel through voice_join, with no guest session.
	_, err := callGuestVoiceRoster(ctxWithUser(testAliceID), seedGuestCrew())
	assertRuntimeErrorCode(t, err, 7)
}

func TestGuestVoiceRoster_RejectsAGuestWithNoSeat(t *testing.T) {
	resetVoiceState()
	resetGuestState()
	seatGuestInGeneral()

	// The voice GC removed the seat, but the guest session is still there.
	voiceUserChannelMu.Lock()
	delete(voiceUserChannel, testGuestID)
	voiceUserChannelMu.Unlock()
	voiceRoomsMu.Lock()
	delete(voiceRooms[testChGeneral].Members, testGuestID)
	voiceRoomsMu.Unlock()

	_, err := callGuestVoiceRoster(ctxWithUser(testGuestID), seedGuestCrew())
	assertRuntimeErrorCode(t, err, 9)
}

func TestGuestVoiceRoster_RejectsAGuestInAnotherChannel(t *testing.T) {
	resetVoiceState()
	resetGuestState()
	seatGuestInGeneral()

	// The seat moved away from the channel the guest session records.
	voiceUserChannelMu.Lock()
	voiceUserChannel[testGuestID] = testChLounge
	voiceUserChannelMu.Unlock()

	_, err := callGuestVoiceRoster(ctxWithUser(testGuestID), seedGuestCrew())
	assertRuntimeErrorCode(t, err, 9)
}

func TestGuestVoiceRoster_RejectsAnExpiredGuest(t *testing.T) {
	resetVoiceState()
	resetGuestState()
	seatGuestInGeneral()

	// ExpireGuestSessions runs on the reconciler tick, so a session can outlive
	// the TTL for a short time. The roster must not serve it.
	guestSessionsMu.Lock()
	guestSessions[testGuestID].JoinedAt = time.Now().Add(-GuestSessionTTL - time.Minute)
	guestSessionsMu.Unlock()

	_, err := callGuestVoiceRoster(ctxWithUser(testGuestID), seedGuestCrew())
	assertRuntimeErrorCode(t, err, 9)
}
