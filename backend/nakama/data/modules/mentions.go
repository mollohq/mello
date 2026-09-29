package main

import (
	"context"
	"regexp"

	"github.com/heroiclabs/nakama-common/runtime"
)

// mentionTokenRe matches a `<@user_id>` mention token (TEXT-CHAT §7). The id
// rules match mello-core `chat::mention_tokens`: not empty, no whitespace, no
// `<`, `>` or `@`.
var mentionTokenRe = regexp.MustCompile(`<@([^\s<>@]+)>`)

// unknownMentionName is shown for a mentioned user that cannot be resolved.
// A raw user id is never shown. Matches mello-core `UNKNOWN_MENTION_NAME`.
const unknownMentionName = "unknown"

// previewMaxRunes bounds a message preview (sidebar, message-preview push).
const previewMaxRunes = 60

// mentionedUserIDs returns the distinct user ids of the mention tokens in body,
// in order of first appearance.
func mentionedUserIDs(body string) []string {
	var ids []string
	seen := map[string]bool{}
	for _, m := range mentionTokenRe.FindAllStringSubmatch(body, -1) {
		if !seen[m[1]] {
			seen[m[1]] = true
			ids = append(ids, m[1])
		}
	}
	return ids
}

// resolveMentionTokensWith replaces each `<@user_id>` token with `@name`.
// lookup maps user ids to names; a missing or empty name shows as
// `@unknown`. lookup is not called when body has no tokens.
func resolveMentionTokensWith(body string, lookup func(ids []string) map[string]string) string {
	ids := mentionedUserIDs(body)
	if len(ids) == 0 {
		return body
	}
	names := lookup(ids)
	return mentionTokenRe.ReplaceAllStringFunc(body, func(token string) string {
		id := token[2 : len(token)-1]
		if name := names[id]; name != "" {
			return "@" + name
		}
		return "@" + unknownMentionName
	})
}

// resolveMentionTokens resolves mention tokens to display names with one
// UsersGetId call. A lookup failure shows every mention as `@unknown`.
func resolveMentionTokens(ctx context.Context, nk runtime.NakamaModule, body string) string {
	return resolveMentionTokensWith(body, func(ids []string) map[string]string {
		names := map[string]string{}
		users, err := nk.UsersGetId(ctx, ids, nil)
		if err != nil {
			return names
		}
		for _, u := range users {
			name := u.GetDisplayName()
			if name == "" {
				name = u.GetUsername()
			}
			names[u.GetId()] = name
		}
		return names
	})
}

// truncateRunes shortens s to at most max runes, ending with "..." when cut.
// It counts runes, not bytes, so a multi-byte character is never split.
func truncateRunes(s string, max int) string {
	r := []rune(s)
	if len(r) <= max {
		return s
	}
	return string(r[:max-3]) + "..."
}

// previewText is the short text shown for a message in previews: the body
// with mentions resolved, truncated to previewMaxRunes.
func previewText(ctx context.Context, nk runtime.NakamaModule, content string) string {
	return truncateRunes(resolveMentionTokens(ctx, nk, extractPreview(content)), previewMaxRunes)
}
