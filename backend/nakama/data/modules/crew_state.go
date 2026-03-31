package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"strings"
	"sync"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
	"github.com/heroiclabs/nakama-common/rtapi"
)

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

const CrewStateCollection = "crew_state"

type CrewCounts struct {
	Online int `json:"online"`
	Total  int `json:"total"`
}

type CrewVoiceState struct {
	Active    bool                `json:"active"`
	MemberIDs []string           `json:"member_ids,omitempty"`
	Members   []*VoiceMemberInfo `json:"members"`
}

type VoiceMemberInfo struct {
	UserID   string `json:"user_id"`
	Username string `json:"username"`
	Avatar   string `json:"avatar,omitempty"`
	Speaking *bool  `json:"speaking,omitempty"` // nil for sidebar (not included)
}

// VoiceChannelStateView is a per-channel voice state returned in CrewState.
type VoiceChannelStateView struct {
	ID        string             `json:"id"`
	Name      string             `json:"name"`
	IsDefault bool               `json:"is_default"`
	Members   []*VoiceMemberInfo `json:"members"`
}

type CrewStreamState struct {
	Active            bool   `json:"active"`
	StreamID          string `json:"stream_id,omitempty"`
	StreamerID        string `json:"streamer_id,omitempty"`
	StreamerUsername  string `json:"streamer_username,omitempty"`
	Title             string `json:"title,omitempty"`
	ViewerCount       int    `json:"viewer_count,omitempty"`
	ThumbnailURL      string `json:"thumbnail_url,omitempty"`
	Width             uint32 `json:"width,omitempty"`
	Height            uint32 `json:"height,omitempty"`
	ThumbnailUpdatedAt string `json:"thumbnail_updated_at,omitempty"`
}

type MessagePreview struct {
	MessageID string `json:"message_id,omitempty"`
	UserID    string `json:"user_id,omitempty"`
	Username  string `json:"username"`
	Preview   string `json:"preview"`
	Timestamp string `json:"timestamp"`
}

type CrewMemberInfo struct {
	UserID   string        `json:"user_id"`
	Username string        `json:"username"`
	Avatar   string        `json:"avatar,omitempty"`
	Presence *UserPresence `json:"presence,omitempty"`
}

type CrewState struct {
	CrewID         string                   `json:"crew_id"`
	Name           string                   `json:"name"`
	Counts         CrewCounts               `json:"counts"`
	Members        []*CrewMemberInfo        `json:"members,omitempty"` // only for active crew (full view)
	Voice          *CrewVoiceState          `json:"voice"`
	VoiceChannels  []*VoiceChannelStateView `json:"voice_channels"`
	Stream         *CrewStreamState         `json:"stream"`
	RecentMessages []*MessagePreview        `json:"recent_messages"`
	UpdatedAt      string                   `json:"updated_at"`
	MyRole         int                      `json:"my_role"` // 0=superadmin, 1=admin, 2=member; set per-request
	SFUEnabled     bool                     `json:"sfu_enabled,omitempty"`
}

// CrewSidebarState is a lighter view for the sidebar.
type CrewSidebarState struct {
	CrewID         string                   `json:"crew_id"`
	Name           string                   `json:"name"`
	Counts         CrewCounts               `json:"counts"`
	Voice          *CrewVoiceState          `json:"voice"`
	VoiceChannels  []*VoiceChannelStateView `json:"voice_channels,omitempty"`
	Stream         *CrewStreamState         `json:"stream"`
	RecentMessages []*MessagePreview        `json:"recent_messages,omitempty"`
	Idle           bool                     `json:"idle,omitempty"`
	SFUEnabled     bool                     `json:"sfu_enabled,omitempty"`
}

// ---------------------------------------------------------------------------
// In-memory cache for crew state
// ---------------------------------------------------------------------------

var (
	crewStateCache   = make(map[string]*CrewState)
	crewStateCacheMu sync.RWMutex

	// Recent messages buffer per crew (last 2)
	crewRecentMsgs   = make(map[string][]*MessagePreview)
	crewRecentMsgsMu sync.RWMutex
)

// InvalidateCrewState marks a crew's cached state as stale (delete from cache).
func InvalidateCrewState(crewID string) {
	crewStateCacheMu.Lock()
	delete(crewStateCache, crewID)
	crewStateCacheMu.Unlock()
}

// ---------------------------------------------------------------------------
// State computation
// ---------------------------------------------------------------------------

