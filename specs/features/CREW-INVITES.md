# Crew Invites

> **Component:** Invite system (Backend · Cloudflare · Client)
> **Status:** Implemented
> **Related:** [12-NATIVE-PLATFORM.md](./12-NATIVE-PLATFORM.md) §9, [04-BACKEND.md](./04-BACKEND.md) §8, [00-ARCHITECTURE.md](./00-ARCHITECTURE.md), [13-VOICE-CHANNELS.md](../13-VOICE-CHANNELS.md), [SFU-INTEGRATION.md](./SFU-INTEGRATION.md), [mello-sfu/01-SFU.md](../../../mello-sfu/01-SFU.md) §5

---

## 1. Overview

Shareable invite links let any crew member invite people to their crew. A link
pasted anywhere (Discord, iMessage, Reddit) renders an Open Graph preview card.

Opening the link shows the **web lounge**: a working, cut-down m3llo in the
browser. A guest hears the crew and speaks to them without an install and
without an account. Streams, replays, clips and chat need the app.

**User-facing link format:** `https://m3llo.app/join/{code}`

**Deep link format:** `mello://join/{code}`

The deep link is still used by the client. The lounge does not fire it.

---

## 2. Invite Code

Format: `XXXX-XXXX` (alphanumeric, uppercase).

A crew gets its first code at creation time, from the `create_crew` RPC. A crew
can have many codes. `create_invite_code` writes a new one on every call and
tags it with the caller's user ID, so a code identifies the member who shared it.

Codes are stored in Nakama Storage under the system user in one collection:

- **`invite_codes`** — key is the code. The value holds `crew_id`, and
  `inviter_user_id` when a member created the code.

There is no crew→code index. A member gets a shareable link by calling
`create_invite_code`, which returns a fresh code.

Two helpers in `invite_codes.go` own the storage convention:

| Function | Purpose |
|---|---|
| `normalizeInviteCode` | Trims and upper-cases a user-supplied code |
| `lookupInviteCode` | Resolves a code to `crew_id` and `inviter_user_id` |

Every caller uses them. Do not read the collection directly.

---

## 3. Backend RPC: `resolve_crew_invite`

**File:** `backend/nakama/data/modules/invite_codes.go`

**Purpose:** Return public crew info for a given invite code. Called by the Cloudflare landing page (server key), the OG image generator (server key), and the client (bearer token) to populate the join confirmation screen.

**Request:** `{ "code": "XXXX-XXXX" }`

**Response:** `ResolveCrewInviteResponse`

| Field | Notes |
|---|---|
| `crew_name`, `avatar_seed`, `crew_id` | Always present |
| `highlight` | One line from the latest weekly recap. Empty when no recap exists |
| `member_count`, `members` | Up to 5 previews, shuffled |
| `top_game`, `longest_session_min`, `most_active` | From the latest recap |
| `inviter_display_name`, `inviter_avatar_seed` | Present when the code carries an inviter |
| `recent_clips` | Up to 4. **Includes `media_url`** |
| `session_snapshots` | Up to 8 image URLs |

**Logic:**
1. Resolve the code with `lookupInviteCode`. Return `NOT_FOUND` if missing.
2. Read the group for name, member count and members.
3. Build `highlight` and the recap fields from the crew event ledger.
4. Read clips and snapshots from the ledger.

The highlight approach was chosen over `online_count` to avoid O(n) presence reads per request.

This RPC is callable with the Nakama HTTP key (via `?unwrap=true&http_key=...`) so Cloudflare Functions can call it without a user session.

> **`recent_clips` and `session_snapshots` carry playable media.** Any caller
> that serves a browser must strip them. The lounge uses `guest_crew_feed`
> (§4) for this reason. Only the OG image generator and the client consume this
> RPC's media fields.

---

## 4. Guest Sessions

**File:** `backend/nakama/data/modules/guest_sessions.go`

A **guest** is an anonymous browser participant who followed an invite link.

| Property | Value |
|---|---|
| Identity | Nakama device account, created on join |
| Crew membership | None. The guest never joins the Nakama group |
| `isCrewMember` | False |
| Capability | Voice only |

The guest sits in the real voice room next to members. Every native client sees
the guest arrive.

### 4.1 What a guest can do

