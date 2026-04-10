# HUD Overlay & Mini-Player Specification

> **Component:** Crew HUD (Client, Windows-first)
> **Version:** 0.2
> **Status:** Planned
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md), [13-VOICE-CHANNELS.md](./13-VOICE-CHANNELS.md)

---

## 1. Overview

m3llo has three user-facing contexts depending on what the user is doing:

| Context | HUD state |
|---|---|
| Full app focused | HUD hidden |
| Doing something else (no game) | Mini-player visible |
| Game in foreground | Overlay visible |

The HUD process has two states: mini-player and overlay. When the full app is in focus, the HUD is simply hidden. The full app is not a HUD state.

The mini-player and overlay are implemented in a separate lightweight process: `m3llo-hud.exe`. This process is spawned by the main client on launch and killed when the main client exits. Both windows are created at startup and persist for the lifetime of the process. Only one is ever visible at a time.

**Windows only for initial release.** macOS is deferred. The macOS equivalent will be a persistent popover from the menu bar tray item, covered in 12-NATIVE-PLATFORM.md.

**Out of scope for this version:**
- Chat input in any mode (mini-player shows recent messages read-only)
- Clip history browsing
- Stream viewer
- In-game mouse interaction in overlay mode
- macOS

---

## 2. HUD States

### State 1: Mini-Player

The user is doing something else on their computer but not playing a game. m3llo is not in focus. The mini-player is a compact, opaque, always-on-top Slint window that keeps the crew visible without requiring the full app to be open.

The mini-player is a richer surface than the overlay. It shows:
- Crew name and online count
- Full voice channel member list with per-member state
- Last 2 crew chat messages (read-only)
- A "now streaming" card if a crewmate is actively streaming
- Mute and leave-voice controls

Transition in: any non-game window comes into focus while m3llo is not focused.
Transition out: a game process comes into focus → Overlay mode. m3llo window comes into focus → Full app mode (HUD hides).

### State 2: Overlay

A game process is in the foreground. The overlay is a minimal, transparent, click-through window composited over the game by DWM. It shows only what the user needs at a glance during play: who is in voice and who is speaking.

The overlay never intercepts mouse or keyboard input. It is purely informational.

Transition in: game process comes into focus.
Transition out: any non-game window comes into focus → Mini-player mode.

---

## 3. Process Architecture

### 3.1 Process model

`m3llo-hud.exe` is a standalone Rust binary. It is not a DLL, not injected, and never reads or writes to any game process.

The main client spawns `m3llo-hud.exe` on launch and terminates it on exit. If the HUD process crashes, the main client detects the exit and respawns it after 2 seconds.

```
m3llo.exe  ──IPC named pipe──►  m3llo-hud.exe
                                 │
                                 ├── Slint window       (Mini-player mode)
                                 └── Win32/D2D window   (Overlay mode)
```

Both windows are created at startup and persist for the lifetime of the HUD process. Only one is visible at any given time. This ensures zero latency on mode transitions — there is no window creation at switch time.

### 3.2 IPC protocol

Named pipe: `\\.\pipe\m3llo-hud`

All messages are newline-delimited JSON. The main client pushes state; the HUD pushes user actions back.

**State push (main → HUD):**

```json
{
  "type": "state",
  "mode": "mini_player",
  "crew": {
    "name": "The Vanguard",
    "initials": "TV",
    "online_count": 5
  },
  "voice": {
    "channel_name": "General",
    "members": [
      {
        "id": "abc",
        "display_name": "k0ji_tech",
        "initials": "KT",
        "avatar_rgba": "<base64>",
        "speaking": true,
        "muted": false,
        "self": false
      }
    ],
    "self_muted": false
  },
  "recent_messages": [
    { "display_name": "k0ji", "text": "Anyone up for ranked later?" },
    { "display_name": "nova", "text": "Yeah im down, give me 10" }
  ],
  "stream_card": {
    "streamer": "k0ji_tech",
    "title": "PROJECT AVALON"
  },
  "clip_toast": null
}
```

If `voice` is `null`, the user is not in a voice channel and the HUD hides entirely.

The `mode` field tells the HUD which window to show. The main client is the source of truth for mode; the HUD does not independently determine it.

`recent_messages` and `stream_card` are only populated in `mini_player` mode. They are omitted or null in `overlay` mode to keep IPC payloads minimal.

`avatar_rgba` is a base64-encoded 32×32 RGBA bitmap pre-rasterized by the main client. The HUD never fetches or decodes images itself.

**Clip toast:**

