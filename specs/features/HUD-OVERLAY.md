# HUD Overlay Specification

> **Component:** Crew HUD (Client, Windows)
> **Version:** 0.4
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

On Windows, the taskbar thumbnail toolbar provides mute/deafen/leave controls when hovering over the m3llo icon in the taskbar.

**Out of scope:**
- Chat input
- Clip history browsing
- Stream viewer
- In-game mouse interaction
- Mini-player as a standalone window (removed — replaced by taskbar toolbar on Windows, menubar app on macOS)

---

## 2. Architecture

### 2.1 In-client overlay thread

The overlay runs on a dedicated thread within the main `mello.exe` process. No separate binary is built or shipped.

```
mello.exe
 ├── main thread (Slint UI)
 ├── poll loop thread
 └── hud-overlay thread  (Win32 message loop + D2D rendering)
       └── mpsc::Receiver<HudMessage>
```

`HudManager` owns the `mpsc::Sender` and pushes state/settings/shutdown messages. The overlay thread creates its Win32 window, sets up the DComp rendering pipeline, and enters a message loop that drains both Win32 messages and channel messages every ~16ms.

### 2.2 Message protocol

All communication uses `mpsc::channel<HudMessage>`. Three message variants:

**State push:**
```rust
HudMessage::State(Box<HudState>)
```

`HudState` contains: `mode` (Hidden/Overlay), `crew` (name, initials, avatar), `voice` (channel name, members with speaking/muted/streaming state, avatars), `stream_card`, `clip_toast`.

Avatars are pre-rasterized 24×24 RGBA bitmaps, base64-encoded by the main client. The overlay never fetches or decodes images itself.

**Settings push:**
```rust
HudMessage::Settings(HudSettings { overlay_opacity, show_clip_toasts })
```

Applied instantly — opacity updates the D2D panel background alpha, clip toast flag suppresses/enables toast display.

**Shutdown:**
```rust
HudMessage::Shutdown
```

### 2.3 Visibility rules

| Condition | Mode |
|---|---|
| User not in a voice channel | Hidden |
| m3llo main window focused | Hidden |
| HUD disabled in settings | Hidden |
| Fullscreen app covers monitor | Hidden (suppressed) |
| Any other foreground window | Overlay |

When a fullscreen application covers the entire monitor (detected via `GetWindowRect` + `MonitorFromWindow`), the overlay is hidden to avoid blocking input behind an invisible overlay.

---

## 3. Overlay

### 3.1 Window setup

```
WS_EX_NOREDIRECTIONBITMAP | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW
```

- `WS_EX_NOREDIRECTIONBITMAP` — required for DirectComposition rendering
- `WS_EX_LAYERED` + `WS_EX_TRANSPARENT` — cross-process click-through (input passes to windows behind)
- `WS_EX_TOPMOST` — always above other windows (re-asserted each tick)
- `WS_EX_NOACTIVATE` — never steals focus
- `WS_EX_TOOLWINDOW` — excluded from taskbar and Alt+Tab

After creation, `SetLayeredWindowAttributes(alpha=255)` activates the layered window at full opacity — DComp handles actual content compositing.

### 3.2 Rendering pipeline

DirectComposition (DComp) with a D3D11/DXGI swap chain and D2D1 device context:

1. D3D11 device → DXGI device → DComp device
2. DXGI swap chain (FLIP_SEQUENTIAL, premultiplied alpha) bound to a DComp visual
3. DComp visual tree: target → visual → swap chain
4. D2D1 device context renders to a bitmap created from the swap chain's back buffer
5. Present + DComp Commit

The overlay redraws only on state change (`needs_render` flag). There is no fixed frame-rate render loop.

### 3.3 Visual design

Panel: dark semi-transparent background (`rgba(0,0,0, <user opacity>)`) with rounded corners (10px) and subtle border (`rgba(255,255,255,0.08)`). Width: 230px. Height: dynamic based on member count.

**Panel layout, top to bottom:**

1. **Header row** (24px)
   - Crew avatar (22×22px, 5px radius rounded rect): shows bitmap if available, falls back to accent-tinted rounded rect with initials
   - Crew name (Inter semi-bold, 12px, white)

2. **Channel name** (16px) — `.:: channel-name ::.` in Inter 11px, muted grey

3. **Separator** — 1px line, `rgba(255,255,255,0.10)`

4. **Member rows** (26px each, 4px gap) — one per voice channel member
   - Avatar (20×20px, 5px radius rounded rect): bitmap if available, otherwise initials in a state-colored background
     - Speaking: green-tinted bg (`#10B981` at 20%), green border, bitmap gets green border ring
     - Muted: dark bg, dimmed initials
     - Idle: dark bg, grey initials
   - Display name (Inter medium/semi-bold 13px): white for speaking, light grey for idle, muted grey for muted
   - LIVE indicator (next to streaming member's name): red dot + "LIVE" text with 1px red border, no background fill
   - State indicator (right side): 3-bar animation for speaking, mic-slash icon for muted

5. **Clip toast** (when active, 26px) — accent pill with white text. Auto-dismissed after 4 seconds.

### 3.4 Position & dragging

Default: top-left corner of the primary monitor, inset 16px from each edge.

The overlay is repositionable via a **grip window** — a separate 20×20 Win32 window that appears at the overlay's top-right corner when the user hovers over the overlay. The grip:
- Is NOT click-through (no `WS_EX_TRANSPARENT`)
- Returns `HTCAPTION` from `WM_NCHITTEST` for native Win32 drag
- Moves the main overlay via `SetWindowPos` in its `WM_MOVE` handler
- Shows `IDC_SIZEALL` cursor
- Auto-hides when the cursor leaves the overlay area

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
| Enable HUD | On | Disabling hides the overlay |
| Overlay opacity | 80% | Scales alpha of the overlay panel background (clamped 10%–100%) |
| Clip toast notifications | On | Suppresses clip toasts in the overlay when off |

Settings changes are pushed over the channel instantly and applied in the same render tick.

---

## 6. Performance Constraints

| Metric | Target |
|---|---|
| RAM overhead (overlay visible, idle) | < 15 MB |
| CPU (nothing speaking) | < 0.1% |
| CPU (speaking animation active) | < 0.5% |
| GPU (nobody speaking) | 0% |

The HUD must have zero measurable impact on game frame rate or input latency.

---

## 7. Failure Handling

| Failure | Behaviour |
|---|---|
| Overlay thread panics | Main client continues running; HUD unavailable until restart |
| Avatar bitmap missing or corrupt | Render initials fallback: rounded rect with state-colored background, initials centered |
| Direct2D/DComp device lost | Overlay thread exits; HUD unavailable until restart |

---

## 8. Future Work

- **macOS mini-player menubar app** — Reuse existing mini-player Slint code (retained in `mello/hud/src/mini_player/`) to build a macOS menubar popover that shows crew, voice, chat, and stream info
- Persist overlay position across sessions
- Stream clip toast with thumbnail preview
- Push-to-talk indicator in overlay
- Per-monitor DPI awareness for mixed-scaling multi-monitor setups
- Global hotkey to toggle HUD visibility