| Action | Guest | Member |
|---|---|---|
| Hear the crew | Yes | Yes |
| Speak | Yes | Yes |
| See who is in the channel | Yes | Yes |
| See clip and session metadata | Yes | Yes |
| Play a clip or a replay | No | Yes |
| Watch a live stream | No | Yes |
| Send chat | No | Yes |
| Stream, clip, overlay | No | Yes |

### 4.2 `guest_voice_join`

Auth: a device session. Crew membership is **not** required.

**Request:** `{ "code": "XXXX-XXXX", "nickname": "…", "channel_id": "…" }`

`channel_id` is optional and defaults to the crew's default channel.

**Response:** `success`, `crew_id`, `channel_id`, `channel_name`, `voice_state`,
`mode` (always `sfu`), `sfu_endpoint`, `sfu_token`, `expires_in`.

**Order of checks:**
1. Resolve the code with `lookupInviteCode`.
2. Reject when `guest_policy` is `off`.
3. Apply the per-code rate limit.
4. Reject when SFU auth is not configured.
5. Resolve the channel.
6. Reject when the channel is at the guest cap.
7. Seat the guest with `joinVoiceRoom`.
8. Sign the SFU token. On failure, release the seat and return an error.

### 4.3 Two rules that differ from `voice_join`

**No membership check.** Holding a valid invite code is the authorization.

**The SFU is mandatory.** A browser cannot join the native P2P mesh, so the
premium-crew check does not apply to guests and there is no P2P fallback. When
the SFU cannot issue a token, the RPC fails. It does not seat a guest who can
neither speak nor hear.

Both paths call the same `resolveVoiceChannel`, `joinVoiceRoom` and
`issueVoiceSFUToken` in `voice_state.go`. Do not fork them. A second copy will
drift from the roster, presence and push behaviour that members see.

### 4.4 Guests and the weekly recap

`joinVoiceRoom` calls `recordLedgerSession`, which returns early for a guest.

The crew event ledger feeds the weekly recap. Without this guard, a visitor who
sits in voice for 40 minutes can become the crew's most active member.
`updateLastSeen` is skipped for the same reason.

Two tests fail if the guard is removed.

### 4.5 `guest_voice_leave`

Auth: a device session. Releases the seat and forgets the guest session.

### 4.6 `guest_crew_feed`

Auth: the Nakama HTTP key. No user session.

This is the read path for the lounge. It returns a **public-safe projection**.

| Data | Guest sees |
|---|---|
| Crew name, member count, member names | Yes |
| Inviter name | Yes |
| Weekly recap, including per-player win/loss | Yes |
| Clip type, clipper, duration, game | Yes |
| Clip file address | No |
| Stream name, duration, viewers | Yes |
| Stream snapshot images | No. A `has_snapshots` flag only |
| User IDs | No |

The guest-visible structs have no field for `MediaURL`, `LocalPath` or
`SnapshotURLs`. A leak therefore needs a new field, not a missed condition. The
tests serialize the payload and search the text for forbidden strings.

`collectGuestClips` reads clips from both places a crew keeps them: the durable
`crew_clips` document and `clip` events in the event ledger. Reading one source
under-reports. It de-duplicates on clip ID.

### 4.7 Limits

Every limit is enforced on the server. The browser cannot change them.

| Limit | Value | Constant |
|---|---|---|
| Guests per voice channel | 3 | `MaxGuestsPerVoiceChannel` |
| Session length | 30 minutes | `GuestSessionTTL` |
| Joins per invite code | 1 per 2 seconds | `GuestJoinMinInterval` |
| Nickname length | 24 runes | `maxGuestNicknameLen` |

The voice reconciler calls `ExpireGuestSessions` on each tick. A closed browser
tab sends no leave, so the TTL is the only thing that removes that guest.

`sanitizeGuestNickname` cleans the name before the crew sees it. It removes
control characters, collapses whitespace, and truncates on runes, not bytes.

### 4.8 Cost

Guests always use the SFU. An invite to any crew can therefore create SFU
traffic, including for crews without the premium entitlement. Voice is about
40 kbit/s for each participant. The limits in §4.7 bound the exposure.

---

## 5. Client: Deep Link Parsing

**File:** `client/src/deep_link.rs`

The `DeepLink` enum handles two URL patterns:

- `mello://join/{code}` → `DeepLink::Join { code }`
- `mello://crew/{id}` → `DeepLink::Crew { id }`

`extract_deep_link()` reads `argv[1]` at startup. The `mello://` scheme is registered in `Cargo.toml` via `osx_url_schemes = ["mello"]` for macOS app bundles.

---

## 6. Client: IPC Relay for Deep Links

**File:** `client/src/ipc.rs`

When m3llo is already running and the OS launches a second instance (via `mello://join/...`), the second instance must relay the URL to the running instance instead of silently dropping it.

**Mechanism:** Platform-specific one-shot IPC using a shared endpoint derived from the app lock name (`app.mello.desktop`).

- **macOS/Linux:** Unix domain socket at `/tmp/app.mello.desktop.sock`. The first instance binds a non-blocking `UnixListener`. The second instance connects, writes the URL as a newline-terminated string, and exits.
- **Windows:** Named pipe at `\\.\pipe\app.mello.desktop`. The first instance runs a background thread that blocks on `ConnectNamedPipe` in a loop, reading one line per connection and forwarding it via `mpsc` channel. The second instance opens the pipe as a regular file and writes the URL.

The poll loop (`poll_loop.rs`, 50ms timer) calls `ipc_listener.try_recv()` each tick. Received URLs are parsed with `deep_link::parse()` and dispatched immediately as `Command::ResolveCrewInvite` or `Command::SelectCrew` — no `pending_deep_link` needed since the app is already authenticated and running.

**Cleanup:** The `IpcListener` removes the socket file on drop (Unix). The socket is also cleaned up before bind to handle stale files from crashes.

---

## 7. Client: Startup Deep Link Dispatch

**File:** `client/src/main.rs`, `client/src/handlers/auth.rs`

On startup, `extract_deep_link()` parses `argv[1]` into a `DeepLink` and stores it in `AppContext::pending_deep_link`. The link is dispatched after authentication completes:

- **Returning user:** dispatched on `Event::LoggedIn` (after `Command::LoadMyCrews`).
- **New user:** dispatched on `Event::OnboardingReady` (after onboarding finishes and crews are loaded).

`dispatch_pending_deep_link()` takes the pending link and sends the appropriate command to mello-core.

---

## 8. Client: In-App Flows

### 8.1 Sharing an invite link

**Entry points:**
- "Invite" icon button in the crew panel header (`crew_panel.slint`)
- "Share invite link" button on the invite card in the crew feed (`crew_feed.slint`)

**Flow:**
1. User clicks invite button.
2. `invite-share-requested` callback fires. Rust reads the `invite_code` from the active crew's data model.
3. Constructs the full URL: `https://m3llo.app/join/{code}`.
4. Opens the `InviteShareModal` (`invite_share_modal.slint`) showing the URL and a "Copy link" button.
5. Clicking "Copy link" writes the URL to the system clipboard via `arboard` and visually confirms with "Copied!" + green button state.

### 8.2 Invite card in the crew feed

**File:** `client/src/handlers/clip.rs`, `client/ui/panels/crew_feed.slint`

An `InviteCard` component is injected client-side at a fixed position (slot 2) in the feed layout. It shows "Invite friends" with a description, a primary "Share invite link" button, and a "Hide" link.

- **Visibility:** Always shown unless the user hides it. Hidden crew IDs are persisted in `settings.hidden_invite_crew_ids`.
- **Hide action:** `on_hide_invite_card` removes the card from the current feed model and saves the crew ID to settings.

### 8.3 Join Crew confirmation screen

**File:** `client/ui/panels/join_crew_modal.slint`

Full-screen modal overlay shown when `DeepLink::Join` is dispatched:

- Crew avatar (large, centered)
- Crew name (large text)
- Highlight text from the weekly recap (if available), e.g. "7h hangout · 3 clips · Counter-Strike 2"
- Primary button: **"Join crew"** — calls `join_by_invite_code` RPC, navigates to the crew on success
- Secondary text link: **"Not now"** — dismisses the modal

**Error states:**
- `NOT_FOUND`: "This invite link is no longer valid." with a dismiss button.
- Network error: retry option.

---

## 9. Web Lounge (Cloudflare Pages Function)

**Files:** `mello-site/functions/join/[code].ts`, `mello-site/lounge/*`