```json
{
  "type": "state",
  "mode": "overlay",
  "clip_toast": { "label": "Clip saved" },
  "..."
}
```

The HUD manages the toast timer (3 seconds) and dismisses it without a further message from the main client.

**User action (HUD → main):**

```json
{ "type": "action", "action": "mute_toggle" }
{ "type": "action", "action": "leave_voice" }
```

### 3.3 Visibility rules

| Condition | Mode |
|---|---|
| User not in a voice channel | Hidden entirely |
| m3llo main window focused | Hidden (full app is visible) |
| Game process in foreground | Overlay |
| Any other foreground window | Mini-player |
| HUD manually dismissed by user | Hidden until voice channel changes or user re-enables from tray |

---

## 4. Game Detection

The HUD does not independently poll for game processes. The main client determines the foreground window using the same game-sensing logic from the presence pipeline (spec 17) and includes the resolved `mode` in every state push.

When the foreground window changes, the main client sends a state update with the new `mode`. The HUD acts on it immediately.

---

## 5. Overlay Mode

### 5.1 Window setup

```
WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW
```

- `WS_EX_LAYERED` — per-pixel alpha compositing via DWM
- `WS_EX_TRANSPARENT` — all input passes through to the game
- `WS_EX_TOPMOST` — always above other windows
- `WS_EX_NOACTIVATE` — never steals focus
- `WS_EX_TOOLWINDOW` — excluded from taskbar and Alt+Tab

### 5.2 Rendering

Direct2D onto a DXGI swap chain. Format: `DXGI_FORMAT_B8G8R8A8_UNORM` with premultiplied alpha. `DXGI_SWAP_EFFECT_FLIP_DISCARD`. The window background is cleared to fully transparent on each frame; only HUD content has any opacity.

The HUD redraws only on state change. There is no fixed frame rate render loop. The only continuous animation is the speaking EQ bars; these drive redraws at approximately 60fps only while at least one member is speaking. When nobody is speaking, the HUD is fully static and consumes no GPU.

### 5.3 Visual design

Glass panel: dark semi-transparent background (`rgba(0,0,0,0.45)`) with backdrop blur. Rounded corners (12px). Subtle border (`rgba(255,255,255,0.08)`). Drop shadow.

**Panel layout, top to bottom:**

1. **Header row**
   - Crew initials monogram (JetBrains Mono, bold, 10px, accent color `#EB4D5F`)
   - Crew name (Barlow bold, 12px, white)
   - Online count pill (dark fill, green dot `#10B981` with glow, count in JetBrains Mono 9px, `#A1A1AA`)

2. **Divider** — 1px horizontal gradient, transparent → `rgba(255,255,255,0.10)` → transparent

3. **Member rows** — one row per member in the voice channel
   - Avatar square (18×18px, 4px radius): speaking members get a green-tinted background (`#10B981` at 20% opacity) and green border with glow; muted members render at 50% opacity with a dimmed background; idle members get a plain dark fill
   - Display name: Inter semi-bold, 11px. White for speaking, `#D4D4D8` for idle, `#A1A1AA` for muted
   - State indicator (right side): 3-bar EQ animation for speaking, muted mic icon in `#EB4D5F` for muted, empty for idle

**Speaking EQ animation:**

Three bars, each 2px wide, `#10B981`, rounded ends. Heights animate independently:

| Bar | Min height | Max height | Duration | Delay |
|---|---|---|---|---|
| 1 | 3px | 12px | 0.7s | 0s |
| 2 | 4px | 9px | 0.5s | 0.1s |
| 3 | 3px | 12px | 0.6s | 0.2s |

Easing: ease-in-out. Animation only runs while `speaking: true` for that member.

4. **Clip toast** (when active) — pill below the member list. Dark background, white text `Clip saved ✓`. Fades in 150ms, holds 3s, fades out 300ms.

Maximum members shown: 6. Beyond 6, show first 5 and a `+N` label in the final slot.

Total overlay footprint at 5 members: approximately 220px × 160px.

### 5.4 Position

Default: top-left corner of the primary monitor, inset 16px from each edge. User-draggable (drag targets the panel itself, requires briefly toggling `WS_EX_TRANSPARENT` off during drag, restoring immediately after). Position persisted as `hud.overlay.x` / `hud.overlay.y`. Independent from mini-player position.

---

## 6. Mini-Player Mode

### 6.1 Window setup

Standard Slint window with Win32 extended styles applied via raw HWND after creation:

```
WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW
```

