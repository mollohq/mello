package main

import (
	"encoding/json"
	"strings"
	"testing"
	"time"
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

func TestProjectGuestClips_WithholdsMedia(t *testing.T) {
	clips := []StoredClip{
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
	}

	out := projectGuestClips(clips, 6)
	if len(out) != 1 {
		t.Fatalf("expected 1 clip, got %d", len(out))
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

func TestProjectGuestClips_NewestFirstAndLimited(t *testing.T) {
	clips := []StoredClip{
		{ClipperName: "oldest"},
		{ClipperName: "middle"},
		{ClipperName: "newest"},
	}
	out := projectGuestClips(clips, 2)
	if len(out) != 2 {
		t.Fatalf("expected the limit to apply, got %d", len(out))
	}
	if out[0].ClipperName != "newest" || out[1].ClipperName != "middle" {
		t.Errorf("expected newest first, got %q then %q", out[0].ClipperName, out[1].ClipperName)
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
