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

The lounge does not fire the deep link on its own. Its "Open in m3llo"
button opens it (§9.2). On a download, the lounge copies the web link to the
clipboard, and a fresh install reads it on its first launch (§7.1).

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

**Purpose:** Return public crew info for a given invite code. Called by the Cloudflare landing page (server key), the OG image generator (server key), and the client. The client uses its bearer token, or the HTTP key when no session exists (a fresh install opened from an invite link, §7).

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

`parse_invite_input(text)` reads what a user types or pastes into an invite field. It returns the code as `XXXX-XXXX`, or `None`. It accepts:

- a web link: `https://m3llo.app/join/{code}`, with or without the scheme and `www.`, and with a trailing slash, a query or a fragment
- a deep link: `mello://join/{code}`
- a bare code, in any case, with or without the dash

Leading and trailing spaces do not matter. The invite-code card (§8.5) and the Discover field (§8.4) both call this function.

`lounge_link_code(text, lounge_host)` reads the clipboard at startup (§7.1). It is stricter than `parse_invite_input`, because the user did not type the text. It accepts only `https://{lounge_host}/join/{code}`, with `http`, `www.`, a trailing slash, a query or a fragment. A bare code, a deep link, a text longer than 256 bytes and any other text return `None`. An empty `lounge_host` accepts any host.

`extract_deep_link()` reads `argv[1]` at startup. The `mello://` scheme is registered in `Cargo.toml` via `osx_url_schemes = ["mello"]` for macOS app bundles.

---

## 6. Client: Deep Links to a Running App

**Files:** `client/src/ipc.rs`, `client/src/platform/macos_url_events.rs`, `client/src/poll_loop.rs`

A deep link can reach an app that already runs. Windows and Linux start a second instance with the URL in argv. macOS does not.

### 6.1 Windows and Linux: IPC relay

When m3llo is already running and the OS launches a second instance (via `mello://join/...`), the second instance must relay the URL to the running instance instead of silently dropping it.

**Mechanism:** Platform-specific one-shot IPC using a shared endpoint derived from the app lock name (`app.mello.desktop`).

- **macOS/Linux:** Unix domain socket at `/tmp/app.mello.desktop.sock`. The first instance binds a non-blocking `UnixListener`. The second instance connects, writes the URL as a newline-terminated string, and exits.
- **Windows:** Named pipe at `\\.\pipe\app.mello.desktop`. The first instance runs a background thread that blocks on `ConnectNamedPipe` in a loop, reading one line per connection and forwarding it via `mpsc` channel. The second instance opens the pipe as a regular file and writes the URL.

**Cleanup:** The `IpcListener` removes the socket file on drop (Unix). The socket is also cleaned up before bind to handle stale files from crashes.

### 6.2 macOS: Apple Event

macOS never puts the URL in argv, and it does not start a second instance. LaunchServices sends a `kAEGetURL` Apple Event to the app: at a cold start, and while the app runs.

`macos_url_events::install()` registers a handler with `NSAppleEventManager`. `lib.rs` calls it before any Slint code. AppKit can install its own `kAEGetURL` handler while the app finishes launching. The module therefore also registers again on `NSApplicationWillFinishLaunchingNotification`. The handler queues the URL.

At a cold start the event arrives after startup. The URL takes the path of §6.3, not `pending_deep_link`.

### 6.3 Dispatch

The poll loop (`poll_loop.rs`, 100 ms timer) takes the relayed URLs and the macOS queue on each tick. `dispatch_running_link` parses each URL with `deep_link::parse()` and sends `Command::ResolveCrewInvite` or `Command::SelectCrew` at once. It does not use `pending_deep_link`.

The resolve answer decides the screen (`handlers::crew`). For a fresh install, `opens_onboarding` is true on step 1, and the answer opens the welcome screen (§7). For anyone else it opens the join modal (§8.3).

> **The relay on step 1 is the lounge's second way in.** The installer can
> start the app before the user presses "Open in m3llo". The link then
> reaches an app that shows step 1. It must open the welcome screen, not the
> join modal. `flow_tests::a_relayed_join_link_on_step_one_opens_the_welcome_screen`
> sends the link over a real IPC endpoint and checks this.