**URL:** `https://m3llo.app/join/{code}`

The page is server-side rendered so Open Graph tags are in the initial HTML.
Link previewers do not execute JavaScript. The lounge itself is a set of plain
ES modules. The site has no build step.

| File | Purpose |
|---|---|
| `lounge/main.js` | Entry point. Owns the join, mute and gate state |
| `lounge/voice.js` | Device auth, guest RPCs, WebRTC to the SFU |
| `lounge/ui.js` | Frame, rail, feed, chat, control bar, join panel |
| `lounge/gates.js` | The install dialogs |
| `lounge/data.js` | Maps the bootstrap payload onto the view model |
| `lounge/fixtures.js` | Sample data for `?mock=1` |

### 9.1 Request flow

1. Extract `code` from the URL path.
2. Call `resolve_crew_invite` and `guest_crew_feed` in parallel, with the HTTP key.
3. On `NOT_FOUND` from the invite: render an "invite not found" page.
4. Render the shell with OG tags and a JSON bootstrap payload.

`?mock=1` skips both RPCs and renders from fixtures. Use it to work on the page
with no backend running.

Pages include `<meta name="robots" content="noindex, nofollow">`.

### 9.2 Design source

The lounge copies the native client. Values come from
`client/ui/theme.slint`, not from the marketing site.

| Element | Value |
|---|---|
| Accent | `#FF1E56` |
| Window, panel surface | `#181818`, `#202020` |
| Columns | Crew rail 240, stage, chat 340, `Theme.gap` 12 |
| Control bar | `Theme.control-bar-height` 81px, inside the feed column |

When a mockup in `designs/` disagrees with `theme.slint`, `theme.slint` wins.

An **invite frame** wraps the client. The frame is not from the client: it
carries the wordmark, the inviter, the crew name and the install button.

### 9.3 Joining voice

A panel points at the voice channel in the crew rail and takes the guest's name.

> **The panel's button is required, not decorative.** Browsers refuse to play
> audio, and Safari refuses `getUserMedia`, until the user interacts with the
> page. A guest joined automatically sits in the room and hears silence. Do not
> replace this panel with an automatic join.

One click stores the name, unblocks audio playback, satisfies the gesture
requirement and joins the channel talking. Clicking the channel row rejoins
after a hangup.

A blocked microphone does not fail the join. The guest still hears the crew. A
silent placeholder track holds the sender open, so granting the microphone later
is a `replaceTrack` and not a renegotiation.

The panel reports progress and failure in place. The control bar is behind the
dim while the panel is up, so an error shown only there is invisible.

### 9.4 Install gates

Five actions open a dialog that names what the app adds: `stream`, `replay`,
`clip`, `chat`, `broadcast`. Each reports its own analytics event, so the wall a
guest reaches first is measurable.

### 9.5 Open Graph tags

```html
<meta property="og:title"       content="Join {crew_name} on m3llo" />
<meta property="og:description" content="{highlight}" />
<meta property="og:image"       content="https://m3llo.app/og/{code}" />
<meta property="og:image:width" content="1200" />
<meta property="og:image:height" content="630" />
<meta property="og:url"         content="https://m3llo.app/join/{code}" />
<meta property="og:type"        content="website" />
<meta name="twitter:card"       content="summary_large_image" />
```

---

## 10. OG Image Generator (Cloudflare Pages Function)

**File:** `mello-site/functions/og/[code].ts`

**URL:** `https://m3llo.app/og/{code}`

Generates a 1200×630 PNG Open Graph card on demand using `@resvg/resvg-wasm`.

### 10.1 Pipeline

1. Call `resolve_crew_invite` on Nakama with HTTP key.
2. Fetch the crew avatar PNG from `avatar.m3llo.app/{seed}.png`.
3. Build an SVG card with crew avatar, name, highlight text, and m3llo branding.
4. Rasterize to PNG using `resvg-wasm` with embedded font buffers.
5. Return with `Content-Type: image/png`. Cached via `caches.default`.

### 10.2 Font embedding

Fonts are subsetted to Latin characters and stored as `.ttf.bin` files (the `.bin` extension is required for Cloudflare Pages Functions bundler to treat them as binary imports):

