# MELLO Text Chat Specification

> **Component:** Text Chat (Rich Messaging)  
> **Version:** 0.2  
> **Status:** Shipped with chat v2 (see `plans/CHAT-V2.md` for the build log)  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)  
> **Related:** [01-CLIENT.md](./01-CLIENT.md), [02-MELLO-CORE.md](./02-MELLO-CORE.md), [04-BACKEND.md](./04-BACKEND.md), [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md)

---

## 1. Overview

Text chat is crew-scoped. Each crew has one text channel using the existing
Nakama channel `crew.{crew_id}`. All messages use a structured JSON envelope
as the Nakama message `content` field. No DMs in beta.

### Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Message format | Structured JSON envelope with `v` field | Forward-compatible, typed, extensible |
| Formatting | Markdown-lite (Discord-style syntax) | Users already know it, fast to implement |
| GIF provider | Giphy API v1, client-side | Tenor's API closed 30 June 2026. Giphy's rules require client-side calls and forbid a proxy or a cache. |
| Emoji | Unicode, Twemoji bundled, full set in the picker | One look on every OS; the bundled COLRv0 font keeps the system emoji font out of memory. Each crew also has up to five crew emoji (8.1). |
| Images | Up to four per message, resized to 1600px, on the `mello-media` bucket | See 2.3 |
| Reactions | Server-side storage object per message | See 2.4 |
| Replies | Quote-reply with `reply_to` field | Simple, no threads |
| DMs | Not in beta | Crew chat only |
| Unread tracking | A read cursor per crew in the user's storage | Survives a restart. See 2.5 |
| Message length | 2000 characters max | Enforced client-side and server-side |
| Message history | Pages on scroll-up, a window of the newest 200 on screen | Nakama cursor-based pagination. See 4.3 |

### What Changes

| Layer | Before | After |
|-------|--------|-------|
| Message content | Plain text string | Structured JSON envelope (see §2) |
| Client rendering | Flat text, raw ISO timestamps | Grouped messages, relative time, markdown, inline GIFs |
| Chat input | Single text field in panel | Text field with a send button + icon buttons (Attach, GIF, Emoji) |
| Backend | No message validation | Before-hook validates envelope, enforces length limit |

### What Doesn't Change

- Nakama channel messaging transport (`channel_message_send`, `channel_message_list`)
- One text channel per crew
- Chat panel position (right sidebar)
- Message persistence (handled by Nakama)

---

## 2. Message Envelope

Every message sent via `channel_message_send` uses this JSON structure as the
`content` field.

### 2.1 Schema