// ComputeCrewState builds the full aggregate state for a crew.
// callerID is the requesting user's ID; when non-empty, MyRole is set on the result.
func ComputeCrewState(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID string, includeMembers bool, callerID string) (*CrewState, error) {
	// Check cache first (only for non-member requests, since member list is optional)
	if !includeMembers {
		crewStateCacheMu.RLock()
		cached, ok := crewStateCache[crewID]
		crewStateCacheMu.RUnlock()
		if ok {
			return cached, nil
		}
	}

	// Fetch group info
	groups, err := nk.GroupsGetId(ctx, []string{crewID})
	if err != nil || len(groups) == 0 {
		return nil, runtime.NewError("crew not found", 5)
	}
	group := groups[0]

	// Fetch members
	members, _, err := nk.GroupUsersList(ctx, crewID, 100, nil, "")
	if err != nil {
		return nil, runtime.NewError("failed to list crew members", 13)
	}

	// Count online and build member list
	onlineCount := 0
	callerRole := 2 // default to member if not found
	var memberInfos []*CrewMemberInfo
	if includeMembers {
		memberInfos = make([]*CrewMemberInfo, 0, len(members))
	}
	for _, m := range members {
		u := m.GetUser()
		if callerID != "" && u.GetId() == callerID {
			callerRole = int(m.GetState().GetValue())
		}
		p, _ := ReadPresence(ctx, nk, u.GetId())
		if p != nil && p.Status != StatusOffline {
			onlineCount++
		}
		if includeMembers {
			memberInfos = append(memberInfos, &CrewMemberInfo{
				UserID:   u.GetId(),
				Username: u.GetDisplayName(),
				Avatar:   u.GetAvatarUrl(),
				Presence: p,
			})
		}
	}

	// Voice state (legacy single-voice field for backward compat)
	voiceSnap := GetVoiceSnapshot(crewID)
	voiceState := &CrewVoiceState{
		Active:  voiceSnap.Active,
		Members: make([]*VoiceMemberInfo, 0, len(voiceSnap.Members)),
	}
	if voiceSnap.Active {
		voiceState.MemberIDs = voiceSnap.MemberIDs
		for _, vm := range voiceSnap.Members {
			speaking := vm.Speaking
			voiceState.Members = append(voiceState.Members, &VoiceMemberInfo{
				UserID:   vm.UserID,
				Username: vm.Username,
				Speaking: &speaking,
			})
		}
	}

	// Voice channels (new multi-channel state)
	channelDefs, _ := GetVoiceChannels(ctx, nk, crewID)
	var voiceChannels []*VoiceChannelStateView
	if channelDefs != nil && len(channelDefs.Channels) > 0 {
		voiceChannels = make([]*VoiceChannelStateView, 0, len(channelDefs.Channels))
		for _, ch := range channelDefs.Channels {
			chSnap := GetVoiceChannelSnapshot(ch.ID)
			chView := &VoiceChannelStateView{
				ID:        ch.ID,
				Name:      ch.Name,
				IsDefault: ch.IsDefault,
				Members:   make([]*VoiceMemberInfo, 0, len(chSnap.Members)),
			}
			for _, vm := range chSnap.Members {
				speaking := vm.Speaking
				chView.Members = append(chView.Members, &VoiceMemberInfo{
					UserID:   vm.UserID,
					Username: vm.Username,
					Speaking: &speaking,
				})
			}
			voiceChannels = append(voiceChannels, chView)
		}
	} else {
		voiceChannels = []*VoiceChannelStateView{}
	}

	// Stream state
	streamState := getActiveStreamForCrew(ctx, nk, crewID)

	// Recent messages
	crewRecentMsgsMu.RLock()
	recentMsgs := crewRecentMsgs[crewID]
	crewRecentMsgsMu.RUnlock()
	if recentMsgs == nil {
		recentMsgs = []*MessagePreview{}
	}

	state := &CrewState{
		CrewID: crewID,
		Name:   group.GetName(),
		Counts: CrewCounts{
			Online: onlineCount,
			Total:  len(members),
		},
		Members:        memberInfos,
		Voice:          voiceState,
		VoiceChannels:  voiceChannels,
		Stream:         streamState,
		RecentMessages: recentMsgs,
		UpdatedAt:      time.Now().UTC().Format(time.RFC3339),
		MyRole:         callerRole,
		SFUEnabled:     hasPremiumCrew(ctx, nk, crewID),
	}

	// Cache it (without members to keep cache light)
	if !includeMembers {
		crewStateCacheMu.Lock()
		crewStateCache[crewID] = state
		crewStateCacheMu.Unlock()
	}

	return state, nil
}

// getActiveStreamForCrew looks up the active stream for a crew from storage.
func getActiveStreamForCrew(ctx context.Context, nk runtime.NakamaModule, crewID string) *CrewStreamState {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{
			Collection: StreamMetaCollection,
			Key:        crewID,
			UserID:     SystemUserID,
		},
	})
	if err != nil || len(objects) == 0 {
		return &CrewStreamState{Active: false}
	}

	var meta StreamMeta
	if err := json.Unmarshal([]byte(objects[0].Value), &meta); err != nil {
		return &CrewStreamState{Active: false}
	}

	return &CrewStreamState{
		Active:            true,
		StreamID:          meta.StreamID,
		StreamerID:        meta.StreamerID,
		StreamerUsername:  meta.StreamerUsername,
		Title:             meta.Title,
		ViewerCount:       len(meta.ViewerIDs),
		ThumbnailURL:      meta.ThumbnailURL,
		ThumbnailUpdatedAt: meta.ThumbnailUpdatedAt,
		Width:             meta.Width,
		Height:            meta.Height,
	}
}

