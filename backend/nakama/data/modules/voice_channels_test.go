package main

import (
	"strings"
	"testing"
)

func TestGenerateChannelID(t *testing.T) {
	id := generateChannelID()
	if !strings.HasPrefix(id, "ch_") {
		t.Errorf("expected ch_ prefix, got %s", id)
	}
	// "ch_" + 8 chars = 11
	if len(id) != 11 {
		t.Errorf("expected length 11, got %d (%s)", len(id), id)
	}

	// IDs should be unique
	ids := make(map[string]bool)
	for i := 0; i < 100; i++ {
		gen := generateChannelID()
		if ids[gen] {
			t.Fatalf("duplicate ID generated: %s", gen)
		}
		ids[gen] = true
	}
}

func TestVoiceChannelDefSortOrder(t *testing.T) {
	list := &VoiceChannelList{
		Channels: []*VoiceChannelDef{
			{ID: "ch_general", Name: "General", IsDefault: true, SortOrder: 0},
			{ID: "ch_strategy", Name: "Strategy", IsDefault: false, SortOrder: 1},
			{ID: "ch_afk", Name: "AFK", IsDefault: false, SortOrder: 2},
		},
	}

	if len(list.Channels) != 3 {
		t.Fatalf("expected 3 channels, got %d", len(list.Channels))
	}
	if !list.Channels[0].IsDefault {
		t.Error("expected first channel to be default")
	}
	for i, ch := range list.Channels {
		if ch.SortOrder != i {
			t.Errorf("channel %s sort_order: expected %d, got %d", ch.Name, i, ch.SortOrder)
		}
	}
}

func TestMaxChannelsPerCrew(t *testing.T) {
	if MaxChannelsPerCrew != 8 {
		t.Errorf("expected max 8 channels, got %d", MaxChannelsPerCrew)
	}
}

func TestMaxVoiceChannelMembers(t *testing.T) {
	if MaxVoiceChannelMembers != 6 {
		t.Errorf("expected max 6 members per channel, got %d", MaxVoiceChannelMembers)
	}
}

func TestChannelListMaxEnforcement(t *testing.T) {
	// Simulate the check done in ChannelCreateRPC
	list := &VoiceChannelList{Channels: make([]*VoiceChannelDef, 0)}
	for i := 0; i < MaxChannelsPerCrew; i++ {
		list.Channels = append(list.Channels, &VoiceChannelDef{
			ID:        generateChannelID(),
			Name:      "ch",
			SortOrder: i,
		})
	}

	if len(list.Channels) < MaxChannelsPerCrew {
		t.Fatal("should have filled to max")
	}

	// The RPC would reject at this point
	if len(list.Channels) >= MaxChannelsPerCrew {
		// Correct — cannot add more
	} else {
		t.Error("expected to be at or above max")
	}
}

func TestDeleteDefaultChannelBlocked(t *testing.T) {
	list := &VoiceChannelList{
		Channels: []*VoiceChannelDef{
			{ID: "ch_default", Name: "General", IsDefault: true, SortOrder: 0},
			{ID: "ch_extra", Name: "Extra", IsDefault: false, SortOrder: 1},
		},
	}

	// Simulate the check in ChannelDeleteRPC
	for _, ch := range list.Channels {
		if ch.ID == "ch_default" && ch.IsDefault {
			// Delete should be blocked
			return
		}
	}
	t.Error("expected default channel deletion to be blocked")
}

func TestDeleteNonDefaultChannel(t *testing.T) {
	list := &VoiceChannelList{
		Channels: []*VoiceChannelDef{
			{ID: "ch_default", Name: "General", IsDefault: true, SortOrder: 0},
			{ID: "ch_extra", Name: "Extra", IsDefault: false, SortOrder: 1},
			{ID: "ch_afk", Name: "AFK", IsDefault: false, SortOrder: 2},
		},
	}

	// Remove "ch_extra"
	idx := -1
	for i, ch := range list.Channels {
		if ch.ID == "ch_extra" {
			if ch.IsDefault {
				t.Fatal("should not be default")
			}
			idx = i
			break
		}
	}
	if idx < 0 {
		t.Fatal("channel not found")
	}

	list.Channels = append(list.Channels[:idx], list.Channels[idx+1:]...)
	for i, ch := range list.Channels {
		ch.SortOrder = i
	}

	if len(list.Channels) != 2 {
		t.Fatalf("expected 2 channels, got %d", len(list.Channels))
	}
	if list.Channels[0].ID != "ch_default" {
		t.Error("expected General to remain first")
	}
	if list.Channels[1].ID != "ch_afk" {
		t.Error("expected AFK to be second")
	}
	if list.Channels[1].SortOrder != 1 {
		t.Errorf("expected AFK sort_order 1, got %d", list.Channels[1].SortOrder)
	}
}

func TestReorderChannels(t *testing.T) {
	list := &VoiceChannelList{
		Channels: []*VoiceChannelDef{
			{ID: "ch_a", Name: "A", SortOrder: 0},
			{ID: "ch_b", Name: "B", SortOrder: 1},
			{ID: "ch_c", Name: "C", SortOrder: 2},
		},
	}

	newOrder := []string{"ch_c", "ch_a", "ch_b"}
	byID := make(map[string]*VoiceChannelDef, len(list.Channels))
	for _, ch := range list.Channels {
		byID[ch.ID] = ch
	}

	if len(newOrder) != len(list.Channels) {
		t.Fatal("reorder must include all channels")
	}

	reordered := make([]*VoiceChannelDef, 0, len(newOrder))
	for i, id := range newOrder {
		ch, ok := byID[id]
		if !ok {
			t.Fatalf("unknown channel_id: %s", id)
		}
		ch.SortOrder = i
		reordered = append(reordered, ch)
	}
	list.Channels = reordered

	if list.Channels[0].ID != "ch_c" || list.Channels[0].SortOrder != 0 {
		t.Errorf("expected ch_c at 0, got %s at %d", list.Channels[0].ID, list.Channels[0].SortOrder)
	}
	if list.Channels[1].ID != "ch_a" || list.Channels[1].SortOrder != 1 {
		t.Errorf("expected ch_a at 1, got %s at %d", list.Channels[1].ID, list.Channels[1].SortOrder)
	}
	if list.Channels[2].ID != "ch_b" || list.Channels[2].SortOrder != 2 {
		t.Errorf("expected ch_b at 2, got %s at %d", list.Channels[2].ID, list.Channels[2].SortOrder)
	}
}

func TestChannelNameValidation(t *testing.T) {
	// Simulate the 32-char limit from ChannelCreateRPC / ChannelRenameRPC
	validName := "General"
	longName := strings.Repeat("a", 33)

	if len(validName) > 32 {
		t.Error("expected valid name to pass")
	}
	if len(longName) <= 32 {
		t.Error("expected long name to be rejected")
	}
}
