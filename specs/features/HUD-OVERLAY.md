# HUD Overlay Specification

> **Component:** Crew HUD (Client, Windows)
> **Version:** 0.3
> **Status:** Implemented (Windows), Planned (macOS)
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md), [13-VOICE-CHANNELS.md](./13-VOICE-CHANNELS.md)

---

## 1. Overview

The HUD is a small, transparent, click-through overlay that shows crew and voice state when the main m3llo window is not focused. It appears whenever the user is in a voice channel and switches away from m3llo — regardless of whether they're gaming, browsing, or doing anything else.

| Context | What the user sees |
|---|---|
| m3llo focused | HUD hidden |
| Not focused, in voice, HUD enabled | Overlay visible |
| Not in voice / HUD disabled | HUD hidden |

The overlay is implemented in a separate lightweight process: `m3llo-hud.exe`, spawned by the main client on launch. On Windows, the taskbar thumbnail toolbar provides mute/deafen/leave controls when hovering over the m3llo icon in the taskbar.

**Out of scope:**
- Chat input
- Clip history browsing
- Stream viewer
- In-game mouse interaction
- Mini-player as a standalone window (removed — replaced by taskbar toolbar on Windows, menubar app on macOS)

---

## 2. Process Architecture

### 2.1 Process model

`m3llo-hud.exe` is a standalone Rust binary. It is not a DLL, not injected, and never reads or writes to any game process.

The main client spawns `m3llo-hud.exe` on launch and terminates it on exit. If the HUD process crashes, the main client detects the exit and respawns it after 2 seconds.

```
m3llo.exe  ──IPC named pipe──►  m3llo-hud.exe
                                 └── Win32/D2D window  (Overlay)
```

The overlay window is created at startup and persists for the lifetime of the HUD process. It is shown/hidden by the mode manager based on state pushed from the main client.

### 2.2 IPC protocol

Named pipe: `\\.\pipe\m3llo-hud`

All messages are newline-delimited JSON. The main client pushes state; the HUD pushes user actions back.

**State push (main → HUD):**

```json
{
  "type": "state",
  "mode": "overlay",
  "crew": {
    "name": "The Vanguard",
    "initials": "TV",
    "avatar_rgba": "24:24:<base64>",
    "online_count": 5
  },
  "voice": {
    "channel_name": "General",
    "members": [
      {
        "id": "abc",
        "display_name": "k0ji_tech",
        "initials": "KT",
        "avatar_rgba": "24:24:<base64>",
        "speaking": true,
        "muted": false,
        "is_self": false
      }
    ],
    "self_muted": false
  },
  "stream_card": {
    "streamer": "k0ji_tech",
    "title": "PROJECT AVALON"
  },
  "clip_toast": null
}
```

If `voice` is `null`, the user is not in a voice channel and the HUD hides entirely.

The `mode` field tells the HUD whether to show or hide. The main client is the source of truth for mode; the HUD does not independently determine it. Valid modes: `"hidden"`, `"overlay"`.

`avatar_rgba` is a string in format `w:h:<base64_rgba>`, containing a 24×24 RGBA bitmap pre-rasterized and downscaled by the main client. The HUD never fetches or decodes images itself. When `null`, the renderer falls back to initials.

**Settings push (main → HUD):**

```json
{ "type": "settings", "overlay_opacity": 0.8, "show_clip_toasts": true }
```

Settings are applied instantly on receipt — opacity updates the D2D panel background alpha, clip toast flag suppresses/enables toast display.

**User action (HUD → main):**

```json
{ "type": "action", "action": "mute_toggle" }
{ "type": "action", "action": "leave_voice" }
```

### 2.3 Visibility rules

| Condition | Mode |
|---|---|
| User not in a voice channel | Hidden |
| m3llo main window focused | Hidden |
| HUD disabled in settings | Hidden |
| Any other foreground window | Overlay |

---

## 3. Overlay

### 3.1 Window setup

```
WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW
```

- `WS_EX_LAYERED` — per-pixel alpha compositing via `UpdateLayeredWindow`
- `WS_EX_TRANSPARENT` — all input passes through
- `WS_EX_TOPMOST` — always above other windows (re-asserted each tick)
- `WS_EX_NOACTIVATE` — never steals focus
- `WS_EX_TOOLWINDOW` — excluded from taskbar and Alt+Tab

### 3.2 Rendering

Direct2D DC render target, rendered into a memory DC with a 32-bit DIB section. Pixels are premultiplied and composited via `UpdateLayeredWindow` with `ULW_ALPHA`. The window background is fully transparent; only HUD content has opacity.

