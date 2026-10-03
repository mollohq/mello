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
	"sort"
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

// guestVoiceMember is one person in a voice channel, as the public lounge
// shows them. The struct has no user ID field on purpose: guest_crew_feed is
// callable with the HTTP key, so anyone with an invite code can read it.
// Speaking is also absent, because the browser detects speech from the audio.
type guestVoiceMember struct {
	DisplayName string `json:"display_name"`
	Muted       bool   `json:"muted"`
	Deafened    bool   `json:"deafened"`
	IsGuest     bool   `json:"is_guest"`
}

// guestVoiceChannel is a crew voice channel and the people in it. The lounge
// sends ID back as channel_id in guest_voice_join to join this channel.
type guestVoiceChannel struct {
	ID        string             `json:"id"`
	Name      string             `json:"name"`
	IsDefault bool               `json:"is_default"`
	Members   []guestVoiceMember `json:"members"`
}

// guestLiveStream says that a crew member streams now, and what game. The
// lounge shows the card but cannot play the stream: watching needs the app.
type guestLiveStream struct {
	StreamerName string `json:"streamer_name"`
	Game         string `json:"game,omitempty"`
}

type guestCrewFeedResponse struct {
	CrewName      string                `json:"crew_name"`
	MemberCount   int                   `json:"member_count"`
	Members       []InviteMemberPreview `json:"members,omitempty"`
	InviterName   string                `json:"inviter_display_name,omitempty"`
	GuestPolicy   string                `json:"guest_policy"`
	Recap         *WeeklyRecapData      `json:"recap,omitempty"`
	Clips         []guestClip           `json:"clips,omitempty"`
	Sessions      []guestSessionCard    `json:"sessions,omitempty"`
	ClipCount     int                   `json:"clip_count"`
	VoiceChannels []guestVoiceChannel   `json:"voice_channels"`
	LiveStreams   []guestLiveStream     `json:"live_streams"`
}

// GuestCrewFeedRPC returns the read-only crew feed behind an invite code.
//
// Callable with the Nakama HTTP key so the Cloudflare Pages function can render
// the lounge server-side. It returns a public-safe projection only: no media
// URLs, no local paths, no user IDs. Keep this rule when you add a field. The
// guest structs carry no field for these values, so a leak needs a new field.
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
		CrewName:      group.GetName(),
		MemberCount:   int(group.GetEdgeCount()),
		GuestPolicy:   guestPolicyFor(ctx, nk, crewID),
		VoiceChannels: []guestVoiceChannel{},
		LiveStreams:   []guestLiveStream{},
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
	resp.Clips, resp.ClipCount = collectGuestClips(clipsDoc, ledger, guestFeedClipLimit)
	resp.Sessions = projectGuestSessions(ledger, guestFeedSessionCap)

	if channels, chErr := GetVoiceChannels(ctx, nk, crewID); chErr == nil && channels != nil {
		resp.VoiceChannels = projectGuestVoiceChannels(channels.Channels)
	}

	// stream_meta/{crew_id} is the live stream record that crew_state reads.
	// The record has no game field, so the game comes from the streamer's
	// presence, the same source as the crew's active games.
	if stream := getActiveStreamForCrew(ctx, nk, crewID); stream.Active {
		var game *GamePresence
		if p, pErr := ReadPresence(ctx, nk, stream.StreamerID); pErr == nil {
			game = p.Game
		}
		resp.LiveStreams = projectGuestLiveStreams(stream, game)
	}

	out, _ := json.Marshal(resp)
	return string(out), nil
}

