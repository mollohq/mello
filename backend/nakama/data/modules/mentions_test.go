package main

import (
	"reflect"
	"testing"
)

func TestMentionedUserIDsDistinctInOrder(t *testing.T) {
	got := mentionedUserIDs("<@u2> hi <@u1> and <@u2>, not <@ bad> or <@>")
	want := []string{"u2", "u1"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("mentionedUserIDs = %v, want %v", got, want)
	}
}

func TestResolveMentionTokensShowsNamesNeverIDs(t *testing.T) {
	lookup := func(ids []string) map[string]string {
		return map[string]string{"u1": "Åsa", "u2": ""}
	}
	got := resolveMentionTokensWith("hej <@u1>, <@u2> och <@u3>", lookup)
	want := "hej @Åsa, @unknown och @unknown"
	if got != want {
		t.Fatalf("resolve = %q, want %q", got, want)
	}
}

func TestResolveMentionTokensSkipsLookupWithoutTokens(t *testing.T) {
	called := false
	got := resolveMentionTokensWith("plain text", func([]string) map[string]string {
		called = true
		return nil
	})
	if got != "plain text" || called {
		t.Fatalf("got %q, lookup called = %v", got, called)
	}
}

func TestTruncateRunesNeverSplitsACharacter(t *testing.T) {
	s := "åäöååååååååå" // 12 runes, 24 bytes
	got := truncateRunes(s, 8)
	if got != "åäöåå..." {
		t.Fatalf("truncateRunes = %q", got)
	}
	if truncateRunes("short", 8) != "short" {
		t.Fatal("short text must be unchanged")
	}
}