- `functions/_shared/fonts/Oxanium-Latin.ttf.bin`
- `functions/_shared/fonts/Barlow-Latin.ttf.bin`
- `functions/_shared/fonts/Audiowide-Latin.ttf.bin`

These are imported as `ArrayBuffer` and passed to the `Resvg` constructor via `fontBuffers`.

### 10.3 SVG card layout

```
┌──────────────────────────────────────────────────────────────────┐  1200×630
│                                                                  │
│   [avatar 120×120]   {crew_name}                    m3llo        │
│   rounded square     Oxanium 48px white             Audiowide    │
│                                                     22px #EB4D5F │
│                      {highlight}                                 │
│                      Barlow 28px #888                            │
│                                                                  │
│   Background #0D0D0F                                             │
└──────────────────────────────────────────────────────────────────┘
```

Avatar is embedded as a base64 data URI in SVG `<image>` with `rx="16"` for rounded corners.

---

## 11. Shared Nakama Client (Cloudflare)

**File:** `mello-site/functions/_shared/nakama.ts`

Shared utility used by both Pages Functions. It calls Nakama RPCs with the HTTP
key passed as a query parameter (`&http_key=...`).

| Function | Used by |
|---|---|
| `resolveCrewInvite(env, code)` | The lounge shell and the OG image |
| `guestCrewFeed(env, code)` | The lounge feed |

The `Env` interface:

| Variable | Type | Purpose |
|---|---|---|
| `NAKAMA_BASE_URL` | Required | Nakama address, used server-side |
| `NAKAMA_HTTP_KEY` | Required, secret | Admin-level. **Never send to a browser** |
| `NAKAMA_SERVER_KEY` | Optional | Public client key. The browser needs it for device auth |
| `NAKAMA_PUBLIC_URL` | Optional | Browser-reachable Nakama address, when it differs from `NAKAMA_BASE_URL` |

Without `NAKAMA_SERVER_KEY` the lounge renders read-only and hides voice. Set
both optional variables in the Cloudflare Pages environment for production.

The bootstrap payload sent to the browser carries `NAKAMA_SERVER_KEY` and never
`NAKAMA_HTTP_KEY`.

---

## 12. Dev Seed

**File:** `backend/nakama/data/modules/dev_seed.go`

The dev seed script writes one invite code for each of the 6 sample crews into
the `invite_codes` collection.

| Crew | Code |
|---|---|
| Devs | `DEVS-0001` |
| Gamers | `GAME-0001` |
| Music | `MUSC-0001` |
| Design | `DSGN-0001` |
| Ops | `OPS0-0001` |
| Retro | `RETR-0001` |

Use `http://localhost:8788/join/DEVS-0001` to open the lounge against the local
stack.

---

## 13. Invite Policy

Crew admins can control who is allowed to generate invite codes via the `invite_policy` field in group metadata:

| Policy | Who can create invites |
|--------|----------------------|
| `everyone` (default) | Any crew member |
| `admins` | Only owner (state 0) and admins (state 1) |

The policy is set via the `update_crew` RPC and enforced in `CreateInviteCodeRPC`. The setting is exposed in the crew settings Overview tab as a two-state selector ("Everyone" / "Admins").

---

## 14. Guest Policy

Crew admins control whether the invite link opens a working lounge, via the
`guest_policy` field in group metadata.

| Policy | Effect |
|--------|--------|
| `open` (default) | Anyone with the code can join voice from a browser |
| `off` | `guest_voice_join` refuses. The page still renders and offers the download |

The policy is set via `update_crew` and read by `guestPolicyFor`. A crew that
never sets it is open. `parseGuestPolicy` treats absent, malformed and unknown
values as `open`, so a crew must opt out on purpose.

Setting `guest_policy` does not clear `invite_policy`. `update_crew` loads the
existing metadata once and writes both.

**Not yet exposed in the client.** The field has no control in the crew settings
Overview tab.

---

## 15. Out of Scope (this version)

- Per-invite usage analytics
- Expiring or single-use invites
- Playing clips, replays or live streams in the browser
- Sending chat from the browser
- Switching crews in the lounge
- Invite link in crew discovery or public directory
- A `guest_policy` control in crew settings

**Partly addressed:** deferred deep link. The lounge appends `?invite={code}` to
the download URL. The client does not read it yet, so a guest who installs still
has to open the link again.