// collectGuestClips merges the two places a crew's clips live: the durable
// crew_clips document and "clip" events still in the event ledger. Reading only
// one under-reports — the same split resolve_crew_invite and crew_feed handle.
// Returns the projected page plus the total unique count.
func collectGuestClips(doc *CrewClipsDoc, ledger *CrewEventLedger, limit int) ([]guestClip, int) {
	type dated struct {
		clip guestClip
		id   string
	}
	var all []dated
	seen := make(map[string]bool)

	add := func(id string, c guestClip) {
		if id != "" {
			if seen[id] {
				return
			}
			seen[id] = true
		}
		all = append(all, dated{clip: c, id: id})
	}

	if doc != nil {
		for _, c := range doc.Clips {
			add(c.ClipID, guestClip{
				ClipType:        c.ClipType,
				ClipperName:     c.ClipperName,
				DurationSeconds: c.DurationSeconds,
				Game:            c.Game,
				Ts:              c.Ts,
			})
		}
	}

	if ledger != nil {
		for _, ev := range ledger.Events {
			if ev.Type != "clip" {
				continue
			}
			dataBytes, err := json.Marshal(ev.Data)
			if err != nil {
				continue
			}
			var cd ClipData
			if json.Unmarshal(dataBytes, &cd) != nil {
				continue
			}
			add(cd.ClipID, guestClip{
				ClipType:        cd.ClipType,
				ClipperName:     cd.ClipperName,
				DurationSeconds: cd.DurationSeconds,
				Game:            cd.Game,
				Ts:              ev.Timestamp,
			})
		}
	}

	sort.SliceStable(all, func(i, j int) bool { return all[i].clip.Ts > all[j].clip.Ts })

	out := make([]guestClip, 0, limit)
	for _, d := range all {
		if len(out) >= limit {
			break
		}
		out = append(out, d.clip)
	}
	return out, len(all)
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

// sortedVoiceMembers returns the members of a voice channel in join order. The
// room is a map, so without a sort the lounge rows change order on each read.
func sortedVoiceMembers(channelID string) []*VoiceMemberState {
	members := GetVoiceChannelSnapshot(channelID).Members
	sort.SliceStable(members, func(i, j int) bool {
		if members[i].JoinedAt != members[j].JoinedAt {
			return members[i].JoinedAt < members[j].JoinedAt
		}
		return members[i].UserID < members[j].UserID
	})
	return members
}

// projectGuestVoiceChannels lists the crew's voice channels in sort order with
// the people in each one. It reads the in-memory voice rooms only. The result
// has no user IDs (see guestVoiceMember).
func projectGuestVoiceChannels(defs []*VoiceChannelDef) []guestVoiceChannel {
	ordered := make([]*VoiceChannelDef, 0, len(defs))
	for _, d := range defs {
		if d != nil {
			ordered = append(ordered, d)
		}
	}
	sort.SliceStable(ordered, func(i, j int) bool { return ordered[i].SortOrder < ordered[j].SortOrder })

	out := make([]guestVoiceChannel, 0, len(ordered))
	for _, d := range ordered {
		ch := guestVoiceChannel{
			ID:        d.ID,
			Name:      d.Name,
			IsDefault: d.IsDefault,
			Members:   []guestVoiceMember{},
		}
		for _, m := range sortedVoiceMembers(d.ID) {
			ch.Members = append(ch.Members, guestVoiceMember{
				DisplayName: m.Username,
				Muted:       m.Muted,
				Deafened:    m.Deafened,
				IsGuest:     m.IsGuest,
			})
		}
		out = append(out, ch)
	}
	return out
}

// projectGuestLiveStreams reduces the crew's live stream to a name and a game.
// The stream ID, streamer ID, thumbnail and viewer list stay on the server. A
// crew has at most one live stream today (stream_meta is keyed by crew), but
// the field is a list so that the lounge does not change if that limit goes.
func projectGuestLiveStreams(stream *CrewStreamState, game *GamePresence) []guestLiveStream {
	out := []guestLiveStream{}
	if stream == nil || !stream.Active {
		return out
	}
	ls := guestLiveStream{StreamerName: stream.StreamerUsername}
	if game != nil {
		ls.Game = game.GameName
	}
	return append(out, ls)
}
