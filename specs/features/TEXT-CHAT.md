# MELLO Text Chat Specification

> **Component:** Text Chat (Rich Messaging)  
> **Version:** 0.1  
> **Status:** Beta Scope  
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
| GIF provider | Tenor API v2, client-side | Free tier, generous limits, no backend proxy needed |
| Emoji | Unicode native + picker | No custom emoji in beta |
| Replies | Quote-reply with `reply_to` field | Simple, no threads |
| DMs | Not in beta | Crew chat only |
| Unread tracking | Client-side volatile counters | No backend storage needed for beta |
| Message length | 2000 characters max | Enforced client-side and server-side |
| Message history | 50 messages initial load, paginate on scroll-up | Nakama cursor-based pagination |

### What Changes

| Layer | Before | After |
|-------|--------|-------|
| Message content | Plain text string | Structured JSON envelope (see §2) |
| Client rendering | Flat text, raw ISO timestamps | Grouped messages, relative time, markdown, inline GIFs |
| Chat input | Single text field in panel | Text field + action pills row (GIF, Emoji) |
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
  "type": "text | gif | system",
  "body": "string",
  "reply_to": "message_id | null",
  "mentions": ["user_id", "..."],
  "gif": { ... }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `v` | `integer` | Yes | Schema version. Always `1` for now. |
| `type` | `string` | Yes | One of: `text`, `gif`, `system`. |
| `body` | `string` | Yes | Message text. May contain markdown and mention tokens. Max 2000 chars. Empty string for gif-only messages. |
| `reply_to` | `string \| null` | No | Nakama message ID of the quoted message. `null` or omitted if not a reply. |
| `mentions` | `string[]` | No | Array of user IDs mentioned in `body`. Used for notification routing. Omit if empty. |
| `gif` | `object \| null` | No | GIF metadata. Required when `type` is `gif`. See §2.2. |

### 2.2 GIF Object

```json
{
  "tenor_id": "12345678",
  "url": "https://media.tenor.com/.../mp4",
  "preview": "https://media.tenor.com/.../tinygif",
  "width": 320,
  "height": 240,
  "alt": "search query or description"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `tenor_id` | `string` | Tenor content ID. Used for Tenor share registration. |
| `url` | `string` | Primary display URL. Use `mediummp4` format from Tenor response for bandwidth efficiency. |
| `preview` | `string` | Low-res preview URL. Use `tinygif` format. Shown while `url` loads. |
| `width` | `integer` | Native width in pixels. |
| `height` | `integer` | Native height in pixels. |
| `alt` | `string` | Search query that produced this result. Used as accessible description. |

### 2.3 System Message

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

Messages anchor to the bottom of the chat panel and grow upward. When the user
is scrolled to the bottom, new messages auto-scroll into view. When the user has
scrolled up (reading history), new messages do **not** auto-scroll. Instead,
show a "New messages" pill at the bottom of the message area; clicking it scrolls
to bottom.

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

GIF messages render inline in the message flow. Display rules:

1. Show `preview` URL immediately (low-res, fast load).
2. Load `url` (mp4) in background. Swap to mp4 when ready.
3. Max display width: 300px. Scale height proportionally using `width`/`height`
   from the gif object.
4. Click on GIF does nothing in beta (no lightbox).
5. GIF-only messages (empty body) show just the media. GIF + text messages show
   text above the media.

Rendering mp4 in Slint: use the platform's native video element or fall back to
rendering the `preview` (animated GIF) as an image if mp4 playback is not
feasible in Slint. The preview URL is always a GIF image and can be displayed
with Slint's `Image` element via async fetch.

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

The chat input area sits at the bottom of the chat panel. It has two rows:
an action pills row above the text input.

```
│  ... messages ...                                │
│                                                  │
│  [GIF]  [Emoji]                                  │  <- pills row
│  ┌────────────────────────────────────────────┐  │
│  │ Message crewmates...                       │  │  <- text input (36px height)
│  └────────────────────────────────────────────┘  │
```

### 5.2 Action Pills

Small, ghost-style buttons. Icon + short label. Muted text color, no border.
On hover: subtle background fill (`Theme.bg-tertiary`). On active/open: accent
color text.

| Pill | Icon | Action |
|------|------|--------|
| GIF | filmstrip/play icon | Opens GIF search popover (§6) |
| Emoji | smiley face icon | Opens emoji picker popover (§8) |

Future pills (not in beta, but leave room): Attach (paperclip icon, grayed
out/hidden for now).

Pills row height: ~28px. Compact, does not dominate.

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

On hover over a message, show a small floating action row at the top-right
corner of the message:

```
                                    [↩️] [😀] [⋯]
  hey everyone check out this link
