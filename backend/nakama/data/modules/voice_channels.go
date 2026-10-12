package main

import (
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/json"
	"fmt"
	"math/big"

	"github.com/heroiclabs/nakama-common/runtime"
)

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const (
	VoiceChannelCollection    = "voice_channels"
	MaxChannelsPerCrew        = 8
	MaxVoiceChannelMembers    = 6
	MaxSFUVoiceChannelMembers = 50

	// Voice bitrate of a channel in kbps: the top rung of the encoder ladder
	// (spec 10 section 4.6). 96 is the highest rung for now.
	DefaultChannelBitrateKbps = 64
	MinChannelBitrateKbps     = 24
	MaxChannelBitrateKbps     = 96
)

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type VoiceChannelDef struct {
	ID        string `json:"id"`
	Name      string `json:"name"`
	IsDefault bool   `json:"is_default"`
	SortOrder int    `json:"sort_order"`
	// BitrateKbps is the channel's voice bitrate in kbps: the top rung of
	// the encoder ladder (spec 10 section 4.6, spec 13 section 3.1). 0 in a
	// definition stored before the field existed: read it through
	// EffectiveBitrateKbps.
	BitrateKbps int `json:"bitrate_kbps,omitempty"`
}

// EffectiveBitrateKbps is the channel bitrate, or the default for a channel
// stored before the field existed.
func (c *VoiceChannelDef) EffectiveBitrateKbps() int {
	if c.BitrateKbps == 0 {
		return DefaultChannelBitrateKbps
	}
	return c.BitrateKbps
}

// validateChannelBitrate checks a requested channel bitrate. Absent (nil)
// is the default. A present value must be within MinChannelBitrateKbps to
// MaxChannelBitrateKbps.
func validateChannelBitrate(kbps *int) (int, error) {
	if kbps == nil {
		return DefaultChannelBitrateKbps, nil
	}
	if *kbps < MinChannelBitrateKbps || *kbps > MaxChannelBitrateKbps {
		return 0, runtime.NewError(fmt.Sprintf("bitrate_kbps must be %d to %d",
			MinChannelBitrateKbps, MaxChannelBitrateKbps), 3)
	}
	return *kbps, nil
}

// newVoiceChannelStateView is the per-channel view of a channel definition,
// without members.
func newVoiceChannelStateView(ch *VoiceChannelDef) *VoiceChannelStateView {
	return &VoiceChannelStateView{
		ID:          ch.ID,
		Name:        ch.Name,
		IsDefault:   ch.IsDefault,
		BitrateKbps: ch.EffectiveBitrateKbps(),
	}
}

type VoiceChannelList struct {
	Channels []*VoiceChannelDef `json:"channels"`
}

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

func GetVoiceChannels(ctx context.Context, nk runtime.NakamaModule, crewID string) (*VoiceChannelList, error) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{
			Collection: VoiceChannelCollection,
			Key:        crewID,
			UserID:     SystemUserID,
		},
	})
	if err != nil {
		return nil, err
	}
	if len(objects) == 0 {
		// Lazy-init: create a default "General" channel for crews that predate voice channels
		if err := InitDefaultChannel(ctx, nk, crewID); err != nil {
			return &VoiceChannelList{Channels: []*VoiceChannelDef{}}, nil
		}
		// Read back what we just wrote (no recursion — if still empty, return empty)
		objects2, err := nk.StorageRead(ctx, []*runtime.StorageRead{
			{Collection: VoiceChannelCollection, Key: crewID, UserID: SystemUserID},
		})
		if err != nil || len(objects2) == 0 {
			return &VoiceChannelList{Channels: []*VoiceChannelDef{}}, nil
		}
		var list VoiceChannelList
		if err := json.Unmarshal([]byte(objects2[0].Value), &list); err != nil {
			return nil, err
		}
		return &list, nil
	}

	var list VoiceChannelList
	if err := json.Unmarshal([]byte(objects[0].Value), &list); err != nil {
		return nil, err
	}
	return &list, nil
}

func saveVoiceChannels(ctx context.Context, nk runtime.NakamaModule, crewID string, list *VoiceChannelList) error {
	data, _ := json.Marshal(list)
	_, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      VoiceChannelCollection,
			Key:             crewID,
			UserID:          SystemUserID,
			Value:           string(data),
			PermissionRead:  2,
			PermissionWrite: 0,
		},
	})
	return err
}

// InitDefaultChannel creates the "General" channel for a newly created crew.
func InitDefaultChannel(ctx context.Context, nk runtime.NakamaModule, crewID string) error {
	list := &VoiceChannelList{
		Channels: []*VoiceChannelDef{
			{
				ID:        generateChannelID(),
				Name:      "General",
				IsDefault: true,
				SortOrder: 0,
			},
		},
	}
	return saveVoiceChannels(ctx, nk, crewID, list)
}

// ---------------------------------------------------------------------------
// ID generation
// ---------------------------------------------------------------------------

