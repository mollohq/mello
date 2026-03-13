# MELLO Native Platform Integration Specification

> **Component:** Native OS Integration (Client)
> **Version:** 0.2
> **Status:** Planned
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [01-CLIENT.md](./01-CLIENT.md)

---

## 1. Overview

This spec covers everything that makes Mello a well-behaved native citizen on macOS and Windows. It does not replace the main client spec — it extends it with OS-level concerns that live outside Slint's rendering loop.

**Goals:**
- Correct app bundle on both platforms (real icon, no terminal fallback)
- Platform-native window chrome and menus
- Tray / menu bar status item with speaking state indicator
- Global push-to-talk hotkey that fires when Mello is not focused
- Single instance enforcement
- OS notifications for background activity
- Deep link / URL scheme handling for crew invites
- Start-on-boot (optional user setting)

**Out of scope for this version:**
- Mini HUD / popover from tray
- macOS dock badge and bounce
- Window vibrancy / blur (NSVisualEffectView)
- Speaking indicator pulse animation (static frame swap only for now)

---

## 2. App Bundle & Icon

### 2.1 Bundle identifiers

| Platform | Identifier |
|---|---|
| macOS | `app.mello.desktop` |
| Windows | `app.mello.desktop` (used as AppUserModelId) |

### 2.2 cargo-bundle (macOS .app)

Add to `client/Cargo.toml`:

```toml
[package.metadata.bundle]
name = "Mello"
identifier = "app.mello.desktop"
icon = ["assets/icons/app_icon.icns"]
short_description = "Hang out with your crew."
osx_url_schemes = ["mello"]
```

Run with:

```bash
cargo install cargo-bundle
cargo bundle --release
# → target/release/bundle/osx/Mello.app
```

### 2.3 Icon assets required

| File | Sizes | Used for |
|---|---|---|
| `assets/icons/app_icon.icns` | 16, 32, 64, 128, 256, 512, 1024px | macOS dock, Finder, App Switcher |
| `assets/icons/app_icon.ico` | 16, 32, 48, 64, 256px | Windows taskbar, Explorer |
| `assets/icons/menubar.svg` | 22×22 | Status item source (rasterised at runtime) |

The `.icns` and `.ico` are generated from the master artwork (`assets/icons/app_icon_master.png`, 1024×1024). The `menubar.svg` is a white circle on transparent background — a placeholder until the M mark is available as a clean vector.

### 2.4 Generating .icns from PNG

```bash
# macOS (requires iconutil)
mkdir Mello.iconset
sips -z 16 16     app_icon_master.png --out Mello.iconset/icon_16x16.png
sips -z 32 32     app_icon_master.png --out Mello.iconset/icon_16x16@2x.png
sips -z 32 32     app_icon_master.png --out Mello.iconset/icon_32x32.png
sips -z 64 64     app_icon_master.png --out Mello.iconset/icon_32x32@2x.png
sips -z 128 128   app_icon_master.png --out Mello.iconset/icon_128x128.png
sips -z 256 256   app_icon_master.png --out Mello.iconset/icon_128x128@2x.png
sips -z 256 256   app_icon_master.png --out Mello.iconset/icon_256x256.png
sips -z 512 512   app_icon_master.png --out Mello.iconset/icon_256x256@2x.png
sips -z 512 512   app_icon_master.png --out Mello.iconset/icon_512x512.png
sips -z 1024 1024 app_icon_master.png --out Mello.iconset/icon_512x512@2x.png
iconutil -c icns Mello.iconset
```

---

## 3. Window Chrome (Slint style)

Slint's built-in styles are selected at compile time in `build.rs`. The correct style per platform gives native window chrome (title bar, traffic lights on macOS, window controls on Windows) for free.

```rust
// client/build.rs

fn main() {
    let style = if cfg!(target_os = "macos") {
        "cupertino"
    } else if cfg!(target_os = "windows") {
        "fluent"
    } else {
        "fluent"  // Linux fallback
    };

    slint_build::compile_with_config(
        "ui/main.slint",
        slint_build::CompilerConfiguration::new()
            .with_style(style.into()),
    )
    .unwrap();
}
```

