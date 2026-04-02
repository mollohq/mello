# Avatar System Specification

> **Component:** mello-client (Rust/Slint) · Cloudflare Worker · Nakama Backend
> **Status:** Draft
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [01-CLIENT.md](./01-CLIENT.md), [04-BACKEND.md](./04-BACKEND.md), [06-SOCIAL-LOGIN.md](./06-SOCIAL-LOGIN.md)

---

## 1. Overview

Every m3llo user gets a profile avatar. During onboarding (Step 02), users pick from a set of auto-generated illustrated avatars or upload their own image. The generated avatars come from the DiceBear library, rendered by a Cloudflare Worker at `avatar.m3llo.app`. Once the user selects an avatar, the SVG is fetched one final time, baked into Nakama Storage as a stored object, and the user's `avatar_url` is set to the Nakama storage path. After onboarding, no external dependency exists. Every client reads avatars from Nakama only.

### 1.1 Goals

- Every user has a unique, visually distinct avatar from their first session
- Avatar selection feels delightful, not like a chore
- Zero runtime dependency on external services after onboarding
- Self-hosters are never blocked by m3llo infrastructure unavailability
- Avatars render crisply at all sizes (32px chat, 48px voice channel, 140px profile)

### 1.2 Non-Goals

- User-customizable avatar parts (hair, eyes, etc.). DiceBear handles variation via seed.
- Animated avatars
- Avatar marketplace or unlockable styles (future Aura perk consideration)

---

## 2. DiceBear Styles

The client uses a curated subset of DiceBear styles. During avatar generation, each of the 7 grid slots randomly picks a style from this set, creating a mixed grid.

### 2.1 Approved Styles

| Style ID | License | Character |
|---|---|---|
| `adventurer-neutral` | CC BY 4.0 | Illustrated faces, gender-neutral |
| `avataaars-neutral` | Free for personal/commercial | Cartoon people, gender-neutral |
| `fun-emoji` | CC BY 4.0 | Expressive emoji faces |
| `pixel-art` | MIT | Retro pixel characters |
| `thumbs` | CC0 1.0 | Playful hand gestures |

### 2.2 License Compliance

Styles licensed under CC BY 4.0 require attribution. The client includes attribution in Settings > About: "Avatar illustrations by Lisa Wischofsky and Pablo Stanley via DiceBear (dicebear.com), licensed under CC BY 4.0 and CC0 1.0."

---

## 3. Cloudflare Worker: `avatar.m3llo.app`

A lightweight JS worker that imports `@dicebear/core` and the approved style packages. Single endpoint, deterministic output, aggressive caching.

### 3.1 Endpoint

```
GET https://avatar.m3llo.app/:style/svg?seed=:seed
```

**Parameters:**

| Param | Type | Description |
|---|---|---|
| `style` | path | One of the approved style IDs from Section 2.1 |
| `seed` | query | Arbitrary string used to deterministically generate the avatar |

**Response:** SVG content, `Content-Type: image/svg+xml`.

**Cache headers:**

```
Cache-Control: public, max-age=31536000, immutable
```

Output is deterministic (same style + seed = same SVG), so responses are cached at the Cloudflare edge indefinitely.

### 3.2 Validation

The worker validates `style` against the approved list. Unknown styles return `400 Bad Request`. Seeds longer than 128 characters are rejected.

### 3.3 Error Handling

| Condition | Response |
|---|---|
| Invalid style | `400 { "error": "unknown_style" }` |
| Seed too long | `400 { "error": "seed_too_long" }` |
| Internal error | `500 { "error": "generation_failed" }` |

### 3.4 Deployment

- **Region:** Cloudflare edge (auto-distributed)
- **Domain:** `avatar.m3llo.app` via Cloudflare DNS
- **Runtime:** Cloudflare Workers (free tier sufficient for beta)
- **Dependencies:** `@dicebear/core`, style packages from `@dicebear/collection`

---

## 4. Onboarding UX: Step 02

Avatar selection lives in onboarding Step 02 ("Profile & Device Setup"), alongside nickname input and audio device selection.

### 4.1 Seed Generation

Avatars must render immediately when Step 02 loads, before the user has entered a nickname. The client generates a random session seed on Step 02 init (a UUID v4 via the `uuid` crate, stored in memory only). All avatar seeds are derived from this session seed:

```
seed = "{session_seed}_r{roll_counter}_{slot_index}"
```

Example: `"a7f3c9e1-4b2d-4f8a-9c3e-1d5f7a8b2c4d_r0_3"`. The session seed is ephemeral and discarded after onboarding. Only the final selected avatar's SVG data is persisted.