```

| Button | Action |
|--------|--------|
| ↩️ Reply | Activates reply mode in input (§5.4) |
| 😀 React | Opens reaction picker (post-beta, hidden for now) |
| ⋯ More | Context menu: Edit (own messages only), Delete (own messages only), Copy Text |

Edit: replaces message body in input, switches to edit mode. On submit, calls
`channel_message_update` with the updated envelope. The input shows "Editing
message" indicator (similar to reply bar). Pressing Escape cancels edit.

Delete: confirmation prompt ("Delete this message?"). On confirm, calls
`channel_message_remove`.

---

## 6. GIF Integration (Tenor)

### 6.1 API Setup

- Provider: Tenor API v2
- API key: Embedded in client binary. Store as a build-time constant.
- Base URL: `https://tenor.googleapis.com/v2`
- Required params on all requests: `key={API_KEY}`, `client_key=mello`,
  `media_filter=mp4,tinygif`

Register for a Tenor API key at https://developers.google.com/tenor/guides/quickstart.
Free tier allows 50 search requests/minute.

### 6.2 Endpoints

**Search:**
```
GET /search?q={query}&key={KEY}&client_key=mello&media_filter=mp4,tinygif&limit=20
```

**Trending (shown on popover open, before user types):**
```
GET /featured?key={KEY}&client_key=mello&media_filter=mp4,tinygif&limit=20
```

**Register share (call after user sends a GIF):**
```
POST /registershare?key={KEY}&client_key=mello&id={tenor_id}&q={search_query}
```

### 6.3 Response Mapping

From Tenor search results, extract per-result:

```
tenor_id = result.id
url      = result.media_formats.mp4.url        (or mediummp4 if mp4 absent)
preview  = result.media_formats.tinygif.url
width    = result.media_formats.mp4.dims[0]     (or tinygif dims)
height   = result.media_formats.mp4.dims[1]
alt      = search query
```

### 6.4 GIF Popover UI

Opens upward from the GIF pill, overlaying the message area. Does not resize
the chat panel.

```
┌──────────────────────────────────────────┐
│  🔍 Search Tenor                         │  <- search input, auto-focus
├──────────────────────────────────────────┤
│  ┌────────┐ ┌────────┐ ┌────────┐       │
│  │  gif   │ │  gif   │ │  gif   │       │  <- masonry-style grid
│  │        │ │        │ └────────┘       │     (2 or 3 columns)
│  └────────┘ └────────┘ ┌────────┐       │
│  ┌────────┐ ┌────────┐ │  gif   │       │
│  │  gif   │ │  gif   │ │        │       │
│  └────────┘ └────────┘ └────────┘       │
└──────────────────────────────────────────┘
```

- Popover dimensions: ~340px wide, ~360px tall.
- On open: load trending GIFs.
- On type in search: debounce 300ms, then call search endpoint.
- Grid shows preview URLs (tinygif) for fast loading.
- Click a GIF: sends message with `type: "gif"`, closes popover,
  calls registershare in background.
- Escape or click outside: closes popover without sending.
- "Powered by Tenor" attribution at bottom of popover (required by Tenor ToS).