---

## 4. Native Menus (`muda`)

**macOS only.** Windows does not get a menu bar — keyboard shortcuts and the tray context menu are the only affordances on Windows.

### 4.1 Dependency

```toml
# client/Cargo.toml
[dependencies]
muda = "0.15"
```

### 4.2 macOS menu bar structure

```
Mello     Edit     View     Help
```

```
Mello
│
├── About Mello
├── ─────────────
├── Check for Updates…
├── ─────────────
├── Preferences…    Cmd+,
├── ─────────────
├── Services        ►
├── ─────────────
├── Hide Mello      Cmd+H
├── Hide Others     Cmd+Opt+H
├── Show All
├── ─────────────
└── Quit Mello      Cmd+Q
```

```
Edit
│
├── Undo            Cmd+Z
├── Redo            Cmd+Shift+Z
├── ─────────────
├── Cut             Cmd+X
├── Copy            Cmd+C
├── Paste           Cmd+V
├── Select All      Cmd+A
├── ─────────────
└── Find…           Cmd+F
```

```
View
│
├── Toggle Mute     Cmd+Ctrl+M
└── Toggle Deafen   Cmd+Ctrl+D
```

```
Help
│
└── Mello on GitHub
```

### 4.3 Notes on the Edit menu

`muda` exposes `PredefinedMenuItem` variants for all standard Edit actions — these wire into the macOS responder chain properly, meaning Slint `TextInput` fields get undo/redo/copy/paste for free without any custom handling. **Always use `PredefinedMenuItem` for these** — custom `MenuItem` with matching accelerators will not correctly target the first responder.

File and Window menus are intentionally omitted. Mello is a single-window app; Close Window (Cmd+W) is handled by the close→tray behaviour, not a menu item.

### 4.4 Implementation

```rust
// src/platform/macos.rs

use muda::{Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};

/// Build the macOS NSMenuBar. Must be called on the main thread before
/// the Slint event loop starts.
#[cfg(target_os = "macos")]
pub fn build_menu_bar() -> Menu {
    let menu = Menu::new();

    // ── Mello ──────────────────────────────────────────────────────────────
    let app_menu = Submenu::with_title("Mello", true);
    app_menu.append(&PredefinedMenuItem::about(Some("About Mello"), None)).ok();
    app_menu.append(&PredefinedMenuItem::separator()).ok();
    app_menu.append(&MenuItem::with_id(
        MenuId::new("prefs"),
        "Preferences…",
        true,
        Some("cmd+,".parse().unwrap()),
    )).ok();
    app_menu.append(&PredefinedMenuItem::separator()).ok();
    app_menu.append(&PredefinedMenuItem::quit(Some("Quit Mello"))).ok();
    menu.append(&app_menu).ok();

    // ── Edit ───────────────────────────────────────────────────────────────
    // All PredefinedMenuItems — these integrate with the macOS responder chain
    // and give Slint TextInput fields correct system behaviour for free.
    let edit_menu = Submenu::with_title("Edit", true);
    edit_menu.append(&PredefinedMenuItem::undo(None)).ok();
    edit_menu.append(&PredefinedMenuItem::redo(None)).ok();
    edit_menu.append(&PredefinedMenuItem::separator()).ok();
    edit_menu.append(&PredefinedMenuItem::cut(None)).ok();
    edit_menu.append(&PredefinedMenuItem::copy(None)).ok();
    edit_menu.append(&PredefinedMenuItem::paste(None)).ok();
    edit_menu.append(&PredefinedMenuItem::select_all(None)).ok();
    edit_menu.append(&PredefinedMenuItem::separator()).ok();
    edit_menu.append(&MenuItem::with_id(
        MenuId::new("find"),
        "Find…",
        true,
        Some("cmd+f".parse().unwrap()),
    )).ok();
    menu.append(&edit_menu).ok();

    // ── View ───────────────────────────────────────────────────────────────
    let view_menu = Submenu::with_title("View", true);
    view_menu.append(&MenuItem::with_id(
        MenuId::new("mute"),
        "Toggle Mute",
        true,
        Some("cmd+ctrl+m".parse().unwrap()),
    )).ok();
    view_menu.append(&MenuItem::with_id(
        MenuId::new("deafen"),
        "Toggle Deafen",
        true,
        Some("cmd+ctrl+d".parse().unwrap()),
    )).ok();
    menu.append(&view_menu).ok();

    // ── Help ───────────────────────────────────────────────────────────────
    let help_menu = Submenu::with_title("Help", true);
    help_menu.append(&MenuItem::with_id(
        MenuId::new("github"),
        "Mello on GitHub",
        true,
        None,
    )).ok();
    menu.append(&help_menu).ok();

    menu
}
```