### 4.2 Grid Layout

4 columns, 2 rows. 7 avatar slots + 1 upload slot (bottom-right).

### 4.3 Initial Load

When Step 02 renders:

1. Client generates the session seed (`Uuid::new_v4()`)
2. For each of the 7 slots, client randomly picks a style from the approved list (Section 2.1) and constructs the seed string
3. Client fires 7 parallel async HTTP GET requests (via `reqwest`) to `avatar.m3llo.app/{style}/svg?seed={seed}`
4. As each response arrives, the SVG string is parsed into a pixel buffer via `resvg` and set as the slot's `Image` source
5. Each slot plays its deal-in animation on image load, staggered by slot index (details in Section 4.9)
6. No avatar is pre-selected
7. If all 7 requests fail (network unavailable), fall back to self-hoster behavior (Section 7)

### 4.4 Ambient Shuffle

A Slint `Timer` fires every 3 seconds. On each tick:

1. Pick a random unselected slot that is not currently mid-flip
2. Generate a new seed string (incrementing a shuffle counter) and randomly pick a new style
3. Fire an async HTTP request to the worker for the new avatar
4. On response, trigger the card flip animation on that slot (details in Section 4.9)
5. Mark the slot as "flipping" to prevent it from being picked again until the animation completes (~1.1 seconds)

The ambient shuffle timer starts on Step 02 load and runs continuously until the user selects an avatar.

### 4.5 Selection

Clicking a slot selects it. The selected slot shows a highlight border (accent color, 1.5px, with a second 1px accent border as outer glow). The ambient shuffle timer stops immediately. The selected slot is never flipped.

Clicking the selected slot again deselects it. The ambient shuffle timer restarts.

### 4.6 Reroll

A "Reroll all" button below the grid (dice icon + label). On click:

1. Clear any current selection
2. Increment the roll counter
3. Generate 7 new seed strings with the new roll counter, each with a randomly assigned style
4. Fire 7 parallel async requests to the worker
5. Stagger-flip all 7 cards as responses arrive (100ms delay offset per slot)
6. Dice icon `rotation-angle` animates from current to current + 360 degrees over 500ms
7. Ambient shuffle timer restarts after all cards have landed (~1.9 seconds)

### 4.7 Upload Slot

The 8th slot (bottom-right) has a dashed border, a "+" icon, and "Upload" label. Click opens a native file dialog via the `rfd` crate, filtered to `.png`, `.jpg`, `.jpeg`, `.webp`. The selected image is loaded via the `image` crate, resized to 256x256 pixels (center crop via `imageops::resize` with `CatmullRom` filter), and stored in memory as the avatar candidate. No crop UI for beta, center-crop is sufficient.

### 4.8 Easter Egg: Hold-to-Spin

A `PointerEvent` press on any avatar card starts a hold timer (Slint `Timer`, single-shot, 3 second delay). If the pointer is released or leaves the element before the timer fires, the timer cancels and normal click/select behavior applies. If the timer fires:

1. Ambient shuffle timer stops
2. The held card enters "spin mode": a Slint `Timer` in `Repeating` mode at ~16ms interval drives manual updates to the card's `rotation-angle` property
3. The card spins around the Y axis, starting at 2.8 degrees per tick, ramping up by 0.15 degrees per tick to a max of 8.4 degrees per tick
4. Each time the card crosses a 180-degree boundary (angle mod 360 crosses between first and second half), the visible face content swaps to the next word in the sequence
5. Word sequence: "hey", "there,", "we", "love", "you", "for", "being", "here", followed by a heart symbol
6. Each word is held for 3 consecutive half-rotations before advancing (the word list contains each word tripled). The heart is held for 4 half-rotations.
7. Words render in Oxanium 600 weight, 16px, accent color, centered on the card face. Heart renders at 28px.
8. On pointer release, the repeating timer callback switches to deceleration mode: speed multiplied by 0.96 each tick until speed drops below 0.5 degrees per tick
9. Card snaps `rotation-angle` to 0 degrees, a new avatar is fetched and placed on the front face, normal animation behavior resumes
10. Ambient shuffle timer restarts if no avatar is selected

This easter egg is undocumented. Discovery is the reward.

### 4.9 Animations

All animations are implemented in Slint using property bindings, `animate` blocks, and `Timer` callbacks.