---

## 7. @Mentions

### 7.1 Syntax

Mentions are stored in the message body as `<@user_id>` tokens. Example:

```
"body": "hey <@user_abc> check this out"
```

The client resolves `user_id` to display name at render time using the crew
member list from crew state.

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
- On select: replace `@partial` with `<@user_id>` in the raw body, display as
  `@display_name` with accent color highlight in the input.
- The `mentions` array in the envelope is populated at send time by scanning
  the body for `<@...>` tokens.

### 7.3 Render

Mentions render as `@display_name` with accent color text. If the mentioned user
is no longer in the crew (left/kicked), render as `@unknown` in muted text.

If the mention is for the current user, apply a subtle background highlight to
the entire message to draw attention.

---

## 8. Emoji Picker

### 8.1 Data Source

Embed a static JSON dataset of Unicode emoji in the client binary. Use a
curated subset (~1500 common emoji) grouped by category. The dataset maps
each emoji to: codepoint(s), short name, keywords, category.

Categories: Smileys & People, Animals & Nature, Food & Drink, Activities,
Travel & Places, Objects, Symbols, Flags.

### 8.2 Picker UI

Opens upward from the Emoji pill, same pattern as the GIF popover.

```
┌──────────────────────────────────────────┐
│  🔍 Search emoji                         │
├──────────────────────────────────────────┤
│  😀 😃 😄 😁 😅 😂 🤣 😊 😇 🙂         │
│  😉 😌 😍 🥰 😘 😗 😙 😚 😋 😛         │
│  ...                                     │
├──────────────────────────────────────────┤
│  [😀] [🐱] [🍕] [⚽] [🚗] [💡] [❤️] [🏳️] │  <- category tabs
└──────────────────────────────────────────┘
```

- Grid of emoji, ~8-10 per row.
- Category tabs at bottom for quick navigation.
- Search filters by short name and keywords.
- Click emoji: inserts Unicode character at cursor position in text input.
  Does **not** close the picker (user may want to insert multiple).
- Escape or click outside: closes picker.
- Optional: "Frequently used" row at top, tracked locally (in-memory for beta,
  no persistence needed).

---

## 9. Unread Tracking

### 9.1 Beta Approach (Client-Side Only)

No backend storage for read cursors in beta. Unread state is volatile (resets on
app restart). This is acceptable for small crews.

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

These are implementation ordering guidelines for the agent. Each phase is
independently shippable.

### Phase 1: Visual Fixes (no backend changes)

- Message grouping (§4.1)
- Human-readable timestamps (§4.2)
- Bottom-anchored message gravity (§4.3)
- Avatar on group headers (§4.4)
- Scroll-up pagination with spinner (§11.2)

### Phase 2: Structured Envelope

- Message envelope schema (§2)
- Backend validation hook (§10.1)
- mello-core `ParsedMessage` types and `Command`/`Event` updates (§12)

### Phase 3: Rich Features

- GIF search, popover, send, render (§6, §4.5)
- Reply-to: context action, reply bar, send, render (§4.6, §5.4, §5.5)
- Edit/delete: context actions, edit mode, tombstone render (§4.7, §5.5)
- Action pills row UI (§5.1, §5.2)

### Phase 4: Polish

- @Mentions: autocomplete, token render, highlight own mentions (§7)
- Emoji picker (§8)
- Unread tracking + sidebar badges (§9)
- System messages from server events (§10.2)
- "New messages" scroll pill (§4.3)

### Phase 5: Markdown Rendering (blocked on Slint 1.16)

- Upgrade Slint dependency to 1.16+ (or pin to master if stable enough)
- Swap message body `Text` elements to `StyledText` with `@markdown()` (§3.2)
- Implement `prepare_body_for_display()` pre-processor for mentions and
  bare URL wrapping (§3.3)
- Wire `StyledText.link-clicked` callback to open URLs externally