The `find` and `github` menu events are handled in the main event polling loop alongside tray and hotkey events. The View menu shortcuts are changed to `Cmd+Ctrl+M/D` to avoid colliding with system-level `Ctrl+M/D` shortcuts used by many games on Windows (these shortcuts are macOS-only so the modifier is fine).

---

## 5. Tray / Menu Bar Status Item (`tray-icon`)

### 5.1 Dependency

```toml
[dependencies]
tray-icon = "0.19"
```

### 5.2 Icon states

The status item icon communicates voice state at a glance. All icons are 22×22 RGBA pixel buffers generated at runtime from the source SVG circle shape.

| State | Fill colour | Alpha | Template? |
|---|---|---|---|
| Not in voice | White `#FFFFFF` | 60% | Yes — macOS handles dark/light |
| In voice, silent | White `#FFFFFF` | 100% | Yes |
| Speaking | Green `#44CC44` | 100% | No — full-colour RGBA |
| Muted | Red `#FF4444` | 100% | No — full-colour RGBA |

Template images (`is_template: true`) are pure white/black PNGs. macOS automatically inverts them for dark vs light menu bars. When a colour state is active (speaking/muted), template mode is disabled and the icon is supplied as a full-colour RGBA buffer.

### 5.3 Context menu

Right-click on the status item shows:

```
Open Mello                ← or "Hide Mello" when window is focused
─────────────────────
🎤 Mute                   ← toggle; checkmark when muted; greyed if not in voice
Leave Voice               ← greyed if not in voice
─────────────────────
Quit Mello
```

### 5.4 Click behaviour

Single click (left or primary): toggle window visibility (show if hidden, hide if visible).

### 5.5 Implementation

```rust
// src/platform/mod.rs

use tray_icon::{
    TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon,
};

pub struct StatusItem {
    tray: TrayIcon,
    menu_mute: MenuItem,
    menu_leave: MenuItem,
    current_state: VoiceState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VoiceState {
    Inactive,
    Connected,
    Speaking,
    Muted,
}

impl StatusItem {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let menu_open  = MenuItem::new("Open Mello", true, None);
        let menu_mute  = MenuItem::new("Mute", false, None);   // greyed initially
        let menu_leave = MenuItem::new("Leave Voice", false, None);

        let tray_menu = Menu::new();
        tray_menu.append(&menu_open)?;
        tray_menu.append(&PredefinedMenuItem::separator())?;
        tray_menu.append(&menu_mute)?;
        tray_menu.append(&menu_leave)?;
        tray_menu.append(&PredefinedMenuItem::separator())?;
        tray_menu.append(&PredefinedMenuItem::quit(Some("Quit Mello")))?;

        let icon = Self::render_icon(VoiceState::Inactive);

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(tray_menu))
            .with_icon(icon)
            .with_tooltip("Mello")
            .build()?;

        Ok(Self {
            tray,
            menu_mute,
            menu_leave,
            current_state: VoiceState::Inactive,
        })
    }

    pub fn set_voice_state(&mut self, state: VoiceState) {
        if state == self.current_state { return; }
        self.current_state = state;

        let in_voice = state != VoiceState::Inactive;
        self.menu_mute.set_enabled(in_voice);
        self.menu_leave.set_enabled(in_voice);

        let is_muted = state == VoiceState::Muted;
        self.menu_mute.set_text(if is_muted { "✓ Mute" } else { "Mute" });

        self.tray.set_icon(Some(Self::render_icon(state))).ok();
    }

    fn render_icon(state: VoiceState) -> Icon {
        let (r, g, b, a): (u8, u8, u8, u8) = match state {
            VoiceState::Inactive  => (255, 255, 255, 153),  // white 60%
            VoiceState::Connected => (255, 255, 255, 255),  // white 100%
            VoiceState::Speaking  => ( 68, 204,  68, 255),  // green
            VoiceState::Muted     => (255,  68,  68, 255),  // red
        };

        // Rasterise a filled circle into a 22×22 RGBA buffer
        let size = 22usize;
        let cx = (size / 2) as f32;
        let cy = (size / 2) as f32;
        let radius = 7.0f32;
        let mut rgba = vec![0u8; size * size * 4];

        for py in 0..size {
            for px in 0..size {
                let dx = px as f32 - cx;
                let dy = py as f32 - cy;
                if dx * dx + dy * dy <= radius * radius {
                    let i = (py * size + px) * 4;
                    rgba[i]     = r;
                    rgba[i + 1] = g;
                    rgba[i + 2] = b;
                    rgba[i + 3] = a;
                }
            }
        }

        Icon::from_rgba(rgba, size as u32, size as u32).expect("icon render failed")
    }
}
```

