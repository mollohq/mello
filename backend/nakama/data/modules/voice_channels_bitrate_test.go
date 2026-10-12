package main

import (
	"context"
	"encoding/json"
	"testing"
)

// Channel bitrate: the top rung of the voice encoder ladder (spec 13
// section 3.1, spec 10 section 4.6).

func intPtr(v int) *int { return &v }

func TestValidateChannelBitrate(t *testing.T) {
	cases := []struct {
		name    string
		in      *int
		want    int
		wantErr bool
	}{
		{"absent is the default", nil, 64, false},
		{"lowest", intPtr(24), 24, false},
		{"highest", intPtr(96), 96, false},
		{"between rungs", intPtr(40), 40, false},
		{"below the range", intPtr(23), 0, true},
		{"above the range", intPtr(97), 0, true},
		{"zero", intPtr(0), 0, true},
		{"negative", intPtr(-64), 0, true},
	}
	for _, c := range cases {
		got, err := validateChannelBitrate(c.in)
		if c.wantErr {
			if err == nil {
				t.Errorf("%s: want an error, got %d", c.name, got)
			}
			continue
		}
		if err != nil || got != c.want {
			t.Errorf("%s: got %d, %v; want %d", c.name, got, err, c.want)
		}
	}
}

func TestStoredChannelWithoutBitrateReadsTheDefault(t *testing.T) {
	var def VoiceChannelDef
	if err := json.Unmarshal([]byte(`{"id":"ch_general","name":"General","is_default":true,"sort_order":0}`), &def); err != nil {
		t.Fatal(err)
	}
	if got := def.EffectiveBitrateKbps(); got != DefaultChannelBitrateKbps {
		t.Fatalf("legacy channel bitrate %d, want %d", got, DefaultChannelBitrateKbps)
	}
	def.BitrateKbps = 32
	if got := def.EffectiveBitrateKbps(); got != 32 {
		t.Fatalf("bitrate %d, want 32", got)
	}
}

// channelFixture is a crew with a default channel and one more, and a
// member who may manage channels.
func channelFixture(t *testing.T) *fakeGuestNk {
	t.Helper()
	nk := newFakeGuestNk()
	nk.addMember("crew_a", "user_admin", "Admin")
	nk.put(VoiceChannelCollection, "crew_a", SystemUserID, VoiceChannelList{
		Channels: []*VoiceChannelDef{
			{ID: "ch_general", Name: "General", IsDefault: true, SortOrder: 0},
			{ID: "ch_music", Name: "Music", SortOrder: 1, BitrateKbps: 96},
		},
	})
	return nk
}

func storedChannel(t *testing.T, nk *fakeGuestNk, id string) *VoiceChannelDef {
	t.Helper()
	list, err := GetVoiceChannels(ctxWithUser("user_admin"), nk, "crew_a")
	if err != nil {
		t.Fatal(err)
	}
	for _, ch := range list.Channels {
		if ch.ID == id {
			return ch
		}
	}
	t.Fatalf("channel %s not stored", id)
	return nil
}

func TestChannelUpdateRPCValidatesTheBitrate(t *testing.T) {
	nk := channelFixture(t)
	ctx := ctxWithUser("user_admin")
	rejects := []struct {
		payload string
		code    int
	}{
		{`{"crew_id":"crew_a","channel_id":"ch_general","bitrate_kbps":8}`, 3},
		{`{"crew_id":"crew_a","channel_id":"ch_general","bitrate_kbps":128}`, 3},
		{`{"crew_id":"crew_a","channel_id":"ch_general","bitrate_kbps":0}`, 3},
		{`{"crew_id":"crew_a","channel_id":"ch_general","bitrate_kbps":"64"}`, 3},
		{`{"crew_id":"crew_a","channel_id":"ch_general"}`, 3},
		{`{"crew_id":"crew_a","bitrate_kbps":48}`, 3},
		{`not json`, 3},
		{`{"crew_id":"crew_a","channel_id":"ch_nope","bitrate_kbps":48}`, 5},
	}
	for _, r := range rejects {
		_, err := ChannelUpdateRPC(ctx, testLogger(), nil, nk, r.payload)
		if errCode(err) != r.code {
			t.Errorf("%s: error %v (code %d), want code %d", r.payload, err, errCode(err), r.code)
		}
	}
	if got := storedChannel(t, nk, "ch_general").EffectiveBitrateKbps(); got != 64 {
		t.Fatalf("a rejected update changed the bitrate to %d", got)
	}

	out, err := ChannelUpdateRPC(ctx, testLogger(), nil, nk,
		`{"crew_id":"crew_a","channel_id":"ch_general","bitrate_kbps":48}`)
	if err != nil {
		t.Fatalf("valid update: %v", err)
	}
	var resp struct {
		ChannelID   string `json:"channel_id"`
		BitrateKbps int    `json:"bitrate_kbps"`
	}
	if err := json.Unmarshal([]byte(out), &resp); err != nil {
		t.Fatal(err)
	}
	if resp.ChannelID != "ch_general" || resp.BitrateKbps != 48 {
		t.Fatalf("response %s", out)
	}
	if got := storedChannel(t, nk, "ch_general").BitrateKbps; got != 48 {
		t.Fatalf("stored bitrate %d, want 48", got)
	}
	if got := storedChannel(t, nk, "ch_music").BitrateKbps; got != 96 {
		t.Fatalf("the other channel changed to %d", got)
	}
}