The HUD redraws only on state change (`needs_render` flag). There is no fixed frame-rate render loop.

### 3.3 Visual design

Panel: dark semi-transparent background (`rgba(0,0,0, <user opacity>)`) with rounded corners (10px) and subtle border (`rgba(255,255,255,0.08)`). Width: 230px. Height: dynamic based on member count.

**Panel layout, top to bottom:**

1. **Header row** (24px)
   - Crew avatar (22×22px, 5px radius rounded rect): shows bitmap if available, falls back to accent-tinted rounded rect with initials
   - Crew name (Inter semi-bold, 12px, white)
   - Online count pill (dark fill, green dot + count in JetBrains Mono 11px)
   - Optional LIVE badge (red pill, 38×16px) when a crewmate is streaming

2. **Channel name** (16px) — `# channel-name` in Inter 11px, muted grey

3. **Separator** — 1px line, `rgba(255,255,255,0.10)`

4. **Member rows** (26px each, 4px gap) — one per voice channel member
   - Avatar (20×20px, 5px radius rounded rect): bitmap if available, otherwise initials in a state-colored background
     - Speaking: green-tinted bg (`#10B981` at 20%), green border, bitmap gets green border ring
     - Muted: dark bg, dimmed initials
     - Idle: dark bg, grey initials
   - Display name (Inter medium/semi-bold 13px): white for speaking, light grey for idle, muted grey for muted
   - State indicator (right side): 3-bar animation for speaking, mic-slash icon for muted

5. **Clip toast** (when active, 26px) — accent pill with white text. Auto-dismissed after 4 seconds.

### 3.4 Position

Default: top-left corner of the primary monitor, inset 16px from each edge.

---

## 4. Taskbar Thumbnail Toolbar (Windows)

On Windows, the main client registers an `ITaskbarList3` thumbnail toolbar with three buttons on the m3llo taskbar icon. These appear when hovering over the icon in the taskbar.

| Button | Action | Icon states |
|---|---|---|
| Mute toggle | Toggles mic mute | Mic icon (green) / Mic-slash (red) |
| Deafen toggle | Toggles deafen | Headphones (green) / Headphones-off (red) |
| Leave voice | Leaves voice channel | Phone-down (red) |

Buttons are only enabled when the user is in a voice channel. Icons are rendered programmatically as 16×16 `HICON`s using GDI. Button clicks are intercepted via window subclassing (`SetWindowSubclass`) and relayed as `ThumbAction` events to the main poll loop.

---

## 5. Settings

Exposed in the main m3llo client under **Settings → HUD**:

| Setting | Default | Notes |
|---|---|---|
| Enable HUD | On | Disabling kills the HUD process entirely |
| Overlay opacity | 80% | Scales alpha of the overlay panel background (clamped 10%–100%) |
| Clip toast notifications | On | Suppresses clip toasts in the overlay when off |

Settings changes are pushed over IPC instantly and applied in the same render tick.

---

## 6. Performance Constraints

| Metric | Target |
|---|---|
| RAM (overlay visible, idle) | < 25 MB |
| CPU (nothing speaking) | < 0.1% |
| CPU (speaking animation active) | < 0.5% |
| GPU (nobody speaking) | 0% |
| Startup time to first frame | < 200ms |

The HUD must have zero measurable impact on game frame rate or input latency.

---

## 7. Failure Handling

| Failure | Behaviour |
|---|---|
| IPC pipe disconnected | HUD hides, polls for reconnect every 2s |
| Avatar bitmap missing or corrupt | Render initials fallback: rounded rect with state-colored background, initials centered |
| Direct2D device lost | Recreate D2D resources, re-render current state |
| HUD process crashes | Main client detects exit, respawns after 2s |
| `m3llo-hud.exe` not found | Main client logs error, suppresses after 4 failed spawn attempts |

---

## 8. Future Work

- **TODO: macOS mini-player menubar app** — Reuse existing mini-player Slint code (retained in `mello/hud/src/mini_player/`) to build a macOS menubar popover that shows crew, voice, chat, and stream info. The mini-player module and its `.slint` UI are kept in the HUD crate specifically for this purpose.
- Stream clip toast with thumbnail preview
- Push-to-talk indicator in overlay
- Per-monitor DPI awareness for mixed-scaling multi-monitor setups
- Global hotkey to toggle HUD visibility
- User-draggable overlay position (persisted)