---

## 6. Global Hotkeys (`global-hotkey`)

Push-to-talk must fire even when Mello is not the focused window — i.e. when the user is in a game fullscreen.

### 6.1 Dependency

```toml
[dependencies]
global-hotkey = "0.6"
```

### 6.2 Default hotkeys

| Action | Default | Notes |
|---|---|---|
| Push-to-talk (hold) | None | User must assign in settings — no default to avoid conflicts |
| Toggle mute | None | Optional assignment |

PTT has no default because any key could conflict with a game. The user configures it in Settings. The raw `HotKeyCode` is stored in config and re-registered on startup.

### 6.3 PTT behaviour

- **Key down** → unmute microphone (if currently muted)
- **Key up** → re-mute microphone

PTT is only active when the user is connected to a voice channel. The hotkey manager is registered at app start but its action is gated by voice connection state.

### 6.4 Implementation

```rust
// src/platform/hotkeys.rs

use global_hotkey::{GlobalHotKeyManager, GlobalHotKeyEvent, hotkey::{HotKey, Modifiers, Code}};

pub struct HotkeyManager {
    manager: GlobalHotKeyManager,
    ptt_id: Option<u32>,
}

impl HotkeyManager {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            manager: GlobalHotKeyManager::new()?,
            ptt_id: None,
        })
    }

    pub fn register_ptt(&mut self, hotkey: HotKey) -> Result<(), Box<dyn std::error::Error>> {
        // Unregister previous if set
        if let Some(id) = self.ptt_id.take() {
            self.manager.unregister_by_id(id).ok();
        }
        let id = hotkey.id();
        self.manager.register(hotkey)?;
        self.ptt_id = Some(id);
        Ok(())
    }

    /// Poll for hotkey events — call from the Tokio event loop
    pub fn poll(&self) -> Option<GlobalHotKeyEvent> {
        GlobalHotKeyEvent::receiver().try_recv().ok()
    }
}
```

In `app.rs`, poll on each tick and check `event.id == ptt_id` plus `event.state` (pressed/released) to drive `core.voice_set_mute(...)`.

---

## 7. Single Instance Enforcement

A second launch of Mello should focus the existing window and immediately exit.

### 7.1 Dependency

```toml
[dependencies]
single-instance = "0.3"
```

### 7.2 Implementation

```rust
// src/main.rs

use single_instance::SingleInstance;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let instance = SingleInstance::new("app.mello.desktop")?;

    if !instance.is_single() {
        // Another instance is running — signal it to raise its window, then exit.
        // For now just exit cleanly; IPC focus signal is a future improvement.
        eprintln!("Mello is already running.");
        std::process::exit(0);
    }

    // ... rest of startup
}
```

The `SingleInstance` handle must be kept alive for the duration of the process (do not drop it). On Windows this is a named mutex; on macOS/Linux it is a lock file under the platform temp directory.