// ToSidebar converts a full CrewState to a sidebar summary.
func (cs *CrewState) ToSidebar() *CrewSidebarState {
	sidebar := &CrewSidebarState{
		CrewID:         cs.CrewID,
		Name:           cs.Name,
		Counts:         cs.Counts,
		Stream:         cs.Stream,
		RecentMessages: cs.RecentMessages,
		Idle:           cs.Counts.Online == 0,
		SFUEnabled:     cs.SFUEnabled,
	}
	// Sidebar voice: strip speaking state
	if cs.Voice != nil {
		sidebarVoice := &CrewVoiceState{
			Active:  cs.Voice.Active,
			Members: make([]*VoiceMemberInfo, len(cs.Voice.Members)),
		}
		for i, m := range cs.Voice.Members {
			sidebarVoice.Members[i] = &VoiceMemberInfo{
				UserID:   m.UserID,
				Username: m.Username,
			}
		}
		sidebar.Voice = sidebarVoice
	}
	// Sidebar voice channels: strip speaking state
	if len(cs.VoiceChannels) > 0 {
		sidebar.VoiceChannels = make([]*VoiceChannelStateView, len(cs.VoiceChannels))
		for i, ch := range cs.VoiceChannels {
			sidebarCh := &VoiceChannelStateView{
				ID:        ch.ID,
				Name:      ch.Name,
				IsDefault: ch.IsDefault,
				Members:   make([]*VoiceMemberInfo, len(ch.Members)),
			}
			for j, m := range ch.Members {
				sidebarCh.Members[j] = &VoiceMemberInfo{
					UserID:   m.UserID,
					Username: m.Username,
				}
			}
			sidebar.VoiceChannels[i] = sidebarCh
		}
	}
	return sidebar
}

// ---------------------------------------------------------------------------
// RPCs
// ---------------------------------------------------------------------------

func CrewStateGetRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID string `json:"crew_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	state, err := ComputeCrewState(ctx, logger, nk, req.CrewID, true, userID)
	if err != nil {
		return "", err
	}

	resp, _ := json.Marshal(state)
	return string(resp), nil
}

func CrewStateGetSidebarRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	_, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewIDs []string `json:"crew_ids"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	crews := make([]*CrewSidebarState, 0, len(req.CrewIDs))
	for _, cid := range req.CrewIDs {
		state, err := ComputeCrewState(ctx, logger, nk, cid, false, "")
		if err != nil {
			logger.Warn("failed to compute crew state for %s: %v", cid, err)
			continue
		}
		crews = append(crews, state.ToSidebar())
	}

	resp, _ := json.Marshal(map[string]interface{}{
		"crews": crews,
	})
	return string(resp), nil
}

// extractPreview pulls display text from a message envelope for sidebar previews.
func extractPreview(content string) string {
	var raw map[string]interface{}
	if err := json.Unmarshal([]byte(content), &raw); err != nil {
		return content
	}
	// Structured envelope: use "body"
	if body, ok := raw["body"].(string); ok {
		if _, hasV := raw["v"]; hasV {
			if msgType, _ := raw["type"].(string); msgType == "gif" && body == "" {
				return "[GIF]"
			}
			return body
		}
	}
	// Legacy format: use "text"
	if text, ok := raw["text"].(string); ok {
		return text
	}
	return content
}

// ---------------------------------------------------------------------------
// Chat message hook
// ---------------------------------------------------------------------------

func OnChatMessage(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, out, in *rtapi.Envelope) error {
	// Only handle ChannelMessageSend
	ack := out.GetChannelMessageAck()
	send := in.GetChannelMessageSend()
	if ack == nil || send == nil {
		return nil
	}

	crewID := ack.GetGroupId()
	if crewID == "" {
		return nil
	}

	content := send.GetContent()

	// Skip P2P signaling messages — they are not real chat.
	if strings.Contains(content, `"signal":true`) {
		return nil
	}

	// Extract preview text from the message envelope
	preview := extractPreview(content)
	if len(preview) > 60 {
		preview = preview[:57] + "..."
	}

	userID, _ := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	username := resolveUsername(ctx, nk, userID)

	msg := &MessagePreview{
		MessageID: ack.GetMessageId(),
		UserID:    userID,
		Username:  username,
		Preview:   preview,
		Timestamp: time.Now().UTC().Format(time.RFC3339),
	}

	// Buffer last 2 messages
	crewRecentMsgsMu.Lock()
	msgs := crewRecentMsgs[crewID]
	msgs = append(msgs, msg)
	if len(msgs) > 2 {
		msgs = msgs[len(msgs)-2:]
	}
	crewRecentMsgs[crewID] = msgs
	crewRecentMsgsMu.Unlock()

	InvalidateCrewState(crewID)

	// Queue message preview for sidebar throttle
	QueueMessagePreview(ctx, logger, nk, crewID, msg)

	// Track chat activity for event ledger aggregation
	incrementChatCounter(crewID, userID)

	// Update last-seen (user is actively chatting in this crew)
	updateLastSeen(ctx, nk, userID, crewID)

	return nil
}