func TestChannelUpdateRPCNeedsTheManagePermission(t *testing.T) {
	nk := channelFixture(t)
	_, err := ChannelUpdateRPC(ctxWithUser("user_outsider"), testLogger(), nil, nk,
		`{"crew_id":"crew_a","channel_id":"ch_general","bitrate_kbps":48}`)
	if errCode(err) != 7 {
		t.Fatalf("outsider: %v, want code 7", err)
	}
	_, err = ChannelUpdateRPC(context.Background(), testLogger(), nil, nk,
		`{"crew_id":"crew_a","channel_id":"ch_general","bitrate_kbps":48}`)
	if errCode(err) != 16 {
		t.Fatalf("no user: %v, want code 16", err)
	}
}

func TestChannelCreateRPCTakesAnOptionalBitrate(t *testing.T) {
	nk := channelFixture(t)
	ctx := ctxWithUser("user_admin")

	if _, err := ChannelCreateRPC(ctx, testLogger(), nil, nk,
		`{"crew_id":"crew_a","name":"Loud","bitrate_kbps":200}`); errCode(err) != 3 {
		t.Fatalf("bitrate 200: %v, want code 3", err)
	}

	out, err := ChannelCreateRPC(ctx, testLogger(), nil, nk, `{"crew_id":"crew_a","name":"Quiet","bitrate_kbps":32}`)
	if err != nil {
		t.Fatal(err)
	}
	var resp struct {
		ID          string `json:"id"`
		BitrateKbps int    `json:"bitrate_kbps"`
	}
	if err := json.Unmarshal([]byte(out), &resp); err != nil {
		t.Fatal(err)
	}
	if resp.BitrateKbps != 32 || storedChannel(t, nk, resp.ID).BitrateKbps != 32 {
		t.Fatalf("created %s, stored %d", out, storedChannel(t, nk, resp.ID).BitrateKbps)
	}

	out, err = ChannelCreateRPC(ctx, testLogger(), nil, nk, `{"crew_id":"crew_a","name":"Plain"}`)
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal([]byte(out), &resp); err != nil {
		t.Fatal(err)
	}
	if resp.BitrateKbps != 64 || storedChannel(t, nk, resp.ID).EffectiveBitrateKbps() != 64 {
		t.Fatalf("default create %s", out)
	}
}

func TestChannelStateViewCarriesTheBitrate(t *testing.T) {
	v := newVoiceChannelStateView(&VoiceChannelDef{ID: "ch_a", Name: "A", BitrateKbps: 96})
	data, err := json.Marshal(v)
	if err != nil {
		t.Fatal(err)
	}
	var got map[string]interface{}
	if err := json.Unmarshal(data, &got); err != nil {
		t.Fatal(err)
	}
	if got["bitrate_kbps"] != float64(96) {
		t.Fatalf("view %s", data)
	}
	legacy := newVoiceChannelStateView(&VoiceChannelDef{ID: "ch_b", Name: "B"})
	if legacy.BitrateKbps != DefaultChannelBitrateKbps {
		t.Fatalf("legacy view bitrate %d", legacy.BitrateKbps)
	}
}