const channelIDChars = "abcdefghijklmnopqrstuvwxyz0123456789"

func generateChannelID() string {
	b := make([]byte, 8)
	for i := range b {
		n, _ := rand.Int(rand.Reader, big.NewInt(int64(len(channelIDChars))))
		b[i] = channelIDChars[n.Int64()]
	}
	return "ch_" + string(b)
}

// resolveChannelName looks up a channel's display name from storage.
func resolveChannelName(ctx context.Context, nk runtime.NakamaModule, crewID, channelID string) string {
	list, err := GetVoiceChannels(ctx, nk, crewID)
	if err != nil || list == nil {
		return ""
	}
	for _, ch := range list.Channels {
		if ch.ID == channelID {
			return ch.Name
		}
	}
	return ""
}

// ---------------------------------------------------------------------------
// Permission check
// ---------------------------------------------------------------------------

func canManageChannels(ctx context.Context, nk runtime.NakamaModule, crewID, userID string) bool {
	// The group creator (superadmin = state 0) can always manage channels.
	members, _, err := nk.GroupUsersList(ctx, crewID, 100, nil, "")
	if err != nil {
		return false
	}
	for _, m := range members {
		if m.GetUser().GetId() == userID {
			// state: 0 = superadmin (creator), 1 = admin, 2 = member
			return m.GetState().GetValue() <= 1
		}
	}
	return false
}

// ---------------------------------------------------------------------------
// RPCs
// ---------------------------------------------------------------------------

func ChannelCreateRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID string `json:"crew_id"`
		Name   string `json:"name"`
		// BitrateKbps is optional; absent is DefaultChannelBitrateKbps.
		BitrateKbps *int `json:"bitrate_kbps"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" || req.Name == "" {
		return "", runtime.NewError("crew_id and name required", 3)
	}
	if len(req.Name) > 32 {
		return "", runtime.NewError("name must be 32 characters or fewer", 3)
	}
	bitrate, err := validateChannelBitrate(req.BitrateKbps)
	if err != nil {
		return "", err
	}

	if !canManageChannels(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not authorized to manage channels", 7)
	}

	list, err := GetVoiceChannels(ctx, nk, req.CrewID)
	if err != nil {
		return "", runtime.NewError("failed to load channels", 13)
	}
	if len(list.Channels) >= MaxChannelsPerCrew {
		return "", runtime.NewError(fmt.Sprintf("maximum %d channels reached", MaxChannelsPerCrew), 9)
	}

	ch := &VoiceChannelDef{
		ID:          generateChannelID(),
		Name:        req.Name,
		IsDefault:   false,
		SortOrder:   len(list.Channels),
		BitrateKbps: bitrate,
	}
	list.Channels = append(list.Channels, ch)

	if err := saveVoiceChannels(ctx, nk, req.CrewID, list); err != nil {
		return "", runtime.NewError("failed to save channels", 13)
	}

	InvalidateCrewState(req.CrewID)

	PushCrewEvent(ctx, logger, nk, req.CrewID, "channel_created", map[string]interface{}{
		"channel_id":   ch.ID,
		"channel_name": ch.Name,
	})

	resp, _ := json.Marshal(map[string]interface{}{
		"id":           ch.ID,
		"name":         ch.Name,
		"is_default":   ch.IsDefault,
		"bitrate_kbps": ch.EffectiveBitrateKbps(),
		"members":      []interface{}{},
	})
	return string(resp), nil
}

func ChannelRenameRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID    string `json:"crew_id"`
		ChannelID string `json:"channel_id"`
		Name      string `json:"name"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" || req.ChannelID == "" || req.Name == "" {
		return "", runtime.NewError("crew_id, channel_id and name required", 3)
	}
	if len(req.Name) > 32 {
		return "", runtime.NewError("name must be 32 characters or fewer", 3)
	}

	if !canManageChannels(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not authorized to manage channels", 7)
	}

	list, err := GetVoiceChannels(ctx, nk, req.CrewID)
	if err != nil {
		return "", runtime.NewError("failed to load channels", 13)
	}

	found := false
	for _, ch := range list.Channels {
		if ch.ID == req.ChannelID {
			ch.Name = req.Name
			found = true
			break
		}
	}
	if !found {
		return "", runtime.NewError("channel not found", 5)
	}

	if err := saveVoiceChannels(ctx, nk, req.CrewID, list); err != nil {
		return "", runtime.NewError("failed to save channels", 13)
	}

	InvalidateCrewState(req.CrewID)

	PushCrewEvent(ctx, logger, nk, req.CrewID, "channel_renamed", map[string]interface{}{
		"channel_id":   req.ChannelID,
		"channel_name": req.Name,
	})

	return `{"success":true}`, nil
}

func ChannelDeleteRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID    string `json:"crew_id"`
		ChannelID string `json:"channel_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" || req.ChannelID == "" {
		return "", runtime.NewError("crew_id and channel_id required", 3)
	}

	if !canManageChannels(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not authorized to manage channels", 7)
	}

	list, err := GetVoiceChannels(ctx, nk, req.CrewID)
	if err != nil {
		return "", runtime.NewError("failed to load channels", 13)
	}

	idx := -1
	for i, ch := range list.Channels {
		if ch.ID == req.ChannelID {
			if ch.IsDefault {
				return "", runtime.NewError("cannot delete the default channel", 9)
			}
			idx = i
			break
		}
	}
	if idx < 0 {
		return "", runtime.NewError("channel not found", 5)
	}

	// Remove from list and fix sort orders
	list.Channels = append(list.Channels[:idx], list.Channels[idx+1:]...)
	for i, ch := range list.Channels {
		ch.SortOrder = i
	}

	if err := saveVoiceChannels(ctx, nk, req.CrewID, list); err != nil {
		return "", runtime.NewError("failed to save channels", 13)
	}

	// Kick users from the deleted channel
	VoiceEvictChannel(ctx, logger, nk, req.ChannelID)

	InvalidateCrewState(req.CrewID)

	PushCrewEvent(ctx, logger, nk, req.CrewID, "channel_deleted", map[string]interface{}{
		"channel_id": req.ChannelID,
	})

	return `{"success":true}`, nil
}

func ChannelReorderRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID     string   `json:"crew_id"`
		ChannelIDs []string `json:"channel_ids"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" || len(req.ChannelIDs) == 0 {
		return "", runtime.NewError("crew_id and channel_ids required", 3)
	}

	if !canManageChannels(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not authorized to manage channels", 7)
	}

	list, err := GetVoiceChannels(ctx, nk, req.CrewID)
	if err != nil {
		return "", runtime.NewError("failed to load channels", 13)
	}

	// Build index
	byID := make(map[string]*VoiceChannelDef, len(list.Channels))
	for _, ch := range list.Channels {
		byID[ch.ID] = ch
	}

	if len(req.ChannelIDs) != len(list.Channels) {
		return "", runtime.NewError("channel_ids must contain all channels", 3)
	}

	reordered := make([]*VoiceChannelDef, 0, len(req.ChannelIDs))
	for i, id := range req.ChannelIDs {
		ch, ok := byID[id]
		if !ok {
			return "", runtime.NewError("unknown channel_id: "+id, 3)
		}
		ch.SortOrder = i
		reordered = append(reordered, ch)
	}
	list.Channels = reordered

	if err := saveVoiceChannels(ctx, nk, req.CrewID, list); err != nil {
		return "", runtime.NewError("failed to save channels", 13)
	}

	InvalidateCrewState(req.CrewID)

	return `{"success":true}`, nil
}

// ChannelUpdateRPC changes the settings of one voice channel. Today that is
// the bitrate (spec 13 section 4.1): every member's encoder takes it as the
// top rung of its ladder (spec 10 section 4.6).
func ChannelUpdateRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req struct {
		CrewID      string `json:"crew_id"`
		ChannelID   string `json:"channel_id"`
		BitrateKbps *int   `json:"bitrate_kbps"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" || req.ChannelID == "" || req.BitrateKbps == nil {
		return "", runtime.NewError("crew_id, channel_id and bitrate_kbps required", 3)
	}
	bitrate, err := validateChannelBitrate(req.BitrateKbps)
	if err != nil {
		return "", err
	}

	if !canManageChannels(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not authorized to manage channels", 7)
	}

	list, err := GetVoiceChannels(ctx, nk, req.CrewID)
	if err != nil {
		return "", runtime.NewError("failed to load channels", 13)
	}
	var updated *VoiceChannelDef
	for _, ch := range list.Channels {
		if ch.ID == req.ChannelID {
			ch.BitrateKbps = bitrate
			updated = ch
			break
		}
	}
	if updated == nil {
		return "", runtime.NewError("channel not found", 5)
	}

	if err := saveVoiceChannels(ctx, nk, req.CrewID, list); err != nil {
		return "", runtime.NewError("failed to save channels", 13)
	}
	logger.Info("Voice channel updated: crew=%s channel=%s bitrate=%d kbps by user=%s",
		req.CrewID, req.ChannelID, bitrate, userID)

	InvalidateCrewState(req.CrewID)

	PushCrewEvent(ctx, logger, nk, req.CrewID, "channel_updated", map[string]interface{}{
		"channel_id":   req.ChannelID,
		"bitrate_kbps": bitrate,
	})
	// The channel list in voice_update carries the bitrate: members in the
	// channel apply it from there (spec 13 section 4.4).
	PushVoiceUpdate(ctx, logger, nk, req.CrewID)

	resp, _ := json.Marshal(map[string]interface{}{
		"success":      true,
		"channel_id":   req.ChannelID,
		"bitrate_kbps": bitrate,
	})
	return string(resp), nil
}
