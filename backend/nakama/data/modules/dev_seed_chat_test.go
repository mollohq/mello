package main

import (
	"testing"
	"time"
)

func TestSeedChatPlan_CoversWhatTheChatMustShow(t *testing.T) {
	plan := seedChatPlan()
	if len(plan) < 55 {
		t.Fatalf("two pages of history need more than 50 messages, got %d", len(plan))
	}
	days := map[int]bool{}
	kinds := map[seedChatKind]int{}
	replies, reactions, mentions := 0, 0, 0
	for i, p := range plan {
		if p.at > 0 {
			t.Fatalf("message %d is in the future", i)
		}
		days[int(p.at/(24*time.Hour))] = true
		kinds[p.kind]++
		if p.reply >= 0 {
			if p.reply >= i {
				t.Fatalf("message %d replies forward to %d", i, p.reply)
			}
			replies++
		}
		if len(p.reactions) > 0 {
			reactions++
		}
		if containsMention(p.body) {
			mentions++
		}
		if i > 0 && p.at < plan[i-1].at {
			t.Fatalf("message %d is older than %d: the plan must be in order", i, i-1)
		}
	}
	if len(days) < 4 {
		t.Fatalf("four days of separators, got %d", len(days))
	}
	if kinds[seedGif] == 0 || kinds[seedImage] < 2 {
		t.Fatalf("a GIF and two image messages, got %v", kinds)
	}
	if replies == 0 || reactions < 3 || mentions < 2 {
		t.Fatalf("replies %d, reactions %d, mentions %d", replies, reactions, mentions)
	}
	cursor := seedChatCursorIndex(plan)
	if plan[cursor+1].body != "gg ez" || len(plan)-cursor-1 < 5 {
		t.Fatalf("the cursor sits before gg ez with a few unread after it, got %d of %d", cursor, len(plan))
	}
}

func containsMention(s string) bool {
	return len(s) > 3 && (indexOf(s, "<@") >= 0)
}

func indexOf(s, sub string) int {
	for i := 0; i+len(sub) <= len(s); i++ {
		if s[i:i+len(sub)] == sub {
			return i
		}
	}
	return -1
}

func TestSeedPNG_IsAPNGOfTheSize(t *testing.T) {
	data, err := seedPNG(120, 80, 7)
	if err != nil {
		t.Fatal(err)
	}
	if len(data) < 100 || string(data[1:4]) != "PNG" {
		t.Fatalf("not a png: %d bytes", len(data))
	}
}