**Deal-in (initial load):** Each slot animates from `opacity: 0, scale-x/scale-y: 0.5, y-offset: 12px` to `opacity: 1, scale: 1.0, y-offset: 0` with a slight overshoot to `scale: 1.06` at the midpoint. Duration: 350ms. Easing: `cubic-bezier(0.34, 1.56, 0.64, 1)`. Stagger: each slot's animation start is delayed by `slot_index * 60ms`, controlled by setting a per-slot `visible` or `animate-trigger` property from Rust via a `Timer` callback.

**Card flip (ambient shuffle and reroll):** Implemented using Slint's `rotation-angle` and `rotation-axis` properties on the card container. Two child elements represent front and back faces. The back face has its own `rotation-angle` offset by 180 degrees.

Slint does not support `backface-visibility: hidden` or `transform-style: preserve-3d`. To fake the 3D card flip: animate `rotation-angle` from 0 to 180 (or 180 to 0). At the 90-degree midpoint, swap which face is visible using a conditional `opacity` or `visible` binding:

```
// Pseudocode for face visibility
front-face.visible: rotation-angle < 90deg || rotation-angle > 270deg
back-face.visible: rotation-angle >= 90deg && rotation-angle <= 270deg
```

Additionally, apply a horizontal scale factor to simulate perspective foreshortening:

```
// Pseudocode: scale-x narrows at 90/270, full width at 0/180
scale-x: cos(rotation-angle).abs()
```

This combination of face-swapping and horizontal scaling produces a convincing card-flip illusion without true 3D transforms.

Duration: 1040ms. Easing: `cubic-bezier(0.4, 0.0, 0.2, 1)`. The `rotation-angle` property is animated via a Slint `animate` block when triggered by the Rust callback.

**Reroll dice spin:** The dice icon's `rotation-angle` animates from 0 to 360 degrees over 500ms with overshoot easing.

### 4.10 Continue Action

When the user clicks Continue with an avatar selected:

1. If a generated avatar: the SVG string is already in memory from the fetch. No additional request needed.
2. If an uploaded image: the cropped PNG data is already in memory.
3. Client writes the avatar to Nakama Storage (Section 5).
4. Client sets the user's `avatar_url` via Nakama `UpdateAccount`.
5. Client transitions to Step 03 (social login).

If no avatar is selected and Continue is pressed: proceed without an avatar. The user's `avatar_url` remains empty and the client renders a fallback (Section 6.3).

---

## 5. Nakama Storage

### 5.1 Avatar Object

| Field | Value |
|---|---|
| Collection | `avatars` |
| Key | `current` |
| User ID | Authenticated user's ID |
| Value | `{ "format": "svg", "data": "<svg>...</svg>", "style": "adventurer-neutral", "seed": "a7f3c9e1-..._r3_2" }` |
| Read permission | Public read (2) |
| Write permission | Owner only (1) |

For uploaded images, `format` is `"png"`, `data` is a base64-encoded string, and `style` and `seed` are omitted.

### 5.2 Avatar URL

The user's `avatar_url` (set via Nakama `UpdateAccount`) is a predictable path:

```
/v2/storage/avatars/current/{user_id}
```

Any client rendering an avatar fetches this path from their Nakama instance. The response contains the stored object including the SVG or PNG data.

### 5.3 Client-Side Caching

The client caches fetched avatars in memory (`HashMap<String, slint::Image>`) for the duration of the session, keyed by `user_id`. A crew of 6 means 6 cached entries. Cache is invalidated when the presence system reports an avatar change event (user updates their avatar mid-session).

### 5.4 Size Limits

SVG avatars from DiceBear are typically 2-8 KB. Uploaded images are resized client-side to 256x256 pixels before encoding, keeping uploads under 50 KB. The Nakama storage object value field has a 64 KB default limit which accommodates both cases.

---

## 6. Rendering

### 6.1 Display Sizes

| Context | Size | Shape |
|---|---|---|
| Chat message | 32x32 px | Rounded square (6px radius) |
| Voice channel member | 40x40 px | Rounded square (8px radius) |
| Crew member list | 36x36 px | Rounded square (6px radius) |
| Profile card / onboarding grid | 80-140 px | Rounded square (8-12px radius) |

### 6.2 Rendering in Slint

For SVG avatars: the SVG string fetched from the worker (or read from Nakama Storage) is parsed and rasterized into an RGBA pixel buffer using the `resvg` crate. The buffer is created at 2x the target display size for crisp rendering on HiDPI displays. The pixel buffer is converted to a Slint `Image` via `Image::from_rgba8()` and set on the Slint `Image` element's `source` property.

For uploaded PNGs: the base64 data from Nakama Storage is decoded, the raw bytes are loaded via the `image` crate, and converted to a Slint `Image` the same way.

### 6.3 Fallback