Opaque. Draggable by clicking anywhere on the background. Excluded from taskbar and Alt+Tab.

### 6.2 Visual design

Same glass aesthetic as the overlay but fully opaque. Dark background, rounded corners, consistent with the main app's visual language.

**Layout, top to bottom:**

1. **Header row**
   - Crew monogram avatar square
   - Crew name (bold, 13px)
   - Online count (green dot + count)
   - Collapse/expand chevron (top-right)

2. **Voice member list** — same row design as the overlay. Speaking EQ bars, muted icon, idle = no indicator. Members sorted: speaking first, then idle, then muted.

3. **Divider**

4. **Recent chat** (last 2 messages, read-only)
   - Username in accent color `#EB4D5F`, message text in muted white
   - Clicking this area sends an `open_crew` action to the main client, which brings the main window into focus on that crew

5. **Stream card** (if a crewmate is actively streaming, otherwise hidden)
   - Dark thumbnail placeholder rect
   - Streamer name label + stream title in bold
   - External link icon — sends `open_stream` action to main client on click

6. **Footer row** (connection info + controls)
   - Self avatar + `Connected · Xms` latency in JetBrains Mono, 9px, `#A1A1AA`
   - Mute toggle (mic icon: green dot = unmuted, red = muted)
   - Leave voice button (red phone-down icon)

**Compact state** (default): header + member list only. Approximately 240px wide, height scales with member count.

**Expanded state**: toggled by the chevron. Reveals chat, stream card, and footer. Approximately 240px × 380px at typical usage.

### 6.3 Clip toast

Pill overlapping the bottom edge of the mini-player window. Slides up from below, holds, fades out. Same timing as overlay (150ms in, 3s hold, 300ms out). Does not resize the window.

### 6.4 Position

Default: bottom-right of primary monitor, inset 20px from each edge. Draggable. Persisted as `hud.miniplayer.x` / `hud.miniplayer.y`. Independent from overlay position.

---

## 7. Mode Switching

```
Foreground window changes
          │
          ▼
  Main client evaluates
          │
          ├── m3llo window focused?    → mode: "hidden"
          ├── known game process?       → mode: "overlay"
          └── anything else?           → mode: "mini_player"
```

On receipt of a mode change:

1. Hide the currently visible HUD window (`ShowWindow(SW_HIDE)`)
2. Show the target window (`ShowWindow(SW_SHOWNOACTIVATE)`)
3. No animation. Instant.

If mode is `"hidden"`, both windows are hidden.

---

## 8. Settings

Exposed in the main m3llo client under **Settings → HUD**:

| Setting | Default | Notes |
|---|---|---|
| Enable HUD | On | Disabling kills the HUD process entirely |
| Show overlay in-game | On | If off, mini-player is used even when a game is focused |
| Overlay opacity | 80% | Scales alpha of all overlay content |
| Overlay position | Top-left | Preset or drag-to-custom |
| Show clip toasts | On | Applies to both modes |

Settings changes are pushed over IPC:

```json
{ "type": "settings", "overlay_opacity": 0.8, "show_clip_toasts": true, "overlay_enabled": true }
```

---

## 9. Performance Constraints

| Metric | Target |
|---|---|
| RAM (mini-player visible, idle) | < 20 MB |
| RAM (overlay visible, not animating) | < 25 MB |
| CPU (nothing speaking) | < 0.1% |
| CPU (speaking animation active) | < 0.5% |
| GPU (overlay, nobody speaking) | 0% |
| Startup time to first frame | < 200ms |

The HUD must have zero measurable impact on game frame rate or input latency.

---

## 10. Failure Handling

| Failure | Behaviour |
|---|---|
| IPC pipe disconnected | HUD hides both windows, polls for reconnect every 2s |
| Avatar bitmap missing or corrupt | Render initials fallback: filled square in a deterministic color derived from user ID, initials in white |
| Direct2D device lost | Recreate device and swap chain, re-render current state |
| HUD process crashes | Main client detects exit, respawns after 2s |
| Game exits while overlay visible | Main client sends `mode: "mini_player"` on next foreground change |

---

## 11. Future Considerations (Out of Scope Now)

- Stream clip toast with thumbnail preview (when stream clipping ships)
- Push-to-talk indicator in overlay (mic icon illuminates while PTT held)
- Chat message input in mini-player
- macOS equivalent via tray popover (12-NATIVE-PLATFORM.md)
- Per-monitor DPI awareness for mixed-scaling multi-monitor setups
- Global hotkey to toggle HUD visibility without opening the main window
