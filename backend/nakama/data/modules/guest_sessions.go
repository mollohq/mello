package main

// Guest sessions back the web lounge at https://m3llo.app/join/{code}.
//
// A guest is an anonymous browser participant who followed an invite link. They
// are NOT a crew member: they never join the Nakama group, so crew rosters stay
// clean when someone bounces after twenty seconds. What a guest gets is voice —
// they are seated in the real voice room alongside members, and every native
// client sees them arrive.
//
// Everything else (streams, replays, clips, chat) is withheld on purpose. That
// gap is the reason to install the app, so the read path here returns metadata
// without any playable media URL.

import (
	"context"
	"database/sql"
	"encoding/json"
	"strings"
	"sync"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

const (
	// GuestPolicyOpen lets anyone holding an invite code join voice as a guest.
	GuestPolicyOpen = "open"
	// GuestPolicyOff refuses guests; the invite page falls back to a download CTA.
	GuestPolicyOff = "off"

	// MaxGuestsPerVoiceChannel caps concurrent guests in one channel. A stranger
	// with a link must never be able to crowd out the crew that owns the room.
	MaxGuestsPerVoiceChannel = 3

	// GuestSessionTTL bounds a single lounge visit. Past this the guest must
	// rejoin, which re-checks the policy and the caps.
	GuestSessionTTL = 30 * time.Minute

	// GuestJoinMinInterval rate-limits joins per invite code.
	GuestJoinMinInterval = 2 * time.Second

	maxGuestNicknameLen = 24
	guestFeedClipLimit  = 6
	guestFeedSessionCap = 4
)

// ---------------------------------------------------------------------------
// Guest bookkeeping
// ---------------------------------------------------------------------------

type guestSession struct {
	CrewID    string
	ChannelID string
	JoinedAt  time.Time
}

var (
	guestSessions   = make(map[string]*guestSession) // userID -> session
	guestSessionsMu sync.RWMutex

	guestLastJoin   = make(map[string]time.Time) // invite code -> last join
	guestLastJoinMu sync.Mutex
)

// guestPolicyFor reads the crew's guest policy from group metadata. Crews that
// have never set it are open, matching the invite_policy default.
func guestPolicyFor(ctx context.Context, nk runtime.NakamaModule, crewID string) string {
	groups, err := nk.GroupsGetId(ctx, []string{crewID})
	if err != nil || len(groups) == 0 {
		return GuestPolicyOpen
	}
	return parseGuestPolicy(groups[0].GetMetadata())
}

// parseGuestPolicy reads the policy out of raw group metadata. Anything absent,
// malformed or unrecognised means open — a crew has to opt out deliberately.
func parseGuestPolicy(meta string) string {
	if meta == "" {
		return GuestPolicyOpen
	}
	var m map[string]interface{}
	if json.Unmarshal([]byte(meta), &m) != nil {
		return GuestPolicyOpen
	}
	if p, ok := m["guest_policy"].(string); ok && p == GuestPolicyOff {
		return GuestPolicyOff
	}
	return GuestPolicyOpen
}

// sanitizeGuestNickname makes a client-supplied name safe to show to the crew.
// The name reaches every member's roster, so strip control characters, collapse
// whitespace and cap the length rather than trusting the browser.
func sanitizeGuestNickname(raw string) string {
	cleaned := strings.Map(func(r rune) rune {
		if r < 32 || r == 127 {
			return -1
		}
		return r
	}, raw)
	cleaned = strings.TrimSpace(strings.Join(strings.Fields(cleaned), " "))
	if cleaned == "" {
		return "guest"
	}
	if len([]rune(cleaned)) > maxGuestNicknameLen {
		cleaned = string([]rune(cleaned)[:maxGuestNicknameLen])
	}
	return cleaned
}

// countGuestsInChannel returns how many guests currently sit in a channel,
// excluding the caller so a rejoin is never blocked by its own stale entry.
func countGuestsInChannel(channelID, exceptUserID string) int {
	voiceRoomsMu.RLock()
	defer voiceRoomsMu.RUnlock()

	room, ok := voiceRooms[channelID]
	if !ok {
		return 0
	}
	n := 0
	for uid, m := range room.Members {
		if m.IsGuest && uid != exceptUserID {
			n++
		}
	}
	return n
}

// rememberGuestSession records a guest so expiry and cleanup can find them.
func rememberGuestSession(userID, crewID, channelID string) {
	guestSessionsMu.Lock()
	guestSessions[userID] = &guestSession{CrewID: crewID, ChannelID: channelID, JoinedAt: time.Now()}
	guestSessionsMu.Unlock()
}

func forgetGuestSession(userID string) {
	guestSessionsMu.Lock()
	delete(guestSessions, userID)
	guestSessionsMu.Unlock()
}

// IsGuestUser reports whether a user is in an active lounge session. The voice
// reconciler uses this to apply a shorter staleness window: a closed browser tab
// sends no leave, and a ghost guest in the roster is worse than an early drop.
func IsGuestUser(userID string) bool {
	guestSessionsMu.RLock()
	defer guestSessionsMu.RUnlock()
	_, ok := guestSessions[userID]
	return ok
}

// expiredGuestUserIDs lists guests whose session has outlived the TTL.
func expiredGuestUserIDs(now time.Time) []string {
	guestSessionsMu.RLock()
	defer guestSessionsMu.RUnlock()

	var expired []string
	for userID, s := range guestSessions {
		if now.Sub(s.JoinedAt) > GuestSessionTTL {
			expired = append(expired, userID)
		}
	}
	return expired
}

// ExpireGuestSessions drops guests past the TTL. Called by the voice reconciler.
func ExpireGuestSessions(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule) {
	for _, userID := range expiredGuestUserIDs(time.Now()) {
		logger.Info("Guest session expired: user=%s", userID)
		voiceLeaveInternal(ctx, logger, nk, userID)
		forgetGuestSession(userID)
	}
}

// ---------------------------------------------------------------------------
// RPC: guest_voice_join
// ---------------------------------------------------------------------------

type guestVoiceJoinRequest struct {
	Code      string `json:"code"`
	Nickname  string `json:"nickname"`
	ChannelID string `json:"channel_id,omitempty"`
}

// GuestVoiceJoinRPC seats a browser guest in a crew voice channel.
//
// The caller is authenticated as a throwaway device-auth account, so a session
// exists, but crew membership is deliberately NOT required — that is the whole
// point. Authorization comes from holding a valid invite code instead.
func GuestVoiceJoinRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req guestVoiceJoinRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	code := normalizeInviteCode(req.Code)
	crewID, _, err := lookupInviteCode(ctx, nk, code)
	if err != nil {
		return "", err
	}

	if guestPolicyFor(ctx, nk, crewID) == GuestPolicyOff {
		return "", runtime.NewError("this crew does not accept web guests", 7)
	}

	// Rate limit per code, so one link cannot be used to hammer the SFU.
	guestLastJoinMu.Lock()
	last := guestLastJoin[code]
	if time.Since(last) < GuestJoinMinInterval {
		guestLastJoinMu.Unlock()
		return "", runtime.NewError("too many join attempts, retry shortly", 8)
	}
	guestLastJoin[code] = time.Now()
	guestLastJoinMu.Unlock()

	// A browser cannot join the native P2P mesh, so the SFU is mandatory here.
	// Unlike VoiceJoinRPC there is no premium-crew gate and no P2P fallback: if
	// the SFU is unavailable the honest answer is that the lounge cannot open.
	if !sfuAuthEnabled() {
		return "", runtime.NewError("voice is unavailable for web guests right now", 14)
	}

	channelID, channelName, err := resolveVoiceChannel(ctx, nk, crewID, req.ChannelID)
	if err != nil {
		return "", err
	}

	if countGuestsInChannel(channelID, userID) >= MaxGuestsPerVoiceChannel {
		return "", runtime.NewError("this crew already has the maximum number of web guests", 8)
	}

	params := voiceJoinParams{
		CrewID:      crewID,
		ChannelID:   channelID,
		ChannelName: channelName,
		UserID:      userID,
		Username:    sanitizeGuestNickname(req.Nickname),
		MaxMembers:  MaxSFUVoiceChannelMembers,
		IsGuest:     true,
	}

	snap, err := joinVoiceRoom(ctx, logger, nk, params)
	if err != nil {
		return "", err
	}

	endpoint, token, signed := issueVoiceSFUToken(logger, params)
	if !signed {
		// Undo the seat: a guest with no token would sit in the roster in silence.
		voiceLeaveInternal(ctx, logger, nk, userID)
		return "", runtime.NewError("failed to authorize voice session", 13)
	}

	rememberGuestSession(userID, crewID, channelID)
	logger.Info("Guest voice join: user=%s nickname=%q crew=%s channel=%s", userID, params.Username, crewID, channelID)

	resp, _ := json.Marshal(map[string]interface{}{
		"success":      true,
		"crew_id":      crewID,
		"channel_id":   channelID,
		"channel_name": channelName,
		"voice_state":  snap,
		"mode":         "sfu",
		"sfu_endpoint": endpoint,
		"sfu_token":    token,
		"expires_in":   int(GuestSessionTTL.Seconds()),
	})
	return string(resp), nil
}