If a user has no avatar (empty `avatar_url`), render a colored rounded square with the first two characters of the nickname in uppercase, centered, Oxanium 500 weight. Color is derived from a hash of the user ID, mapped to one of 8 predefined colors:

| Index | Color |
|---|---|
| 0 | `#EB4D5F` (accent) |
| 1 | `#7C5CFC` |
| 2 | `#06B6D4` |
| 3 | `#F472B6` |
| 4 | `#10B981` |
| 5 | `#F59E0B` |
| 6 | `#8B5CF6` |
| 7 | `#EC4899` |

This fallback is implemented as a Slint component (`AvatarFallback`) that takes `nickname: string` and `user-id: string` as input properties and renders the colored square with initials. It is used anywhere an `Image` element would display an avatar, as a conditional fallback.

---

## 7. Self-Hoster Behavior

Self-hosted m3llo instances may not be able to reach `avatar.m3llo.app`. The client handles this gracefully.

### 7.1 Onboarding Flow

1. Client attempts to fetch avatars from `avatar.m3llo.app` as normal
2. If requests fail (network error, timeout after 3s), the generated avatar slots are hidden
3. The grid collapses to show only the upload slot, presented as a larger, more prominent drop zone
4. The nickname-based fallback (initials in colored square) is shown as a preview so the user knows they'll still have a visual identity
5. A subtle note below: "Connect to m3llo.app for generated avatars, or upload your own"

### 7.2 No Hard Dependency

The avatar generation worker is a convenience, not a gate. Self-hosters can always:

- Upload their own image
- Proceed without an avatar (fallback renders everywhere)
- Change their avatar later from settings

### 7.3 Avatar Storage

Self-hosters store avatars in their own Nakama instance identically to hosted users. The storage schema (Section 5) is the same. No data leaves the self-hoster's infrastructure after onboarding.

---

## 8. Settings: Change Avatar

Post-onboarding, users can change their avatar from Settings > Profile.

### 8.1 UI

Same grid layout and mechanics as onboarding (7 generated + 1 upload, reroll, ambient shuffle, selection). Clicking "Save" writes the new avatar to Nakama Storage, replacing the previous one.

### 8.2 Propagation

When a user updates their avatar, their presence data includes an updated timestamp. Other clients in the same crew detect the change and refetch the avatar from Nakama Storage, replacing their cached version.

---

## 9. Data Flow Summary

```
ONBOARDING                          RUNTIME

 ┌─────────┐   seed+style   ┌──────────────────┐
 │  Client  │ ─────────────> │  CF Worker        │
 │  Step 02 │ <───────────── │  avatar.m3llo.app │
 │          │   SVG string   └──────────────────┘
 │          │
 │  User    │   baked SVG    ┌──────────────────┐
 │  clicks  │ ─────────────> │  Nakama Storage   │
 │ Continue │                │  avatars/current  │
 └─────────┘                └──────────────────┘
                                      │
                                      │ read
                                      ▼
                              ┌──────────────┐
                              │ Other clients │
                              │ in the crew   │
                              └──────────────┘
```

---

## 10. Dependencies

### 10.1 Client (Rust)

| Crate | Purpose |
|---|---|
| `resvg` | SVG parsing and rasterization to pixel buffer |
| `rfd` | Native file dialog for upload slot |
| `image` | Image resize/crop for uploaded photos |
| `uuid` | Session seed generation |
| `reqwest` | HTTP requests to CF worker (already a dependency) |

### 10.2 Cloudflare Worker (JS)

| Package | Purpose |
|---|---|
| `@dicebear/core` | Avatar generation engine |
| `@dicebear/collection` | All approved style packages |

---

## 11. Implementation Priority

| Task | Effort | Priority |
|---|---|---|
| Cloudflare Worker (avatar.m3llo.app) | Small (2-3 hours) | P0 |
| Onboarding grid + deal-in animation | Medium (1-2 days) | P0 |
| Ambient shuffle + reroll | Small (half day) | P0 |
| Selection + bake to Nakama | Small (half day) | P0 |
| Upload slot with center crop | Small (half day) | P0 |
| Self-hoster fallback (hide grid on failure) | Small (2 hours) | P1 |
| Easter egg hold-to-spin | Small (half day) | P2 |
| Settings > Change Avatar | Medium (reuse onboarding) | P1 |
| Initials fallback renderer | Small (2 hours) | P0 |

---

*This spec defines the avatar system. For onboarding flow context, see [06-SOCIAL-LOGIN.md](./06-SOCIAL-LOGIN.md). For presence propagation, see [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md).*