---

## 7. Client: Startup Deep Link Dispatch

**File:** `client/src/main.rs`, `client/src/handlers/auth.rs`

On startup, `extract_deep_link()` parses `argv[1]` into a `DeepLink` and stores it in `AppContext::pending_deep_link`.

**Fresh install** (no session, no device account, onboarding before the account exists): a join link is resolved at once, before an account exists. Onboarding skips step 1 and opens the welcome screen. It names the inviter and the crew. "Join {crew}" opens step 2, and finalize joins the crew by its invite code. "Not now" opens step 1 and forgets the invite. Step 2 has "Back", which opens the welcome screen again with the invite kept. The crew tile on these screens shows the crew avatar (§8.3). See [01-CLIENT.md](../01-CLIENT.md) §6.2. File: `client/src/onboarding_invite.rs`.

An invite typed in the card on step 1 (§8.5) takes the same path, also for a machine with a device account.

### 7.1 The invite on the clipboard

The installer cannot carry the invite. On a download the lounge copies `https://m3llo.app/join/{code}` to the clipboard (§9.2). The app reads it once, in `onboarding_invite::dispatch_at_startup`.

The client reads the clipboard only when all of these are true:

- The machine is a fresh install: `opens_onboarding` is true.
- Startup resumes `Loading`. This is the first launch. A later launch is on step 1 or further. An invite that the user declined with "Not now" must not open again.
- No deep link arrived in argv.

The client reads the text with `lounge_link_code` (§5) and the lounge host. A valid code becomes `pending_deep_link`, and takes the path of a deep link.

Any other text is ignored. The client does not log, store or send it. The client does not change the clipboard.

**e2e.** An `e2e` build reads the file in `MELLO_E2E_CLIPBOARD_FILE` instead of the system clipboard. The driver gives each journey run one file, the clipboard of its machine. `qa/journeys/invite-lounge-download.ts` (flow INV-10) drives the local lounge and both ways into the app. It needs the lounge on `localhost:8788` (`npm run dev` in mello-site).

**Lounge host.** `Config::lounge_host` in mello-core, next to `nakama_host`. It is set at compile time from `LOUNGE_HOST`. `release.yml` sets `m3llo.app`. A build without it has an empty host, and accepts a join link on any host. Use this for a local lounge.

**macOS.** A deep link at a cold start arrives after startup (§6.2). Startup can then read the clipboard first. When both hold an invite, the deep link resolves last and replaces the clipboard invite.

**A second way in.** The user can copy something else before the first launch. The lounge's "Open in m3llo" button then carries the invite (§6.3).

**Any other case:** the link is dispatched after authentication completes:

- **Returning user:** dispatched on `Event::LoggedIn` (after `Command::LoadMyCrews`).
- **Device account in onboarding:** dispatched on `Event::OnboardingReady` (after onboarding finishes and crews are loaded).

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

- Inviter line: the inviter as an octagon, and "{inviter} invited you". No line when the invite has no inviter.
- Crew avatar (large, centered). The resolve answer has no avatar. The client sends `FetchCrewAvatars` with the crew ID, and the tile shows the initials until `CrewAvatarLoaded` arrives, and when the crew has none. Core calls `get_crew_avatar`, which needs no session.
- Crew name (large text)
- Sub line: the member count and the highlight from the weekly recap (if available), e.g. "4 members · 7h hangout · 3 clips"
- Primary button: **"Join crew"** — calls `join_by_invite_code` RPC, navigates to the crew on success
- Secondary text link: **"Not now"** — dismisses the modal

The modal stays open while the join runs. The button reads "Joining…" and does
not accept a click. `InviteJoined` closes the modal. `InviteJoinFailed` keeps it
open with the error above the button, and the button accepts a retry.

> **Do not close the modal before the join returns.** For a user with a device
> account who is in onboarding, the modal sits on top of step 3. No other
> surface there can show the error. A fresh install does not get the modal: it
> joins the crew in onboarding (§7).