// ---------------------------------------------------------------------------
// RPC: guest_voice_leave
// ---------------------------------------------------------------------------

func GuestVoiceLeaveRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	voiceLeaveInternal(ctx, logger, nk, userID)
	forgetGuestSession(userID)
	logger.Info("Guest voice leave: user=%s", userID)

	return `{"success":true}`, nil
}

// ---------------------------------------------------------------------------
// RPC: guest_crew_feed
// ---------------------------------------------------------------------------

// guestClip is clip metadata with every playable field removed. The lounge shows
// that a clip exists, who made it and how long it runs; playing it needs the app.
type guestClip struct {
	ClipType        string  `json:"clip_type"`
	ClipperName     string  `json:"clipper_name"`
	DurationSeconds float64 `json:"duration_seconds"`
	Game            string  `json:"game,omitempty"`
	Ts              int64   `json:"ts"`
}

// guestSessionCard describes a past stream without exposing its snapshots.
type guestSessionCard struct {
	StreamerName string `json:"streamer_name"`
	Title        string `json:"title"`
	Game         string `json:"game,omitempty"`
	DurationMin  int    `json:"duration_min"`
	PeakViewers  int    `json:"peak_viewers"`
	HasSnapshots bool   `json:"has_snapshots"`
	Ts           int64  `json:"ts"`
}