```json
{
  "v": 1,
  "type": "text | gif | image | system",
  "body": "string",
  "reply_to": "message_id | null",
  "mentions": ["user_id", "..."],
  "gif": { ... }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `v` | `integer` | Yes | Schema version. Always `1` for now. |
| `type` | `string` | Yes | One of: `text`, `gif`, `image`, `system`. |
| `body` | `string` | Yes | Message text. May contain markdown and mention tokens. Max 2000 chars. Empty string for gif-only messages. |
| `reply_to` | `string \| null` | No | Nakama message ID of the quoted message. `null` or omitted if not a reply. |
| `mentions` | `string[]` | No | Array of user IDs mentioned in `body`. Used for notification routing. Omit if empty. |
| `gif` | `object \| null` | No | GIF metadata. Required when `type` is `gif`. See §2.2. |
| `images` | `array` | No | Uploaded images. Required when `type` is `image`. See §2.3. |

### 2.2 GIF Object
```json
{
  "id": "abc123",
  "url": "https://media.giphy.com/.../200w.gif",
  "preview": "https://media.giphy.com/.../100w.gif",
  "width": 200,
  "height": 150,
  "alt": "title"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | `string` | Giphy content id. |
| `url` | `string` | The `fixed_width` rendition (200px wide), what the chat shows. |
| `preview` | `string` | The `fixed_width_small` rendition (100px wide), what the picker tiles show. |
| `width` | `integer` | Width of `url` in pixels. |
| `height` | `integer` | Height of `url` in pixels. |
| `alt` | `string` | The Giphy title. Used as accessible description. |

### 2.3 Image Message

`type: "image"` carries up to four uploaded images and an optional caption
in `body`. Each entry:

```json
{ "url": "https://media.m3llo.app/{crew_id}/{user_id}/{id}.jpg",
  "width": 1600, "height": 900, "bytes": 118000, "mime": "image/jpeg" }
```

| Rule | Value |
|---|---|
| Images per message | 1 to 4 |
| Long side | 1600px. The client resizes before upload. |
| Size | 8 MB after encode. The server refuses more. |
| Types | `image/jpeg`, `image/png` (kept when the source PNG has transparency), `image/gif` (bytes kept as they are) |
| EXIF | Dropped by the re-encode. Orientation is applied first. |
| Host | The chat bucket's public base (`MEDIA_S3_PUBLIC_URL`). The server refuses any other host. |

Upload: the client calls `chat_upload_url` (crew, mime, bytes) for a presigned
PUT under `{crew_id}/{user_id}/{id}.{ext}`, PUTs the bytes, then sends the
message. The uploader's id is in the key, so `chat_delete_images` accepts only
the caller's own keys; the client calls it when the sender deletes the message.
The bucket is `mello-media`, apart from clips (see 04-BACKEND.md).

Composer: the Attach pill opens a file dialog (up to four), Cmd/Ctrl+V with an
image on the clipboard adds a chip, and a chip shows preparing, uploading, the
size when ready, or the reason it failed. Enter or the send button sends when
every upload has landed. Drop from another app is not supported: the winit
backend delivers no file drops.

Feed: images render under the body, each at most 300px wide with one cut
corner and the size tag. A click opens the viewer over the panel with Save,
Copy and Close; Escape closes.

### 2.4 Reactions

Reactions are not messages. One storage object per message
(`chat_reactions/{message_id}`, system-owned, read by all crew members) holds
emoji to the user ids that reacted, in order. `chat_react` (crew, message,
emoji, add or remove) updates it and notifies every crew member (code 117)
with the message's reactions; the client replaces what it holds for that
message. On a load, `chat_reactions` returns the objects of the loaded window
in one call. At most 20 distinct emoji per message; text is refused, except
a crew emoji of the crew as `:name:` (8.1). `chat_react` checks that the crew
emoji exists on add, not on remove.

Client: a reaction row under the message with emoji and count, the user's own
chip with the white outline, and a plus that opens the picker for one more.
The hover row's React opens the picker in reaction mode: the pick becomes a
reaction, the picker closes, the field keeps its text. A click on a chip
toggles the user's own reaction.

### 2.5 Read cursor

Where the user stopped reading each crew lives in the user's own storage,
`chat_read/{crew_id}` = `{message_id, time}`. The client stores the newest
message when the feed reaches the bottom, once per message. On open, the feed
starts at the unread rule when the cursor is behind the newest message, else
at the bottom. The sidebar badge of another crew counts its message previews
newer than the cursor, the user's own messages excluded; a crew with no
cursor counts every preview by others.

### 2.6 Day separators

A message that starts a new local day carries a label: Today, Yesterday, the
weekday with the date inside a week, then the date, and the date with the year
before this year. The row draws it as a graphite rule with the label register.
A day change starts a new group.

### 2.7 System Message

System messages are sent by the server (not users) for crew events.

```json
{
  "v": 1,
  "type": "system",
  "event": "stream_started | stream_ended | member_joined | member_left",
  "body": "bobdev started streaming",
  "data": { "user_id": "...", "stream_title": "..." }
}
```

The `body` is pre-formatted display text. The `data` object carries structured
fields specific to the event type for client rendering (e.g., linking to a user
profile).

---

## 3. Markdown Formatting

Discord-style markdown-lite. Parsed client-side at render time. Never stored
as rich text; the `body` field always contains the raw markdown source.

### 3.1 Supported Syntax

| Syntax | Renders as | Notes |
|--------|-----------|-------|
| `*text*` or `_text_` | *italic* | Single delimiter |
| `**text**` | **bold** | Double asterisk only |
| `***text***` | ***bold italic*** | Triple asterisk |
| `` `code` `` | `inline code` | Monospace, subtle background |
| ` ```code``` ` | Code block | Multi-line, monospace, background, no language highlighting in beta |
| `~~text~~` | ~~strikethrough~~ | Double tilde |
| `https://...` or `http://...` | Clickable link | Auto-detected, no markdown link syntax needed |

### 3.2 Rendering Strategy: Slint `StyledText` (1.16+)

Slint 1.16 introduces the `StyledText` element with native `@markdown()`
support (see https://github.com/slint-ui/slint/issues/9560). This provides
built-in rendering for bold, italic, underline, hyperlinks, lists, and text
colors, which covers everything we need.

**Target approach:** Use `StyledText` with `@markdown()` for all message
body rendering. The raw markdown `body` from the message envelope is passed
directly to the Slint element. No custom parsing or span layout needed.

```slint
// Message body rendering (once Slint 1.16 is available)
StyledText {
    styled-text: @markdown(body);
    font-size: Theme.font-md;
    color: Theme.text-primary;
    link-clicked(url) => { open-url(url); }
}
```

**Before Slint 1.16 is available:** Render message bodies as plain `Text`
elements with no formatting. The raw markdown source is readable as-is
(`*bold*` just shows the asterisks). This is acceptable for beta. All other
chat features (grouping, GIFs, replies, edit/delete) are independent of
markdown rendering and can ship first.

**Upgrade path:** When Slint 1.16 releases (or when pinning to master is
deemed stable enough), replace `Text { text: body; }` with the `StyledText`
snippet above in the message component. Single-element swap, no structural
changes.

### 3.3 Pre-processing Before Render

Even with `StyledText`, two things require pre-processing in mello-core
before the body reaches the Slint layer:

1. **Mention tokens:** Replace `<@user_id>` with `**@display_name**` so
   that mentions render as bold text. Resolve display names from the crew
   member list. Unknown users render as `**@unknown**`.

2. **URL auto-detection:** Standard markdown `@markdown()` handles `[text](url)`
   links. For bare URLs (`https://example.com`), wrap them as
   `[https://example.com](https://example.com)` before passing to
   `@markdown()`. Match pattern: `https?://[^\s<>]+`, trimming trailing
   punctuation (`.`, `,`, `)`, `!`, `?`) unless part of balanced parens.

This pre-processing happens in mello-core's `prepare_body_for_display()`
function, which takes the raw `body` and crew member map, and returns a
display-ready markdown string.

---

## 4. Client Rendering

### 4.1 Message Grouping

Consecutive messages from the same sender within a **5-minute window** are
grouped. The first message in a group shows the full header (avatar + username +
timestamp). Subsequent messages show only the body with tighter vertical spacing.

```
┌──────────────────────────────────────────────────┐
│  [AB]  alice_b                         10:42 AM  │  <- group header
│        hey everyone                              │
│        check out this link https://example.com   │  <- grouped, no header
│        anyone up for a game?                     │  <- grouped, no header
│                                                  │
│  [KT]  k0ji_tech                       10:45 AM  │  <- new group (different sender)
│        yeah let's go                             │
│                                                  │
│  [AB]  alice_b                         10:51 AM  │  <- new group (>5min gap)
│        alright starting stream                   │
└──────────────────────────────────────────────────┘
```

Grouping breaks when any of:
- Different sender
- Time gap > 5 minutes between consecutive messages
- A system message appears
- The message is a reply (always gets its own header)

### 4.2 Timestamps

| Condition | Display |
|-----------|---------|
| < 1 minute ago | "just now" |
| < 60 minutes ago | "Xm ago" |
| Same day | "HH:MM" (24h or 12h based on OS locale) |
| Yesterday | "Yesterday HH:MM" |
| Same year | "MMM D" (e.g., "Mar 20") |
| Older | "MMM D, YYYY" |

Timestamps appear on the group header only, not on every message. On hover over
any message in the group, show the exact timestamp as a tooltip.

### 4.3 Message Gravity
The feed is a plain Flickable over a window of the newest 200 loaded messages
(`ChatScrollState::WINDOW`). Every row has its real height, so the content
height is exact: scroll to bottom lands on the last message, and a prepend
moves the viewport by the exact growth so the row the user reads stays put.
A virtualized list estimates its height from the rows on screen; that estimate
sent the view to the same row every time, which is why it is not used.

Rows gather at the bottom when they are fewer than the feed holds. When the
user is at the bottom, a new message scrolls into view and the read cursor
moves (2.5). When the user has scrolled up, a new message shows the red
"New messages" pill instead; clicking it scrolls to the bottom. An own message
always scrolls to the bottom.

The window grows while the user reads older rows and goes back to 200 when
they return to the bottom. The top of the feed reveals loaded rows that were
trimmed before it asks the server for a page. Each scroll target is applied
on three frames, because a row the model just got is laid out on the next
frame.

Opening a crew: at the unread rule when the read cursor is behind the newest
message, else at the bottom.

### 4.4 Avatars

The first message in each group displays the sender's avatar (colored circle
with initials, using the existing `Avatar` component). The avatar appears to the
left of the username. Grouped messages below it are indented to align with the
message text, not the avatar.

```
 [KT]  k0ji_tech                       10:45 AM
       yeah let's go
       starting now
```

Avatar indent: avatar width (28px) + spacing (8px) = 36px left padding for
grouped message bodies.

### 4.5 GIF Rendering
GIF messages render inline in the message flow.

1. The chat fetches `url` (the 200px rendition) and decodes every frame, at
   most 32 frames at 320px, and plays it twice; hover plays it again.
2. Max display width: 300px. Height from `width`/`height`.
3. Frames are cached by URL across row rebuilds; the cache keeps the newest
   16 GIFs. The newest 8 GIFs of the window load at once, older ones on hover.
4. A GIF that cannot load says "GIF not available".
5. GIF-only messages (empty body) show just the media. GIF + text messages show
   text above the media.

### 4.6 Reply Rendering

When a message has `reply_to` set, render a compact quote block above the
message body:

```
 ┌─ Replying to k0ji_tech
 │  yeah let's go
 └─
 actually wait, not yet
```

The quoted content shows sender name + first line of the original message
(truncated to 100 chars). The quote block is visually distinct (left border
accent, muted text color). Clicking the quote scrolls to the original message
if it's loaded in the current history.

To resolve reply_to: look up the referenced message ID in the local message
cache. If not found (message was before the loaded window), show
"Replying to [username]" without the quoted text, or fetch it with a single
`channel_message_list` call filtered around that ID.

### 4.7 Edit/Delete Indicators

Edited messages show "(edited)" in muted text after the message body. The edit
state is determined by comparing Nakama's `update_time > create_time` on the
message object.

Deleted messages render as a tombstone: "[message deleted]" in muted italic.
Nakama's `channel_message_remove` soft-deletes, leaving the message entry with
empty content. Check for empty/null content to detect this.

---

## 5. Chat Input UI

### 5.1 Layout
The chat input area sits at the bottom of the chat panel: pending image chips
when there are any, a row of icon buttons, then the field with the send button
inside it at the right.

```
│  ... messages ...                                │
│  [thumb ✕] [thumb ✕]                             │  <- pending images (when any)
│  [📎] [GIF] [☺]                                   │  <- icon buttons, 28px
│  ┌──────────────────────────────────────── [↑] ┐  │
│  │ Message crewmates...                        │  │  <- field, 40px, send inside
│  └─────────────────────────────────────────────┘  │
```

### 5.2 Action Pills
Icon buttons, 32×28px, no label on screen (the label is the accessible
name). Muted icon; on hover a graphite fill; when its popover is open, white
on the dim fill with the button's cut corners.

| Button | Icon | Action |
|------|------|--------|
| Attach | paperclip | File dialog, up to four images (2.3) |
| GIF | filmstrip | Opens the GIF popover (§6) |
| Emoji | smiley | Opens the emoji picker (§8) |

The send button sits inside the field at its right: red when there is text or
a ready image (it commits), graphite when the field is empty. Enter sends too.

### 5.3 Text Input

- Height: 36px single line. Grows to max 120px (roughly 5 lines) as text wraps.
- Placeholder: "Message crewmates..."
- Submit on Enter. Shift+Enter for newline.
- On submit: parse body for `<@user_id>` tokens to populate `mentions` array,
  construct the message envelope, send via `channel_message_send`.
- Clear input on successful send.

### 5.4 Reply Mode

When the user triggers a reply (see §5.5), the input area shows a reply bar
above the pills row:

```
│  ┌─ Replying to k0ji_tech              [✕]  │  <- reply bar (dismissable)
│  [GIF]  [Emoji]                              │
│  ┌────────────────────────────────────────┐  │
│  │ Message crewmates...                   │  │
│  └────────────────────────────────────────┘  │
```

The reply bar shows "Replying to {username}" with a close button to cancel.
When present, the sent message includes the `reply_to` field. Focus moves to the
text input when reply mode activates.

### 5.5 Message Context Actions
A click on a message opens its action bar at the top right of the message,
over the header line. A second click on the message, a click on another
message, or Escape closes it. (Hover was unreliable: StyledText takes
pointer presses itself, so a target under it never fired.)

```
                                    [☺] [↩] [⋯]
  hey everyone check out this link
```

| Button | Action |
|--------|--------|
| ☺ React | Opens the emoji picker in reaction mode: the pick becomes a reaction (2.4) |
| ↩ Reply | Activates reply mode in input (§5.4) |
| ⋯ More | Context menu: Edit (own messages only), Delete (own messages only), Copy Text |

Edit: replaces message body in input, switches to edit mode. On submit, calls
`channel_message_update` with the updated envelope. The input shows "Editing
message" indicator (similar to reply bar). Pressing Escape cancels edit.

Delete: confirmation prompt ("Delete this message?"). On confirm, calls
`channel_message_remove`; the objects of an image message go with it (2.3).

## 6. GIF Integration (Giphy)

### 6.1 API Setup

- Provider: Giphy API v1, called from the client. Giphy's rules: "Do not
  proxy requests to GIPHY", "Do not cache API responses or media URLs", and
  search and trending must be called from the client.
- API key: baked in at build time from `GIPHY_API_KEY`. A production build
  refuses to compile without it; a dev build without it shows "GIFs are off in
  this build". Beta keys allow 100 calls per hour per key; apply for a
  production key before launch.
- Base URL: `https://api.giphy.com/v1/gifs`, `rating=g`, `limit=20`.

### 6.2 Endpoints

| Call | Endpoint |
|---|---|
| Search | `GET /search?q={query}&api_key={KEY}&limit=20&rating=g` |
| Trending (on open, before the user types) | `GET /trending?api_key={KEY}&limit=20&rating=g` |

### 6.3 Response Mapping

`url` = `images.fixed_width.url` (or `original`), `preview` =
`images.fixed_width_small.url` (or `preview_gif`, or `url`), `width`/`height`
from `fixed_width`, `alt` = `title`. A dimension that is not a number is 0;
it does not fail the page.

### 6.4 GIF Popover UI

Opens above the composer. Search field with focus on open, trending until the
user types, two columns of tiles at real aspect that animate while open.
Click a tile: sends `type: "gif"` and closes. Escape or the GIF button closes.
"Powered by GIPHY" at the bottom, as Giphy requires.

An error is never a blank grid: the popover shows one sentence ("The GIF
service did not answer. Your messages still send.", "Too many GIF searches
right now. Try again in a minute.", "The GIF service refused this build's
key.", "GIFs are off in this build.") and a Try again button.


## 7. @Mentions

### 7.1 Syntax

Mentions are stored in the message body as `<@user_id>` tokens. Example:

```
"body": "hey <@user_abc> check this out"
```

mello-core resolves each token to a display name before it emits a message
(`ChatMessage.display_body`, plus `mentions: [{user_id, name}]`). Names come
from the `MemberNames` cache; a mentioned user who is not cached (left the
crew, or history loaded before the crew list) is looked up first. Clients
render `display_body` and never resolve tokens themselves. Message previews from
Nakama (sidebar, code 115) resolve tokens on the server.

### 7.2 Autocomplete

When the user types `@` in the text input, open a small autocomplete popover
above the input showing crew members. Filter as the user continues typing.

```
┌──────────────────────────┐
│  [AB]  alice_b           │
│  [KT]  k0ji_tech         │
│  [MR]  m1ra              │
└──────────────────────────┘
@k  <- user is typing
```

- Trigger: `@` character at start of input or after a space.
- Source: crew member list (already available in client state).
- Arrow keys to navigate, Enter or Tab to select.
- On select: replace `@partial` with `@display_name` in the input and record the
  pick `{user_id, name}`. The input never shows a raw `<@user_id>` token.
- `SendMessage` / `EditMessage` carry the picks (`mentions`). mello-core turns
  each picked `@name` into `<@user_id>` (whole word, longest name first). Typed
  `@name` text without a pick stays plain text. An edit starts with the
  message's existing mentions as picks.
- The `mentions` array in the envelope is populated at send time by scanning
  the body for `<@...>` tokens. It is the rule of record for push (spec 23).

### 7.3 Render

Mentions render as `@display_name` in the mention colour: `Theme.mention`
(`#EB4D5F`) on desktop, `Palette.accent` (same value) on iOS. mello-core reports
the spans (`chat::mention_spans`); desktop wraps them in `<font color>` in the
message markdown with the name escaped. A user who left the crew still shows by
name (looked up); `@unknown` appears only when the lookup fails. A raw user id
is never shown.

If the mention is for the current user, apply a subtle background highlight to
the entire message to draw attention.

---

## 8. Emoji Picker

### 8.1 Data Source
The full Unicode set the bundled font draws: `mello-core/src/emoji_table.rs`,
generated by `tools/emoji/gen_emoji_table.py` from Unicode's `emoji-test.txt`,
every fully-qualified emoji up to Emoji 14.0 without skin-tone variants, 1849
entries in eight categories (Smileys & People, Animals & Nature, Food & Drink,
Activities, Travel & Places, Objects, Symbols, Flags). Each entry maps the
emoji to its Unicode name and category. The font is Twemoji (Mozilla's COLRv0
build with base outlines added, `client/ui/fonts/LICENSE-Twemoji.txt`).

### 8.2 Picker UI
Opens above the composer, 320px tall.

```
┌──────────────────────────────────────────┐
│  🔍 Search emoji                         │  <- focus on open
│  RECENT                                  │
│  🔥 💀 😂 👀 🎮 🙏 👍 ❤️ 🤝               │  <- the session's picks, newest first
│  😀 😃 😄 😁 😆 😅 😂 🤣 😊               │
│  😇 🙂 🙃 😉 😌 😍 🥰 😘 😗               │  <- the category, or the search hits
│  ...                                     │
├──────────────────────────────────────────┤
│  [😀] [🐱] [🍕] [⚽] [🚗] [💡] [❤️] [🏳️] │  <- category tabs
└──────────────────────────────────────────┘
```

- Nine cells per row that share the width (about 36px in the 400px panel),
  24px glyphs. The grid is a ListView of uniform rows.
- Search matches every word of the name; the exact name comes first, then
  names that start with the query. No hits: "No emoji called that".
- Click an emoji: inserts it at the end of the field and closes the picker.
  In reaction mode (opened from React, 5.5) the pick is a reaction instead.
- Escape, the Emoji button, or a pick closes it.
- Emoji-only messages of three or fewer render at 32px (`Theme.font-xxl`).

### 8.1 Crew emoji

A crew has up to five crew emoji, a gift of crew quests (`CREW-QUESTS.md`
section 3.3). Any member adds one; the adder and the owner remove it.

- The picker shows the crew's emoji in a row above the Twemoji set, with the
  crew name. A free slot opens the add screen while the gift is open.
- A pick writes `:name:` into the field, or reacts with it in reaction mode.
- A reaction chip draws the crew emoji image in place of the text.
- A message whose body is only crew emoji tokens, three or fewer, draws them
  as 48 px images. The body stays the raw tokens.
- Inside a sentence a crew emoji shows as `:name:` text.
- A removed emoji leaves its reactions and messages as `:name:` text.

## 9. Unread Tracking

### 9.1 Read cursor

Superseded by 2.5: the read cursor lives in the user's own storage and the
unread state survives a restart.

### 9.2 Logic

- Maintain a per-crew `unread_count: u32` in client state.
- When the user's active crew is X and a new message arrives for crew Y (Y != X):
  increment `unread_count` for crew Y.
- When the user switches active crew to Y: reset `unread_count` for Y to 0.
- On app startup: all `unread_count` values start at 0.

### 9.3 Display

Show unread count as a badge on the crew icon in the sidebar. Only show when
count > 0. Cap display at "99+".

If any unread message contains a mention of the current user, the badge uses
accent color instead of the default muted badge color.

---

## 10. Backend Changes

### 10.1 Message Validation Hook

Register a Nakama `before` hook on `ChannelMessageSend` to validate incoming
messages:

```go
func BeforeChannelMessageSend(ctx context.Context, logger runtime.Logger,
    db *sql.DB, nk runtime.NakamaModule, msg *api.ChannelMessageSend,
) (*api.ChannelMessageSend, error) {
    var envelope map[string]interface{}
    if err := json.Unmarshal([]byte(msg.Content), &envelope); err != nil {
        return nil, runtime.NewError("invalid message format", 3) // INVALID_ARGUMENT
    }

    // Require v field
    v, ok := envelope["v"]
    if !ok {
        return nil, runtime.NewError("missing version field", 3)
    }
    if v != float64(1) {
        return nil, runtime.NewError("unsupported version", 3)
    }

    // Require type field
    msgType, ok := envelope["type"].(string)
    if !ok {
        return nil, runtime.NewError("missing type field", 3)
    }
    if msgType != "text" && msgType != "gif" {
        return nil, runtime.NewError("invalid message type", 3)
    }

    // Enforce body length
    body, _ := envelope["body"].(string)
    if len(body) > 2000 {
        return nil, runtime.NewError("message too long (max 2000)", 3)
    }

    return msg, nil // Allow
}
```

System messages (`type: "system"`) are sent server-side and bypass this hook.

### 10.2 System Message Emission

Update existing server-side event handlers to send system messages through
the crew channel when events occur:

| Event | `event` value | `body` template |
|-------|---------------|-----------------|
| Stream started | `stream_started` | "{username} started streaming" |
| Stream ended | `stream_ended` | "{username} stopped streaming" |
| Member joined crew | `member_joined` | "{username} joined the crew" |
| Member left crew | `member_left` | "{username} left the crew" |

Use `nk.ChannelMessageSend()` with the system user context. Set
`persistent: true` so system messages appear in history.

---

## 11. Message History & Pagination

### 11.1 Initial Load

When the user opens/switches to a crew, load the most recent 50 messages using
Nakama's `channel_message_list`:

```
channel_message_list(channel_id, limit=50, forward=false, cursor=nil)
```

Parse each message content as a structured envelope. Store
in an ordered list in client state.

### 11.2 Scroll-Up Pagination

When the user scrolls to the top of the loaded messages, fetch the next page:

```
channel_message_list(channel_id, limit=50, forward=false, cursor=oldest_cursor)
```

Show a loading spinner while fetching. Prepend results to the message list.
Maintain scroll position (the message the user was looking at should not move).

Stop paginating when Nakama returns an empty result (no more history).

### 11.3 Real-Time Messages

New messages arriving via the Nakama WebSocket are appended to the bottom of
the message list. Apply grouping logic as they arrive. If the user is scrolled
to bottom, auto-scroll. If not, show the "New messages" pill (§4.3).

---

## 12. mello-core Interface

### 12.1 Events (Server -> Client)

Add to the existing event enum in mello-core:

```rust
enum Event {
    // ... existing events ...

    /// A new chat message was received (real-time or from history load)
    ChatMessage {
        crew_id: String,
        message: ChatMessageData,
    },

    /// A chat message was edited
    ChatMessageEdited {
        crew_id: String,
        message_id: String,
        updated: ChatMessageData,
    },

    /// A chat message was deleted
    ChatMessageDeleted {
        crew_id: String,
        message_id: String,
    },
}

struct ChatMessageData {
    id: String,
    sender_id: String,
    sender_username: String,
    content: ParsedMessage,    // Parsed envelope
    display_body: String,      // Pre-processed body for rendering (mentions resolved,
                               // bare URLs wrapped). Fed directly to StyledText/@markdown().
                               // Before Slint 1.16: same as content.body (no processing).
    create_time: DateTime,
    update_time: DateTime,
}

struct ParsedMessage {
    v: u32,
    msg_type: MessageType,     // Text, Gif, System
    body: String,
    reply_to: Option<String>,
    mentions: Vec<String>,
    gif: Option<GifData>,
    system_event: Option<String>,
    system_data: Option<serde_json::Value>,
}
```

### 12.2 Commands (Client -> Server)

```rust
enum Command {
    // ... existing commands ...

    /// Send a text message
    SendMessage {
        crew_id: String,
        body: String,
        reply_to: Option<String>,
    },

    /// Send a GIF message
    SendGif {
        crew_id: String,
        gif: GifData,
    },

    /// Edit own message
    EditMessage {
        crew_id: String,
        message_id: String,
        new_body: String,
    },

    /// Delete own message
    DeleteMessage {
        crew_id: String,
        message_id: String,
    },

    /// Load message history (paginate)
    LoadHistory {
        crew_id: String,
        cursor: Option<String>,
    },
}
```

The mello-core command handler constructs the JSON envelope from `SendMessage`
fields before calling `channel_message_send`. The client UI never constructs
the raw JSON directly.

---

## 13. Implementation Phases
Shipped as chat v2. The build log, the decisions and what was dropped are in
`plans/CHAT-V2.md`.

