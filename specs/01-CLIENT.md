# MELLO Client Specification

> **Component:** Desktop Client (Slint UI)  
> **Language:** Rust  
> **Status:** Beta Scope  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

The Mello client is a native desktop application built with Slint UI in Rust. It provides the user interface for crew management, voice chat, text chat, stream viewing, and onboarding. All application logic lives in `mello-core`; the client is purely a UI shell that sends `Command`s and reacts to `Event`s.

---

## 2. Technology Choices

| Aspect | Choice | Rationale |
|--------|--------|-----------|
| Language | Rust | Memory safety, small binaries, modern ecosystem |
| UI Framework | Slint | Declarative, native performance, <2MB, Apache 2.0 |
| Async Runtime | Tokio | Industry standard for Rust async |
| State Management | Slint reactive properties + Rust state | Properties for UI, `Rc<RefCell>` for UI-thread, `Arc<Mutex>` for cross-thread |
| Settings Persistence | `confy` (TOML) | Simple, cross-platform config file |

---

## 3. IPC Architecture

The client uses a `Command`/`Event` IPC pattern to communicate with `mello-core`:

```
┌─────────────────────┐        Command (mpsc::Sender)       ┌──────────────────┐
│                     │ ──────────────────────────────────▶  │                  │
│   Slint UI thread   │                                      │  mello-core      │
│   (main.rs)         │  ◀──────────────────────────────────  │  (async loop)    │
│                     │        Event (mpsc::Receiver)         │                  │
└─────────────────────┘                                      └──────────────────┘
```

- **Commands** are sent from Slint callbacks (button clicks, input changes) into the core's async run loop. Examples: `CreateCrew`, `JoinVoice`, `SendMessage`, `SearchUsers`.
- **Events** are received on the UI thread via polling and update Slint properties. Examples: `CrewCreated`, `MessageReceived`, `VoiceActivity`, `StreamFrame`, `VoiceSfuDisconnected`.
- Slint `on_*` callbacks are organized in `callbacks/` submodules (auth, crew, voice, chat, settings, streaming, onboarding) and wired at startup. A timer-driven event loop in `poll_loop.rs` drains the event receiver and dispatches to `handlers/` submodules which update Slint properties. Shared state lives in an `AppContext` struct (`app_context.rs`) threaded through all modules.

### State ownership

| State type | Mechanism | Example |
|-----------|-----------|---------|
| UI-only, single-thread | `Rc<RefCell<T>>` | Invited users list, discover cursor, loading flags |
| Cross-thread (UI ↔ tokio) | `Arc<Mutex<T>>` | Avatar base64 data (picked on main thread, sent via Command) |
| Persistent across restarts | `Settings` struct via `confy` | Audio device IDs, onboarding step, pending crew details |
| Slint-managed | `in`/`in-out` properties | Crew list, chat messages, UI toggles |

---

## 4. UI Structure

### Main Layout

```
┌─────────────────────────────────────────────────────────────────────────┐
│                            MAIN WINDOW                                  │
│  ┌─────────────┐ ┌───────────────────────────────┐ ┌─────────────────┐  │
│  │   CREW      │ │        STREAM VIEW /           │ │    CHAT         │  │
│  │   PANEL     │ │     VOICE CHANNEL VIEW         │ │    PANEL        │  │
│  │             │ │                                 │ │                 │  │
│  │  - Crews    │ │   - Video frames               │ │  - Messages     │  │
│  │  - Members  │ │   - Voice channel members      │ │  - System msgs  │  │
│  │  - Status   │ │   - Stream info                │ │                 │  │
│  └─────────────┘ └─────────────────────────────────┘ └─────────────────┘  │
│  ┌──────────────────────────────────────────────────────────────────┐   │
│  │                        CONTROL BAR                               │   │
│  │  [Avatar] Name    [Mic] [Headphones] [Settings]   [Message...]   │   │
│  └──────────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────┘
```

### Panels and Modals

All UI components live in `client/ui/panels/`:

