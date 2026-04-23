# Crew Invites

> **Component:** Invite system (Backend · Cloudflare · Client)
> **Status:** Ready for implementation
> **Related:** [12-NATIVE-PLATFORM.md](./12-NATIVE-PLATFORM.md) §9, [04-BACKEND.md](./04-BACKEND.md) §8, [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

Replace the current copy-paste code flow with a shareable link system. Any crew member can share a permanent invite link. Pasting the link anywhere (Discord, iMessage, a Reddit post) renders a rich preview card showing the crew's name, online count, and avatar. Clicking the link opens m3llo directly if installed, or a landing page with a download CTA if not.

**User-facing link format:** `https://m3llo.app/join/{code}`

**Deep link format:** `mello://join/{code}`

> Note: The placeholder scheme in 12-NATIVE-PLATFORM.md §9.1 used `mello://invite/{code}`. This spec supersedes that. Update the `DeepLink` enum in `src/deep_link.rs` accordingly (see §6.2).

---

## 2. Invite Code

The `XXXX-XXXX` invite code format and its storage in Nakama are already implemented as part of `create_crew` (see 04-BACKEND.md §8). Each crew has exactly one permanent invite code, generated at creation time and stored in Nakama Storage under the system user.

**This spec does not change code generation or storage.** The code is already there. This spec builds the link layer on top of it.

The full invite URL is constructed by the client: `https://m3llo.app/join/{code}`.

---

## 3. Data Model

No changes to the existing invite storage schema. The Cloudflare Worker resolves codes via the new `resolve_crew_invite` RPC (§4), not by reading Nakama Storage directly.

One prerequisite: confirm the existing invite storage records use `PUBLIC_READ` permission (read permission = 2). If they currently use a lower permission, the `resolve_crew_invite` RPC must use a server key call, not a user token. The Pages Function and Worker always use the server key regardless, so this is a backend concern only.

---

## 4. Backend: New RPC

The existing `join_by_invite_code` RPC already handles joining. Only one new RPC is needed.

### 4.1 `resolve_crew_invite`

**Purpose:** Return public crew info for a given invite code. Called by the Cloudflare Pages Function (landing page), the OG image Worker, and the client (to populate the join confirmation screen before the user commits).

**Caller:** Cloudflare Worker (server key) or authenticated client

**Request:**
```json
{ "code": "XXXX-XXXX" }
```

**Logic:**
1. Look up the invite code in Nakama Storage. Return `NOT_FOUND` if it does not exist.
2. Fetch the crew record to retrieve name, avatar seed, member count, and current online count.
3. Return only public-safe fields. Do not expose internal user IDs or `created_by`.

**Response:**
```json
{
  "crew_name": "ostkatt's crew",
  "member_count": 7,
  "online_count": 3,
  "avatar_seed": "ostkatt",
  "crew_id": "uuid"
}
```

This RPC must be callable with the Nakama server key (via `?unwrap=true`) so the Cloudflare Worker can call it without a user session.

---

## 5. Invite Code Accessibility (Client)

The invite code must be readable by any crew member, not just the crew creator. Confirm how the code is currently surfaced:

- If `get_crew` or equivalent already returns the invite code in the crew record, no backend change is needed. The client constructs the link from the code it already has.
- If the code is only returned at `create_crew` time, add it to the crew details response so any member can retrieve it.

The Claudie implementing this should check the current `get_crew` RPC response before writing any new backend code.

---

## 6. Client: URL Scheme

### 6.1 Scheme update

The `mello://join/{code}` path is the canonical deep link for crew invites. Update `Cargo.toml` bundle metadata and any existing references to `mello://invite/` to use `mello://join/` instead.

### 6.2 DeepLink enum update

```rust
// src/deep_link.rs

pub enum DeepLink {
    Join { code: String },   // replaces Invite
    Crew { id: String },
}

pub fn parse(url: &str) -> Option<DeepLink> {
    let url = url.strip_prefix("mello://")?;
    let mut parts = url.splitn(2, '/');
    match parts.next()? {
        "join" => Some(DeepLink::Join { code: parts.next()?.to_string() }),
        "crew" => Some(DeepLink::Crew { id:   parts.next()?.to_string() }),
        _      => None,
    }
}
```

### 6.3 Startup deep link handling

The existing `extract_deep_link()` in `main.rs` reads `mello://` URLs from `argv[1]`. On startup, after the user is authenticated, the pending deep link (if any) is dispatched to the UI layer.

**Pending invite during auth:** If a deep link arrives before the user has logged in (first install, not yet onboarded), store the parsed `DeepLink::Join { code }` in memory. After successful login, dispatch it immediately. Do not persist to disk.

---

## 7. Client: In-App Flows

### 7.1 Sharing an invite link

**Entry point:** "Invite" button in the crew panel header. Visible to all crew members.

**Flow:**
1. User taps "Invite".
2. Client retrieves the crew's invite code (from crew state — see §5).
3. Constructs the full URL: `https://m3llo.app/join/{code}`.
4. Displays a small modal with:
   - The full URL in a read-only text field
   - A "Copy link" button that writes the URL to the clipboard and briefly shows "Copied!"
   - No other options. No QR code. No expiry controls.

The modal is deliberately minimal. The link is the whole story.

**Also:** Update the existing "CREW ESTABLISHED" modal (shown after crew creation) to display the full URL and "Copy link" instead of the bare code and "Copy Code".

### 7.2 Joining via deep link (m3llo already installed)

When a `DeepLink::Join { code }` is dispatched (either from startup args or from the IPC relay when m3llo is already running):

1. If the user is not yet authenticated: hold the code in memory until login completes, then proceed from step 2.
2. Call `resolve_crew_invite` to fetch crew info.
3. If the user is already a member of the resolved crew: navigate directly to that crew. No confirmation screen.
4. Otherwise: show the **Join Crew** confirmation screen (§7.3).

### 7.3 Join Crew confirmation screen

A full-screen modal overlay shown before joining.

**Contents:**
- Crew avatar (large, centred)
- Crew name (Oxanium, large)
- "{n} members · {m} online" in Barlow muted text
- Primary button: **"Join crew"** — calls existing `join_by_invite_code` RPC, then navigates to the crew on success
- Secondary text link: **"Not now"** — dismisses the modal, returns to previous state

**Loading state:** Show a skeleton (avatar placeholder, pulsing name and count lines) while `resolve_crew_invite` is in flight.

**Error states:**
- `NOT_FOUND`: show "This invite link is no longer valid." with a dismiss button.
- Network error: show retry option.

---

## 8. Landing Page (Cloudflare Pages Function)

**URL:** `https://m3llo.app/join/[code]`

Implemented as a Cloudflare Pages Function at `functions/join/[code].ts`. Runs server-side on every request so Open Graph tags are present in the initial HTML response. Link previewers do not execute JavaScript; SSR is required.

### 8.1 Request flow

1. Extract `code` from the URL path.
2. Call `resolve_crew_invite` on Nakama via HTTP POST with server key (`NAKAMA_SERVER_KEY`, `NAKAMA_BASE_URL` stored as Pages secrets).
3. On success: render full HTML with populated OG tags and crew data.
4. On `NOT_FOUND`: render a minimal branded "invite not found" page.

### 8.2 Open Graph tags

```html
<meta property="og:title"        content="Join {crew_name} on m3llo" />
<meta property="og:description"  content="{online_count} online · {member_count} members" />
<meta property="og:image"        content="https://avatar.m3llo.app/og/{code}.png" />
<meta property="og:image:width"  content="1200" />
<meta property="og:image:height" content="630" />
<meta property="og:url"          content="https://m3llo.app/join/{code}" />
<meta property="og:type"         content="website" />
<meta name="twitter:card"        content="summary_large_image" />
```

### 8.3 Page content

**Above the fold:**
- Crew avatar (from `avatar.m3llo.app/{seed}.png`)
- Crew name, online count, member count
- Primary CTA: **"Open in m3llo"** — fires `mello://join/{code}`
- Secondary CTA: **"Download m3llo"** — links to `https://m3llo.app/download`

**"Open in m3llo" JS behaviour:**
```javascript
window.location.href = 'mello://join/{code}';
// If the tab is still visible after 2s, m3llo isn't installed
setTimeout(() => showDownloadPrompt(), 2000);
window.addEventListener('blur', () => clearTimeout(t)); // app opened, tab lost focus
```

**Below the fold:**
- "Voice, streaming and all your crew's best gaming moments. Open source. Made in Europe."
- m3llo wordmark

Design: dark background `#0D0D0F`, `#EB4D5F` accents, Oxanium for headings, Barlow for body.

---

## 9. OG Image Generator (Cloudflare Worker)

**Worker:** `avatar.m3llo.app` (existing worker, new route)

**Route:** `GET /og/{code}.png`

### 9.1 Pipeline

1. Check KV cache for `og:{code}`. If hit and not stale (TTL: 5 minutes), return cached PNG immediately.
2. Call `resolve_crew_invite` on Nakama with server key.
3. Fetch the crew avatar PNG from `avatar.m3llo.app/{seed}.png` (existing route).
4. Build SVG card (§9.2).
5. Rasterize to 1200×630 PNG using `resvg` (existing dependency).
6. Write to KV with 5-minute TTL.
7. Return with `Content-Type: image/png`, `Cache-Control: public, max-age=300`.

### 9.2 SVG card layout

```
┌──────────────────────────────────────────────────────────────────┐  1200×630px
│                                                                  │
│   [avatar 120×120]   {crew_name}                    m3llo        │
│   rounded square     Oxanium 48px white             Audiowide    │
│                                                     22px #EB4D5F │
│                      ● {n} online · {m} members                  │
│                      Barlow 28px #888  #EB4D5F dot               │
│                                                                  │
│   Background #0D0D0F                                             │
└──────────────────────────────────────────────────────────────────┘
```

- Avatar embedded as base64 data URI in SVG `<image>`, `rx="16"` for rounded corners
- All fonts embedded as base64 — the Worker has no system fonts
- Subset Oxanium, Barlow, and Audiowide to Latin characters to keep total Worker bundle under 1MB

---

## 10. Implementation Order

1. **Backend:** `resolve_crew_invite` RPC + confirm invite code is returned in crew details for all members (§5)
2. **OG image Worker route** (`/og/{code}.png` on `avatar.m3llo.app`)
3. **Landing page** (Cloudflare Pages Function at `m3llo.app/join/[code]`)
4. **Client deep link wiring** (`DeepLink::Join` dispatch, pending-during-auth logic)
5. **Join confirmation screen** (Slint UI)
6. **Invite modal + "CREW ESTABLISHED" modal update** (Slint UI)

End-to-end smoke test: open the invite modal in the client, copy the link, paste it in Discord (verify OG card renders), click the link in a browser (verify landing page and "Open in m3llo"), click the deep link (verify join confirmation screen appears), join (verify crew membership).

---

## 11. Out of Scope (this version)

- Invite management UI (view, revoke, list per-crew invites)
- Per-invite usage analytics
- Expiring or single-use invites
- Deferred deep link via installer embedding
- Invite link in crew discovery or public directory
