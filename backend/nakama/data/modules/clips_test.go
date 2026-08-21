package main

import (
	"encoding/base64"
	"encoding/json"
	"testing"
)

func clipWaveformBase64(t *testing.T, nbytes int) string {
	t.Helper()
	raw := make([]byte, nbytes)
	for i := range raw {
		raw[i] = byte(i)
	}
	return base64.StdEncoding.EncodeToString(raw)
}

func TestValidateClipWaveform_AcceptsEmpty(t *testing.T) {
	if errStr := validateClipWaveform(""); errStr != "" {
		t.Fatalf("empty waveform should be accepted, got %q", errStr)
	}
}

func TestValidateClipWaveform_Accepts64Bytes(t *testing.T) {
	data := clipWaveformBase64(t, clipWaveformBytes)
	if errStr := validateClipWaveform(data); errStr != "" {
		t.Fatalf("valid waveform rejected: %s", errStr)
	}
}

func TestValidateClipWaveform_RejectsBadBase64(t *testing.T) {
	if errStr := validateClipWaveform("not-base64!!"); errStr != "invalid waveform" {
		t.Fatalf("expected invalid waveform, got %q", errStr)
	}
}

func TestValidateClipWaveform_RejectsWrongLength(t *testing.T) {
	for _, n := range []int{1, 63, 65} {
		data := clipWaveformBase64(t, n)
		if errStr := validateClipWaveform(data); errStr != "waveform must be 64 bytes" {
			t.Fatalf("len %d: expected length rejection, got %q", n, errStr)
		}
	}
}

func TestStoredClipWaveformRoundTripsJSON(t *testing.T) {
	waveform := clipWaveformBase64(t, clipWaveformBytes)
	clip := StoredClip{
		EventID:  "evt_1",
		ClipID:   "clip_1",
		Waveform: waveform,
	}
	data, err := json.Marshal(clip)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	var got StoredClip
	if err := json.Unmarshal(data, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if got.Waveform != waveform {
		t.Fatalf("waveform not preserved: got %q want %q", got.Waveform, waveform)
	}
}

func TestCapClipsBelowCapIsNoOp(t *testing.T) {
	clips := []StoredClip{
		{EventID: "a", Ts: 1},
		{EventID: "b", Ts: 2},
		{EventID: "c", Ts: 3},
	}
	got := capClips(clips)
	if len(got) != 3 {
		t.Fatalf("expected 3 clips, got %d", len(got))
	}
}

func TestCapClipsKeepsMostRecent(t *testing.T) {
	total := CrewClipsMaxRetained + 50
	clips := make([]StoredClip, 0, total)
	// Append out of timestamp order to confirm the cap sorts by Ts.
	for i := 0; i < total; i++ {
		clips = append(clips, StoredClip{
			EventID: string(rune('A' + (i % 26))),
			Ts:      int64(total - i),
		})
	}

	got := capClips(clips)
	if len(got) != CrewClipsMaxRetained {
		t.Fatalf("expected %d clips after cap, got %d", CrewClipsMaxRetained, len(got))
	}

	// Result is sorted ascending by Ts; the oldest retained must be newer than
	// every dropped clip. The 50 oldest (Ts 1..50) should have been dropped.
	for _, c := range got {
		if c.Ts <= 50 {
			t.Fatalf("retained a clip that should have been trimmed: Ts=%d", c.Ts)
		}
	}
}