---

## 8. OS Notifications (`notify-rust`)

Notifications are used to surface activity while the window is hidden to the tray.

### 8.1 Dependency

```toml
[dependencies]
notify-rust = "4"
```

### 8.2 Notification triggers

| Event | Title | Body | Conditions |
|---|---|---|---|
| Crew member joins voice | "Crew" | "{name} jumped in" | Window hidden |
| New chat message | "{crew name}" | "{name}: {preview}" | Window hidden |
| Crew invite received | "Mello invite" | "{name} invited you to {crew}" | Always |

Notifications are suppressed when the window is visible and focused. Do not notify for own actions.

### 8.3 Implementation

```rust
// src/notifications.rs

use notify_rust::Notification;

pub fn notify_member_joined(name: &str) {
    Notification::new()
        .summary("Crew")
        .body(&format!("{} jumped in", name))
        .icon("app.mello.desktop")
        .timeout(notify_rust::Timeout::Milliseconds(4000))
        .show()
        .ok();
}

pub fn notify_message(crew: &str, sender: &str, preview: &str) {
    Notification::new()
        .summary(crew)
        .body(&format!("{}: {}", sender, preview))
        .icon("app.mello.desktop")
        .timeout(notify_rust::Timeout::Milliseconds(4000))
        .show()
        .ok();
}

pub fn notify_invite(inviter: &str, crew: &str) {
    Notification::new()
        .summary("Mello invite")
        .body(&format!("{} invited you to {}", inviter, crew))
        .icon("app.mello.desktop")
        .timeout(notify_rust::Timeout::Milliseconds(0))  // persistent
        .show()
        .ok();
}
```

---

## 9. Deep Links / URL Scheme (`mello://`)

Crew invites use a URL scheme so links in browsers, Discord messages, etc. can open Mello directly.

### 9.1 URL format

```
mello://invite/{invite_code}
mello://crew/{crew_id}
```

### 9.2 Registration

**macOS** — `cargo-bundle` handles this via `Cargo.toml`:

```toml
[package.metadata.bundle]
osx_url_schemes = ["mello"]
```

**Windows** — registry entry written by the installer:

```
HKEY_CLASSES_ROOT\mello
  (Default) = "URL:Mello Protocol"
  URL Protocol = ""
  \shell\open\command
    (Default) = "\"C:\...\mello.exe\" \"%1\""
```

The installer must write this. For development, a helper script can register it manually.

### 9.3 Handling deep links at runtime

> **Status: Deferred.** Deep links are parsed but not acted upon at runtime. Joining a crew or accepting an invite via `mello://` URLs requires server-side support (invite resolution, crew join RPC) that is not yet implemented. This will be wired up once the backend endpoints are available.

When Mello is already running and a `mello://` link is opened, the OS launches a second process. The second process detects it is not the single instance, passes the URL to the running instance via a named pipe / Unix socket, then exits.

When Mello is not running, it starts normally and processes the URL from `std::env::args()`.

```rust
// src/main.rs

fn extract_deep_link() -> Option<String> {
    std::env::args()
        .nth(1)
        .filter(|arg| arg.starts_with("mello://"))
}

// src/deep_link.rs

pub enum DeepLink {
    Invite { code: String },
    Crew { id: String },
}

pub fn parse(url: &str) -> Option<DeepLink> {
    let url = url.strip_prefix("mello://")?;
    let mut parts = url.splitn(2, '/');
    match parts.next()? {
        "invite" => Some(DeepLink::Invite { code: parts.next()?.to_string() }),
        "crew"   => Some(DeepLink::Crew   { id:   parts.next()?.to_string() }),
        _        => None,
    }
}
```

---

## 10. Start on Boot (`auto-launch`)

An optional user setting that registers Mello to start with the OS.

### 10.1 Dependency

```toml
[dependencies]
auto-launch = "0.5"
```

### 10.2 Implementation

