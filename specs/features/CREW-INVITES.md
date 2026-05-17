# Crew Invites

> **Component:** Invite system (Backend · Cloudflare · Client)
> **Status:** Implemented
> **Related:** [12-NATIVE-PLATFORM.md](./12-NATIVE-PLATFORM.md) §9, [04-BACKEND.md](./04-BACKEND.md) §8, [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

Shareable invite links let any crew member invite people to their crew. Pasting a link anywhere (Discord, iMessage, Reddit) renders a rich Open Graph preview card. Clicking the link opens m3llo directly if installed, or shows a landing page with a download CTA if not.

**User-facing link format:** `https://m3llo.app/join/{code}`

**Deep link format:** `mello://join/{code}`

---

## 2. Invite Code

Format: `XXXX-XXXX` (alphanumeric, uppercase). Each crew has exactly one permanent invite code, generated at crew creation time by the `create_crew` RPC. Codes are stored in Nakama Storage under the system user in two collections:

- **`invite_codes`** — key is the code, value contains the `crew_id`. Used for code→crew lookup.
- **`crew_invite_codes`** — key is the `crew_id`, value contains the `code`. Used for crew→code lookup.

The `CrewState` model includes an `InviteCode` field so any member can read the code when loading crew details. The full invite URL is constructed client-side: `https://m3llo.app/join/{code}`.

---

## 3. Backend RPC: `resolve_crew_invite`

**File:** `backend/nakama/data/modules/invite_codes.go`

**Purpose:** Return public crew info for a given invite code. Called by the Cloudflare landing page (server key), the OG image generator (server key), and the client (bearer token) to populate the join confirmation screen.

**Request:** `{ "code": "XXXX-XXXX" }`

**Response:**
```json
{
  "crew_name": "ostkatt's crew",
  "avatar_seed": "ostkatt",
  "crew_id": "uuid",
  "highlight": "7h hangout · 3 clips · Counter-Strike 2"
}
```

**Logic:**
1. Look up the invite code in `invite_codes` storage. Return `NOT_FOUND` if missing.
2. Fetch the crew's group metadata for name and avatar seed.
3. Build a `highlight` string from the crew's latest weekly recap (from `CrewEventLedger`). The highlight combines hangout hours, clip count, and top game into a single human-readable line (e.g. "7h hangout · 3 clips · Counter-Strike 2"). Empty string if no recap data exists yet.
4. Return only public-safe fields.

The highlight approach was chosen over `online_count`/`member_count` to avoid O(n) presence reads per request and to surface more engaging information on preview cards.

This RPC is callable with the Nakama HTTP key (via `?unwrap=true&http_key=...`) so Cloudflare Functions can call it without a user session.

---

## 4. Client: Deep Link Parsing

**File:** `client/src/deep_link.rs`

The `DeepLink` enum handles two URL patterns:

- `mello://join/{code}` → `DeepLink::Join { code }`
- `mello://crew/{id}` → `DeepLink::Crew { id }`

`extract_deep_link()` reads `argv[1]` at startup. The `mello://` scheme is registered in `Cargo.toml` via `osx_url_schemes = ["mello"]` for macOS app bundles.

---

## 5. Client: IPC Relay for Deep Links

**File:** `client/src/ipc.rs`

When m3llo is already running and the OS launches a second instance (via `mello://join/...`), the second instance must relay the URL to the running instance instead of silently dropping it.

**Mechanism:** Platform-specific one-shot IPC using a shared endpoint derived from the app lock name (`app.mello.desktop`).

- **macOS/Linux:** Unix domain socket at `/tmp/app.mello.desktop.sock`. The first instance binds a non-blocking `UnixListener`. The second instance connects, writes the URL as a newline-terminated string, and exits.
- **Windows:** Named pipe at `\\.\pipe\app.mello.desktop`. The first instance runs a background thread that blocks on `ConnectNamedPipe` in a loop, reading one line per connection and forwarding it via `mpsc` channel. The second instance opens the pipe as a regular file and writes the URL.

The poll loop (`poll_loop.rs`, 50ms timer) calls `ipc_listener.try_recv()` each tick. Received URLs are parsed with `deep_link::parse()` and dispatched immediately as `Command::ResolveCrewInvite` or `Command::SelectCrew` — no `pending_deep_link` needed since the app is already authenticated and running.

**Cleanup:** The `IpcListener` removes the socket file on drop (Unix). The socket is also cleaned up before bind to handle stale files from crashes.

---

## 6. Client: Startup Deep Link Dispatch

**File:** `client/src/main.rs`, `client/src/handlers/auth.rs`

On startup, `extract_deep_link()` parses `argv[1]` into a `DeepLink` and stores it in `AppContext::pending_deep_link`. The link is dispatched after authentication completes:

- **Returning user:** dispatched on `Event::LoggedIn` (after `Command::LoadMyCrews`).
- **New user:** dispatched on `Event::OnboardingReady` (after onboarding finishes and crews are loaded).

`dispatch_pending_deep_link()` takes the pending link and sends the appropriate command to mello-core.

---

## 7. Client: In-App Flows

### 7.1 Sharing an invite link

**Entry points:**
- "Invite" icon button in the crew panel header (`crew_panel.slint`)
- "Share invite link" button on the invite card in the crew feed (`crew_feed.slint`)

**Flow:**
1. User clicks invite button.
2. `invite-share-requested` callback fires. Rust reads the `invite_code` from the active crew's data model.
3. Constructs the full URL: `https://m3llo.app/join/{code}`.
4. Opens the `InviteShareModal` (`invite_share_modal.slint`) showing the URL and a "Copy link" button.
5. Clicking "Copy link" writes the URL to the system clipboard via `arboard` and visually confirms with "Copied!" + green button state.

### 7.2 Invite card in the crew feed

**File:** `client/src/handlers/clip.rs`, `client/ui/panels/crew_feed.slint`

An `InviteCard` component is injected client-side at a fixed position (slot 2) in the feed layout. It shows "Invite friends" with a description, a primary "Share invite link" button, and a "Hide" link.

- **Visibility:** Always shown unless the user hides it. Hidden crew IDs are persisted in `settings.hidden_invite_crew_ids`.
- **Hide action:** `on_hide_invite_card` removes the card from the current feed model and saves the crew ID to settings.

### 7.3 Join Crew confirmation screen

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

## 8. Landing Page (Cloudflare Pages Function)

**File:** `mello-site/functions/join/[code].ts`

**URL:** `https://m3llo.app/join/{code}`

Server-side rendered on every request so Open Graph tags are present in the initial HTML (link previewers don't execute JS).

### 8.1 Request flow

1. Extract `code` from the URL path.
2. Call `resolve_crew_invite` on Nakama via HTTP POST with HTTP key (`NAKAMA_HTTP_KEY`, `NAKAMA_BASE_URL` stored as Pages environment variables / secrets).
3. On success: render full HTML with populated OG tags and crew data.
4. On `NOT_FOUND`: render a branded "invite not found" page.
5. Response is cached via `caches.default` with a short TTL.

Pages include `<meta name="robots" content="noindex, nofollow">` to prevent indexing.

### 8.2 Open Graph tags

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

### 8.3 Page content

- Crew avatar, name, and highlight text
- Primary CTA: **"Open in m3llo"** — fires `mello://join/{code}`. If the tab is still visible after 2s (app not installed), shows a download prompt with fallback to `https://m3llo.app/download`.
- Design: dark background `#0D0D0F`, `#EB4D5F` accents, Oxanium headings, Barlow body.

---

## 9. OG Image Generator (Cloudflare Pages Function)

**File:** `mello-site/functions/og/[code].ts`

**URL:** `https://m3llo.app/og/{code}`

Generates a 1200×630 PNG Open Graph card on demand using `@resvg/resvg-wasm`.

### 9.1 Pipeline

1. Call `resolve_crew_invite` on Nakama with HTTP key.
2. Fetch the crew avatar PNG from `avatar.m3llo.app/{seed}.png`.
3. Build an SVG card with crew avatar, name, highlight text, and m3llo branding.
4. Rasterize to PNG using `resvg-wasm` with embedded font buffers.
5. Return with `Content-Type: image/png`. Cached via `caches.default`.

### 9.2 Font embedding

Fonts are subsetted to Latin characters and stored as `.ttf.bin` files (the `.bin` extension is required for Cloudflare Pages Functions bundler to treat them as binary imports):

- `functions/_shared/fonts/Oxanium-Latin.ttf.bin`
- `functions/_shared/fonts/Barlow-Latin.ttf.bin`
- `functions/_shared/fonts/Audiowide-Latin.ttf.bin`

These are imported as `ArrayBuffer` and passed to the `Resvg` constructor via `fontBuffers`.

### 9.3 SVG card layout

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

## 10. Shared Nakama Client (Cloudflare)

**File:** `mello-site/functions/_shared/nakama.ts`

Shared utility used by both Pages Functions. Provides `resolveCrewInvite(env, code)` which calls the Nakama RPC with the HTTP key passed as a query parameter (`&http_key=...`). The `Env` interface expects `NAKAMA_BASE_URL` and `NAKAMA_HTTP_KEY`.

---

## 11. Dev Seed

**File:** `backend/nakama/data/modules/dev_seed.go`

The dev seed script creates invite codes for all 6 sample crews (`DEVS-0001`, `GAMR-0001`, `MUSC-0001`, `DSGN-0001`, `OPS_-0001`, `RETR-0001`) with corresponding `invite_codes` and `crew_invite_codes` storage entries.

---

## 12. Invite Policy

Crew admins can control who is allowed to generate invite codes via the `invite_policy` field in group metadata:

| Policy | Who can create invites |
|--------|----------------------|
| `everyone` (default) | Any crew member |
| `admins` | Only owner (state 0) and admins (state 1) |

The policy is set via the `update_crew` RPC and enforced in `CreateInviteCodeRPC`. The setting is exposed in the crew settings Overview tab as a two-state selector ("Everyone" / "Admins").

---

## 13. Out of Scope (this version)

- Per-invite usage analytics
- Expiring or single-use invites
- Deferred deep link via installer embedding
- Invite link in crew discovery or public directory
