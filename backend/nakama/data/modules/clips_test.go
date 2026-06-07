package main

import "testing"

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
