package main

import "testing"

func TestValidReactionEmoji(t *testing.T) {
	for _, ok := range []string{"🔥", "❤️", "👍🏽", "👨‍👩‍👧", "🇸🇪", "1️⃣"} {
		if !validReactionEmoji(ok) {
			t.Errorf("%q is an emoji", ok)
		}
	}
	for _, bad := range []string{"", "a", "lol", "🔥 ", "1", "🔥🔥🔥🔥🔥🔥🔥🔥🔥🔥🔥", "\n"} {
		if validReactionEmoji(bad) {
			t.Errorf("%q is not a reaction", bad)
		}
	}
}

func TestApplyReaction_AddsOnceRemovesAndDropsEmpty(t *testing.T) {
	r, changed := applyReaction(nil, "🔥", "u1", true)
	if !changed || len(r["🔥"]) != 1 {
		t.Fatalf("first add: %v %v", r, changed)
	}
	r, changed = applyReaction(r, "🔥", "u1", true)
	if changed {
		t.Fatal("a second add by the same user changes nothing")
	}
	r, changed = applyReaction(r, "🔥", "u2", true)
	if !changed || len(r["🔥"]) != 2 || r["🔥"][1] != "u2" {
		t.Fatalf("second user appends in order: %v", r)
	}
	r, changed = applyReaction(r, "🔥", "u1", false)
	if !changed || len(r["🔥"]) != 1 || r["🔥"][0] != "u2" {
		t.Fatalf("remove keeps the others: %v", r)
	}
	r, changed = applyReaction(r, "💀", "u9", false)
	if changed {
		t.Fatal("removing what is not there changes nothing")
	}
	r, _ = applyReaction(r, "🔥", "u2", false)
	if _, exists := r["🔥"]; exists {
		t.Fatalf("an emoji with no users left is dropped: %v", r)
	}
}

func TestApplyReaction_CapsDistinctEmoji(t *testing.T) {
	r := map[string][]string{}
	for i := 0; i < maxReactionsPerMessage; i++ {
		r, _ = applyReaction(r, string(rune(0x1F600+i)), "u1", true)
	}
	if _, changed := applyReaction(r, "🧊", "u1", true); changed {
		t.Fatal("a new emoji past the cap is refused")
	}
	if _, changed := applyReaction(r, "😀", "u2", true); !changed {
		t.Fatal("another user on an existing emoji still counts")
	}
}