```rust
// src/platform/autolaunch.rs

use auto_launch::AutoLaunch;

pub fn set_start_on_boot(enabled: bool) -> Result<(), Box<dyn std::error::Error>> {
    let app_name = "Mello";
    let app_path = std::env::current_exe()?;

    let auto = AutoLaunch::new(app_name, app_path.to_str().unwrap(), &[] as &[&str]);

    if enabled {
        auto.enable()?;
    } else {
        auto.disable()?;
    }

    Ok(())
}

pub fn is_start_on_boot_enabled() -> bool {
    let app_name = "Mello";
    let app_path = std::env::current_exe().unwrap_or_default();
    AutoLaunch::new(app_name, app_path.to_str().unwrap_or(""), &[] as &[&str])
        .is_enabled()
        .unwrap_or(false)
}
```

This is surfaced as a toggle in Settings. Default: off.

---

## 11. Close → Tray Behaviour

Clicking the window close button hides to tray rather than quitting. This is the expected behaviour for a persistent communication app.

```rust
// src/main.rs / src/app.rs

window.on_close_requested(move || {
    window_ref.hide().ok();
    slint::CloseRequestResponse::KeepWindowHidden
});
```

A "Quit Mello" option in the tray context menu performs a real exit. The close-to-tray behaviour can be made configurable in a later version.

---

## 12. Platform Module Structure

```
client/src/platform/
├── mod.rs            // PlatformIntegration trait + factory fn
│                     // StatusItem, HotkeyManager initialisation
├── windows.rs        // Windows-specific: AppUserModelId, registry (deep links), jump list (future)
└── macos.rs          // macOS-specific: NSApp activation policy, URL event handler (deep links)
```

```rust
// src/platform/mod.rs

pub trait PlatformIntegration {
    fn set_voice_state(&mut self, state: VoiceState);
    fn show_notification(&self, title: &str, body: &str);
    fn register_ptt(&mut self, hotkey: global_hotkey::hotkey::HotKey) -> Result<(), Box<dyn std::error::Error>>;
    fn poll_hotkey(&self) -> Option<global_hotkey::GlobalHotKeyEvent>;
    fn poll_menu_event(&self) -> Option<muda::MenuEvent>;
    fn poll_tray_event(&self) -> Option<tray_icon::TrayIconEvent>;
}
```

---

## 13. Dependencies Summary

```toml
# client/Cargo.toml

[dependencies]
# Native menus
muda = "0.15"

# Tray / menu bar status item
tray-icon = "0.19"

# Global hotkeys (PTT)
global-hotkey = "0.6"

# Single instance
single-instance = "0.3"

# OS notifications
notify-rust = "4"

# Start on boot
auto-launch = "0.5"

[build-dependencies]
# (cargo-bundle is installed as a cargo subcommand, not a build dep)
slint-build = "1.5"
```

---

## 14. Testing Checklist

- [ ] `.app` bundle opens with correct icon on macOS dock
- [ ] `.exe` shows correct icon in Windows taskbar and Explorer
- [ ] Window chrome uses cupertino style on macOS, fluent on Windows
- [ ] macOS app menu present with Preferences (Cmd+,) and Quit (Cmd+Q)
- [ ] Tray icon appears on both platforms at startup
- [ ] Tray icon is white/dimmed when not in voice
- [ ] Tray icon turns green when speaking
- [ ] Tray icon turns red when muted
- [ ] Single click on tray: toggles window visibility
- [ ] Right-click tray: context menu appears
- [ ] Mute and Leave Voice greyed out when not in voice
- [ ] Mute toggle works from context menu
- [ ] PTT hotkey fires when Mello is in background
- [ ] PTT hotkey fires when a game is fullscreen
- [ ] Launching a second instance focuses existing window and exits
- [ ] `mello://invite/{code}` opens Mello or routes to running instance
- [ ] OS notification shown when member joins and window is hidden
- [ ] Notifications suppressed when window is focused
- [ ] Start on boot toggle persists across restarts
- [ ] Close button hides to tray (does not quit)
- [ ] Quit from tray exits cleanly

---

*This spec covers native OS integration. For client UI, see [01-CLIENT.md](./01-CLIENT.md). For auto-updates, see [07-AUTO-UPDATER.md](./07-AUTO-UPDATER.md).*