type guestCrewFeedResponse struct {
	CrewName    string                `json:"crew_name"`
	MemberCount int                   `json:"member_count"`
	Members     []InviteMemberPreview `json:"members,omitempty"`
	InviterName string                `json:"inviter_display_name,omitempty"`
	GuestPolicy string                `json:"guest_policy"`
	Recap       *WeeklyRecapData      `json:"recap,omitempty"`
	Clips       []guestClip           `json:"clips,omitempty"`
	Sessions    []guestSessionCard    `json:"sessions,omitempty"`
	ClipCount   int                   `json:"clip_count"`
}

// GuestCrewFeedRPC returns the read-only crew feed behind an invite code.
//
// Callable with the Nakama HTTP key so the Cloudflare Pages function can render
// the lounge server-side. It returns a public-safe projection only: no media
// URLs, no local paths, no user IDs.
func GuestCrewFeedRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	var req struct {
		Code string `json:"code"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	crewID, inviterUserID, err := lookupInviteCode(ctx, nk, normalizeInviteCode(req.Code))
	if err != nil {
		return "", err
	}

	groups, err := nk.GroupsGetId(ctx, []string{crewID})
	if err != nil || len(groups) == 0 {
		return "", runtime.NewError("crew not found", 5)
	}
	group := groups[0]

	resp := guestCrewFeedResponse{
		CrewName:    group.GetName(),
		MemberCount: int(group.GetEdgeCount()),
		GuestPolicy: guestPolicyFor(ctx, nk, crewID),
	}

	if members, _, mErr := nk.GroupUsersList(ctx, crewID, 100, nil, ""); mErr == nil {
		for _, m := range members {
			name := m.GetUser().GetDisplayName()
			if name == "" {
				name = m.GetUser().GetUsername()
			}
			resp.Members = append(resp.Members, InviteMemberPreview{DisplayName: name, AvatarSeed: name})
			if len(resp.Members) >= 8 {
				break
			}
		}
	}

	if inviterUserID != "" {
		if users, uErr := nk.UsersGetId(ctx, []string{inviterUserID}, nil); uErr == nil && len(users) > 0 {
			name := users[0].GetDisplayName()
			if name == "" {
				name = users[0].GetUsername()
			}
			resp.InviterName = name
		}
	}

	_, recap, ledger := buildRecapHighlightWithData(ctx, nk, logger, crewID)
	resp.Recap = recap

	clipsDoc, _ := readClipsDoc(ctx, nk, crewID)
	resp.ClipCount = len(clipsDoc.Clips)
	resp.Clips = projectGuestClips(clipsDoc.Clips, guestFeedClipLimit)
	resp.Sessions = projectGuestSessions(ledger, guestFeedSessionCap)

	out, _ := json.Marshal(resp)
	return string(out), nil
}

// projectGuestClips converts stored clips into the guest-visible shape, newest
// first. StoredClip carries MediaURL and LocalPath; guestClip has neither field,
// so playable media cannot leak through this projection even by accident.
func projectGuestClips(clips []StoredClip, limit int) []guestClip {
	out := make([]guestClip, 0, limit)
	for i := len(clips) - 1; i >= 0 && len(out) < limit; i-- {
		c := clips[i]
		out = append(out, guestClip{
			ClipType:        c.ClipType,
			ClipperName:     c.ClipperName,
			DurationSeconds: c.DurationSeconds,
			Game:            c.Game,
			Ts:              c.Ts,
		})
	}
	return out
}

// projectGuestSessions summarises past streams, newest first. Snapshot URLs are
// reduced to a boolean: a guest learns that a replay exists, not what was on
// screen.
func projectGuestSessions(ledger *CrewEventLedger, limit int) []guestSessionCard {
	if ledger == nil {
		return nil
	}
	out := make([]guestSessionCard, 0, limit)
	for i := len(ledger.Events) - 1; i >= 0 && len(out) < limit; i-- {
		ev := ledger.Events[i]
		if ev.Type != "stream_session" {
			continue
		}
		dataBytes, err := json.Marshal(ev.Data)
		if err != nil {
			continue
		}
		var d StreamSessionData
		if json.Unmarshal(dataBytes, &d) != nil {
			continue
		}
		out = append(out, guestSessionCard{
			StreamerName: d.StreamerName,
			Title:        d.Title,
			Game:         d.Game,
			DurationMin:  d.DurationMin,
			PeakViewers:  d.PeakViewers,
			HasSnapshots: len(d.SnapshotURLs) > 0,
			Ts:           ev.Timestamp,
		})
	}
	return out
}
