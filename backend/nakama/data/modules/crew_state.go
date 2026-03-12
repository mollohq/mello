package main

import (
	"context"
	"database/sql"
	"encoding/json"
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

type CrewStreamState struct {
	Active            bool   `json:"active"`
	StreamID          string `json:"stream_id,omitempty"`
	StreamerID        string `json:"streamer_id,omitempty"`
	StreamerUsername  string `json:"streamer_username,omitempty"`
	Title             string `json:"title,omitempty"`
	ViewerCount       int    `json:"viewer_count,omitempty"`
	ThumbnailURL      string `json:"thumbnail_url,omitempty"`
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
	CrewID         string           `json:"crew_id"`
	Name           string           `json:"name"`
	Counts         CrewCounts       `json:"counts"`
	Members        []*CrewMemberInfo `json:"members,omitempty"` // only for active crew (full view)
	Voice          *CrewVoiceState  `json:"voice"`
	Stream         *CrewStreamState `json:"stream"`
	RecentMessages []*MessagePreview `json:"recent_messages"`
	UpdatedAt      string           `json:"updated_at"`
}

// CrewSidebarState is a lighter view for the sidebar.
type CrewSidebarState struct {
	CrewID         string            `json:"crew_id"`
	Name           string            `json:"name"`
	Counts         CrewCounts        `json:"counts"`
	Voice          *CrewVoiceState   `json:"voice"`
	Stream         *CrewStreamState  `json:"stream"`
	RecentMessages []*MessagePreview `json:"recent_messages,omitempty"`
	Idle           bool              `json:"idle,omitempty"`
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
func ComputeCrewState(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID string, includeMembers bool) (*CrewState, error) {
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
	var memberInfos []*CrewMemberInfo
	if includeMembers {
		memberInfos = make([]*CrewMemberInfo, 0, len(members))
	}
	for _, m := range members {
		u := m.GetUser()
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

	// Voice state
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
		Stream:         streamState,
		RecentMessages: recentMsgs,
		UpdatedAt:      time.Now().UTC().Format(time.RFC3339),
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

	state, err := ComputeCrewState(ctx, logger, nk, req.CrewID, true)
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
		state, err := ComputeCrewState(ctx, logger, nk, cid, false)
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

	preview := send.GetContent()
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

	return nil
}