| File | Description |
|------|-------------|
| `crew_panel.slint` | Left sidebar: crew list (active/idle sections), member avatars, online counts |
| `chat_panel.slint` | Right sidebar: message list, chat input |
| `control_bar.slint` | Bottom bar: user info, mic/deafen toggles, settings button, chat input |
| `stream_view.slint` | Center: video frame rendering, stream host info, viewer controls |
| `voice_channel_view.slint` | Center (when not streaming): voice channel list with member avatars |
| `settings_modal.slint` | Overlay: audio devices, general preferences, profile editing |
| `new_crew_modal.slint` | Overlay: crew creation (name, description, avatar upload, invite members, visibility) |
| `onboarding.slint` | Full-screen: 3-step new user flow (discover, profile, identity) |
| `discover_panel.slint` | Crew discovery with bento grid, infinite scroll, join/invite-code entry |
| `sign_in.slint` | Social login buttons, email form |
| `debug_panel.slint` | Developer diagnostics (toggled via `Command::SetDebugMode`) |
| `update_banner.slint` | Auto-update notification bar |

### Root wiring

`client/ui/main.slint` is the root component. It declares all top-level properties, conditional panel visibility (`if logged-in`, `if onboarding-step < 4`, `if show-discover`), and wires callbacks from child components to root-level callbacks that `main.rs` binds to.

---

## 5. Theme System

`client/ui/theme.slint` defines a `Theme` global with design tokens:

- **Colors:** `bg-app`, `surface`, `surface-hover`, `text-primary`, `text-secondary`, `text-tertiary`, `accent`, `graphic-light`, `graphic-med`, etc. Supports dark/light via `Theme.dark` boolean.
- **Fonts:** Two families — `font-mono` (monospace, used for labels and code-style text) and `font-sans` (sans-serif, used for body text).
- **Radii:** `r-outer` (panel corners), `r-inner` (input fields, buttons).

SVG icons from mockups are extracted into `client/ui/icons/` as `.svg` files and referenced via `Image { source: @image-url("../icons/foo.svg"); colorize: Theme.accent; }`.

---

## 6. Onboarding Flow

Onboarding is a 3-step full-screen flow for new users (when `onboarding_step < 4`):

| Step | Screen | What happens |
|------|--------|-------------|
| 1 | Discover Crews | Bento grid of public crews (fetched unauthenticated via `http_key`). "Create Your Own Crew" opens the new-crew modal in onboarding mode (invite section disabled, button says "Save & Continue"). Crew details stored locally, creation deferred. |
| 2 | Profile Setup | User sets nickname and picks an avatar. |
| 3 | Identity Linking | Optional social login or email. "Continue" sends `FinalizeOnboarding` which: device-auths → creates account → creates/joins crew (with stored details + avatar) → enters main app. |

The `pending_crew_name`, `pending_crew_description`, and `pending_crew_open` fields are persisted in `Settings` (survives restart). The crew avatar base64 is held in memory only (`Arc<Mutex<Option<String>>>`).

---

## 7. Window Behavior

| Behavior | Implementation |
|----------|----------------|
| Minimum size | 1024 x 768 |
| Default size | 1280 x 800 |
| Resizable | Yes |
| System tray | Yes (minimize to tray) |
| Close button | Minimize to tray (configurable) |
| Start on boot | Optional setting |

---

## 8. Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Ctrl + M` | Toggle mute |
| `Ctrl + D` | Toggle deafen |
| `Ctrl + ,` | Open settings |
| `Escape` | Close modal / Deselect |
| `Enter` | Send message (when input focused) |

---

## 9. Performance Targets

| Metric | Target |
|--------|--------|
| Startup time | <3 seconds |
| Frame render | <16ms (60fps) |
| Input latency | <5ms |
| Memory (idle) | <50MB |
| Memory (streaming) | <100MB |
| Binary size | <10MB (client only) |

---

*This spec defines the desktop client. For core logic, see [02-MELLO-CORE.md](./02-MELLO-CORE.md).*