**Error states.** mello-core maps the gRPC code of the RPC error to
`InviteError`. The client picks the text.

| `InviteError` | gRPC code | Resolve failure | Join failure |
|---|---|---|---|
| `InvalidCode` | 3, 5 | "This invite link is no longer valid." with a dismiss button | Same text |
| `CrewFull` | 8 | — | "This crew is full." |
| `NotAllowed` | 7 | — | "You cannot join this crew." |
| `Failed` | Any other, or no code | "Could not load this invite. Try again." | "Could not join the crew. Try again." |

Only `InvalidCode` blames the invite. A server or network failure is `Failed`.

A user who typed the code (§8.5) did not follow a link. For this user the `InvalidCode` resolve text reads "This invite code is not valid."

### 8.4 Invite code field in Discover

**File:** `client/ui/panels/discover_panel.slint`

The "Join a Private Crew" field also calls `join_by_invite_code`. It takes the same input as the invite-code card (§8.5): a link or a code. The client reads the input with `parse_invite_input` and sends the normalised code. An input that is no invite shows "This invite code is not valid." under the field, and no command goes to core. When the join
modal is not open, `InviteJoinFailed` shows under the field. `InvalidCode` reads
"This invite code is not valid." The other texts are the join texts in §8.3.

### 8.5 Invite-code card on onboarding step 1

**Files:** `client/ui/panels/onboarding.slint` (`InviteCodeCard`), `client/src/onboarding_invite.rs`

The web lounge cannot always hand an invite to the app. A user who installs the app then has no link to follow. Step 1 has a card for this user. See [01-CLIENT.md](../01-CLIENT.md) §6.4.

- The card shows for every user on step 1. It sits right of "Create your own crew".
- The user pastes a link or a code, and presses "Open invite" or Enter.
- The client reads the text with `parse_invite_input`. A text that is no invite shows "This invite code is not valid." in the card and marks the field. No command goes to core.
- A valid code sends `ResolveCrewInvite`. Without a session, core uses the `http_key`, as for a deep link. The button reads "Checking…" and ignores clicks until the answer arrives.
- `CrewInviteResolved` takes the path of a deep link: the client stores the invite and opens the welcome screen (§7).
- `CrewInviteResolveFailed` shows the message in the card and marks the field. `InvalidCode` reads "This invite code is not valid." Any other error reads "Could not load this invite. Try again."
- Editing the field clears the message.
- "Not now" on the welcome screen opens step 1 with the field empty.

The button is white: it opens the invite and commits nothing. The red button that commits is "Join {crew}" on the welcome screen.

**After a logout.** The machine has a device account, and the user has no session. The join modal cannot join without a session. An invite typed in the card therefore opens the welcome screen. Finalize sends `device_id` and `invite_code`. Core authenticates the device, which opens the existing account, and calls `join_by_invite_code`.

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

The frame also has an **"Open in m3llo"** button. It opens `mello://join/{code}`
for a guest who has the app installed. After a download, the frame shows
"Installed?" and an "Open m3llo" button. The installed app then shows the welcome
screen ([01-CLIENT.md](../01-CLIENT.md) §6.2). The download URL has no `?invite=`
parameter: an installer cannot read it.

**On a download** (`main.js`, `onDownload`), from the frame, the rail or a gate:

1. The lounge writes `https://m3llo.app/join/{code}` to the clipboard with
   `navigator.clipboard.writeText`. The write happens first, inside the click:
   a browser allows it only during a user gesture.
2. The browser downloads the installer. The page stays, and voice keeps running.
3. The gate opens in the **installed state**: "Install, then open m3llo", an
   **"Open in m3llo"** button and "Keep listening in the browser".
4. When the write succeeds, the gate shows "Your invite is copied. m3llo picks
   it up when it opens." When it fails, the gate shows no line. The button
   stays.

The app reads the clipboard on its first launch (§7.1).

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

- Deferred deep link through the installer or a server. The installer does
  not carry the code. The server keeps no state for an install: no machine ID
  and no IP address. The invite goes through the clipboard (§7.1), and the
  lounge's "Open in m3llo" button (§9.2) is the second way in.
