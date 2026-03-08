# MELLO Client Specification

> **Component:** Desktop Client (Slint UI)  
> **Language:** Rust  
> **Status:** Beta Scope  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

The Mello client is a native desktop application built with Slint UI framework in Rust. It provides the user interface for crew management, voice chat, text chat, and stream viewing.

---

## 2. Technology Choices

| Aspect | Choice | Rationale |
|--------|--------|-----------|
| Language | Rust | Memory safety, small binaries, modern ecosystem |
| UI Framework | Slint | Declarative, native performance, <2MB, Apache 2.0 |
| Async Runtime | Tokio | Industry standard for Rust async |
| State Management | Slint's reactive properties | Built-in, no extra deps |

---

## 3. Project Structure

```
client/
├── Cargo.toml
├── build.rs                    # Slint compilation
├── src/
│   ├── main.rs                 # Entry point, window setup
│   ├── app.rs                  # App state, Slint bindings
│   ├── config.rs               # User settings persistence
│   └── platform/
│       └── windows.rs          # Windows-specific (tray icon, etc.)
│
├── ui/
│   ├── main.slint              # Root component, layout
│   ├── theme.slint             # Colors, fonts, spacing
│   ├── components/
│   │   ├── avatar.slint        # User avatar with status
│   │   ├── crew_card.slint     # Crew member card
│   │   ├── message.slint       # Chat message
│   │   ├── icon_button.slint   # Mic, headphone buttons
│   │   └── text_input.slint    # Chat input field
│   ├── panels/
│   │   ├── crew_panel.slint    # Left sidebar
│   │   ├── stream_view.slint   # Center video area
│   │   ├── chat_panel.slint    # Right sidebar
│   │   └── control_bar.slint   # Bottom controls
│   └── screens/
│       ├── login.slint         # Login/signup
│       ├── main.slint          # Main app (after login)
│       └── settings.slint      # Settings modal
│
└── assets/
    ├── fonts/
    │   └── inter/              # Inter font family
    └── icons/
        └── *.svg               # UI icons
```

---

## 4. UI Layout

Based on the mockup, the main screen has this structure:

```
┌─────────────────────────────────────────────────────────────────────────┐
│                            MAIN WINDOW                                  │
│  ┌─────────────┐ ┌───────────────────────────────┐ ┌─────────────────┐  │
│  │             │ │                               │ │                 │  │
│  │   CREW      │ │                               │ │    CHAT         │  │
│  │   PANEL     │ │        STREAM VIEW            │ │    PANEL        │  │
│  │             │ │                               │ │                 │  │
│  │  - My crews │ │   - Video frame               │ │  - Messages     │  │
│  │  - Members  │ │   - Host info                 │ │  - System msgs  │  │
│  │  - Status   │ │   - Recording indicator       │ │                 │  │
│  │             │ │   - Progress bar              │ │                 │  │
│  │             │ │                               │ │                 │  │
│  │             │ ├───────────────────────────────┤ │                 │  │
│  │             │ │       CREW MEMBERS BAR        │ │                 │  │
│  │             │ │   [KT] [AS] [MR] [+Invite]    │ │                 │  │
│  │             │ └───────────────────────────────┘ │                 │  │
│  └─────────────┘                                   └─────────────────┘  │
│  ┌──────────────────────────────────────────────────────────────────┐   │
│  │                        CONTROL BAR                               │   │
│  │  [ME] Navigator #001    [🎤] [🎧] [⚙️] [🔴]     [Message...]     │   │
│  └──────────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 5. Component Specifications

### 5.1 Theme (`ui/theme.slint`)

```slint
// Color palette
export global Theme {
    // Backgrounds
    out property <color> bg-primary: #FFFFFF;
    out property <color> bg-secondary: #F5F5F5;
    out property <color> bg-tertiary: #E8E8E8;
    out property <color> bg-dark: #1A1A1A;
    
    // Text
    out property <color> text-primary: #1A1A1A;
    out property <color> text-secondary: #666666;
    out property <color> text-muted: #999999;
    
    // Accent
    out property <color> accent-red: #FF4444;
    out property <color> accent-green: #44CC44;
    
    // Border
    out property <color> border-light: #E0E0E0;
    out property <color> border-dark: #CCCCCC;
    
    // Spacing
    out property <length> spacing-xs: 4px;
    out property <length> spacing-sm: 8px;
    out property <length> spacing-md: 16px;
    out property <length> spacing-lg: 24px;
    out property <length> spacing-xl: 32px;
    
    // Border radius
    out property <length> radius-sm: 4px;
    out property <length> radius-md: 8px;
    out property <length> radius-lg: 16px;
    out property <length> radius-full: 9999px;
    
    // Font sizes
    out property <length> font-xs: 10px;
    out property <length> font-sm: 12px;
    out property <length> font-md: 14px;
    out property <length> font-lg: 16px;
    out property <length> font-xl: 24px;
}
```

### 5.2 Avatar Component (`ui/components/avatar.slint`)

```slint
import { Theme } from "../theme.slint";

export component Avatar inherits Rectangle {
    in property <string> initials;
    in property <bool> speaking: false;
    in property <bool> online: true;
    in property <length> size: 48px;
    
    width: size;
    height: size;
    border-radius: size / 2;
    border-width: speaking ? 2px : 1px;
    border-color: speaking ? Theme.accent-red : Theme.border-light;
    background: Theme.bg-secondary;
    
    Text {
        text: initials;
        font-size: size * 0.35;
        color: Theme.text-primary;
        horizontal-alignment: center;
        vertical-alignment: center;
    }
    
    // Online indicator
    if online: Rectangle {
        x: parent.width - 12px;
        y: parent.height - 12px;
        width: 10px;
        height: 10px;
        border-radius: 5px;
        background: Theme.accent-green;
        border-width: 2px;
        border-color: Theme.bg-primary;
    }
}
```

### 5.3 Crew Panel (`ui/panels/crew_panel.slint`)

```slint
import { Theme } from "../theme.slint";

export struct CrewInfo {
    id: string,
    name: string,
    member_count: int,
    max_members: int,
    is_active: bool,
}

export component CrewPanel inherits Rectangle {
    in property <[CrewInfo]> crews;
    in property <string> selected-crew-id;
    in property <int> total-active-count;
    
    callback crew-selected(string);
    
    width: 240px;
    background: Theme.bg-primary;
    border-width: 1px;
    border-color: Theme.border-light;
    border-radius: Theme.radius-md;
    
    VerticalLayout {
        padding: Theme.spacing-md;
        spacing: Theme.spacing-md;
        
        // Header
        HorizontalLayout {
            Text {
                text: "YOUR CREWS";
                font-size: Theme.font-xs;
                color: Theme.text-muted;
                letter-spacing: 1px;
            }
            Rectangle { horizontal-stretch: 1; }
            Rectangle {
                width: 8px;
                height: 8px;
                border-radius: 4px;
                background: Theme.accent-red;
            }
        }
        
        // Global count
        VerticalLayout {
            spacing: Theme.spacing-xs;
            Text {
                text: total-active-count;
                font-size: Theme.font-xl;
                font-weight: 700;
            }
            Text {
                text: "GLOBAL NETWORK";
                font-size: Theme.font-xs;
                color: Theme.text-muted;
                letter-spacing: 1px;
            }
        }
        
        // Active crews section
        // ... (crew list implementation)
    }
}
```

### 5.4 Stream View (`ui/panels/stream_view.slint`)

```slint
import { Theme } from "../theme.slint";

export component StreamView inherits Rectangle {
    in property <image> video-frame;
    in property <string> host-name;
    in property <string> stream-title;
    in property <duration> elapsed-time;
    in property <bool> is-live: false;
    in property <float> progress: 0.0;  // 0.0 to 1.0
    
    background: Theme.bg-secondary;
    border-radius: Theme.radius-md;
    
    VerticalLayout {
        padding: Theme.spacing-md;
        spacing: Theme.spacing-md;
        
        // Header
        HorizontalLayout {
            spacing: Theme.spacing-sm;
            
            VerticalLayout {
                alignment: start;
                Text {
                    text: "CREWMATE STREAM";
                    font-size: Theme.font-xs;
                    color: Theme.text-muted;
                    letter-spacing: 1px;
                }
                Text {
                    text: host-name;
                    font-size: Theme.font-lg;
                    font-weight: 600;
                }
            }
            
            Rectangle { horizontal-stretch: 1; }
            
            // Live indicator
            if is-live: Rectangle {
                width: 80px;
                height: 24px;
                border-radius: 12px;
                background: Theme.bg-dark;
                
                HorizontalLayout {
                    padding-left: 8px;
                    padding-right: 8px;
                    spacing: 6px;
                    alignment: center;
                    
                    Rectangle {
                        width: 8px;
                        height: 8px;
                        border-radius: 4px;
                        background: Theme.accent-red;
                    }
                    Text {
                        text: format-duration(elapsed-time);
                        color: white;
                        font-size: Theme.font-sm;
                    }
                }
            }
        }
        
        // Video area
        Rectangle {
            vertical-stretch: 1;
            background: Theme.bg-tertiary;
            border-radius: Theme.radius-sm;
            
            Image {
                source: video-frame;
                width: parent.width;
                height: parent.height;
                image-fit: contain;
            }
        }
        
        // Progress bar area
        HorizontalLayout {
            spacing: Theme.spacing-md;
            alignment: center-vertical;
            
            Text {
                text: stream-title;
                font-size: Theme.font-sm;
                color: Theme.text-secondary;
            }
            
            Rectangle { horizontal-stretch: 1; }
            
            // Progress bar
            Rectangle {
                width: 200px;
                height: 4px;
                background: Theme.bg-tertiary;
                border-radius: 2px;
                
                Rectangle {
                    width: parent.width * progress;
                    height: parent.height;
                    background: Theme.text-primary;
                    border-radius: 2px;
                }
                
                // Knob
                Rectangle {
                    x: parent.width * progress - 6px;
                    y: -4px;
                    width: 12px;
                    height: 12px;
                    border-radius: 6px;
                    background: Theme.text-primary;
                }
            }
        }
    }
}
```

### 5.5 Control Bar (`ui/panels/control_bar.slint`)

```slint
import { Theme } from "../theme.slint";
import { Avatar } from "../components/avatar.slint";
import { IconButton } from "../components/icon_button.slint";

export component ControlBar inherits Rectangle {
    in property <string> user-initials;
    in property <string> user-name;
    in property <string> user-tag;
    in property <bool> mic-muted: false;
    in property <bool> deafened: false;
    
    callback mic-toggled();
    callback deafen-toggled();
    callback settings-clicked();
    callback leave-clicked();
    callback message-submitted(string);
    
    height: 72px;
    background: Theme.bg-primary;
    border-width: 1px;
    border-color: Theme.border-light;
    border-radius: Theme.radius-md;
    
    HorizontalLayout {
        padding: Theme.spacing-md;
        spacing: Theme.spacing-lg;
        
        // User info
        HorizontalLayout {
            spacing: Theme.spacing-sm;
            alignment: center-vertical;
            
            Avatar {
                initials: user-initials;
                size: 40px;
            }
            
            VerticalLayout {
                alignment: center;
                Text {
                    text: user-name;
                    font-size: Theme.font-md;
                    font-weight: 600;
                }
                Text {
                    text: user-tag;
                    font-size: Theme.font-xs;
                    color: Theme.text-muted;
                }
            }
        }
        
        // Control buttons
        HorizontalLayout {
            spacing: Theme.spacing-sm;
            alignment: center-vertical;
            
            IconButton {
                icon: mic-muted ? "mic-off" : "mic";
                active: !mic-muted;
                clicked => { mic-toggled(); }
            }
            
            IconButton {
                icon: deafened ? "headphones-off" : "headphones";
                active: !deafened;
                clicked => { deafen-toggled(); }
            }
            
            IconButton {
                icon: "settings";
                clicked => { settings-clicked(); }
            }
            
            IconButton {
                icon: "power";
                danger: true;
                clicked => { leave-clicked(); }
            }
        }
        
        Rectangle { horizontal-stretch: 1; }
        
        // Message input
        Rectangle {
            width: 300px;
            height: 40px;
            background: Theme.bg-secondary;
            border-radius: Theme.radius-md;
            
            TextInput {
                x: Theme.spacing-md;
                width: parent.width - Theme.spacing-md * 2;
                height: parent.height;
                font-size: Theme.font-md;
                placeholder-text: "Message crewmates...";
                
                accepted => {
                    message-submitted(self.text);
                    self.text = "";
                }
            }
        }
    }
}
```

---

## 6. App State (Rust)

### 6.1 State Structure

```rust
// src/app.rs

use slint::{ComponentHandle, Weak};
use mello_core::{Crew, Member, Message, StreamInfo};

pub struct AppState {
    // UI handle
    window: Weak<MainWindow>,
    
    // Core connection
    core: mello_core::Client,
    
    // Current state
    current_user: Option<User>,
    current_crew: Option<CrewId>,
    crews: Vec<Crew>,
    
    // Voice state
    mic_muted: bool,
    deafened: bool,
    speaking_members: HashSet<MemberId>,
    
    // Stream state
    active_stream: Option<StreamInfo>,
    watching_stream: Option<StreamView>,
}

pub struct User {
    pub id: String,
    pub name: String,
    pub tag: String,
    pub avatar_initials: String,
}
```

### 6.2 Slint ↔ Rust Bindings

```rust
// src/app.rs

impl AppState {
    pub fn new(window: MainWindow) -> Self {
        let state = Self {
            window: window.as_weak(),
            // ... init
        };
        
        // Bind callbacks
        let state_ref = Rc::new(RefCell::new(state));
        
        {
            let state = state_ref.clone();
            window.on_crew_selected(move |crew_id| {
                state.borrow_mut().select_crew(&crew_id);
            });
        }
        
        {
            let state = state_ref.clone();
            window.on_mic_toggled(move || {
                state.borrow_mut().toggle_mic();
            });
        }
        
        // ... more bindings
        
        state_ref.take()
    }
    
    pub fn select_crew(&mut self, crew_id: &str) {
        // Call mello-core
        self.core.join_crew(crew_id);
        
        // Update UI
        if let Some(window) = self.window.upgrade() {
            window.set_selected_crew_id(crew_id.into());
        }
    }
    
    pub fn toggle_mic(&mut self) {
        self.mic_muted = !self.mic_muted;
        self.core.voice_set_mute(self.mic_muted);
        
        if let Some(window) = self.window.upgrade() {
            window.set_mic_muted(self.mic_muted);
        }
    }
}
```

### 6.3 Video Frame Rendering

```rust
// Receiving frames from mello-core and pushing to Slint

use slint::{SharedPixelBuffer, Rgba8Pixel, Image};

impl AppState {
    pub fn handle_video_frame(&mut self, frame: &mello_core::VideoFrame) {
        // frame.data is RGB or RGBA bytes
        let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
            frame.data,
            frame.width,
            frame.height,
        );
        
        let image = Image::from_rgba8(buffer);
        
        if let Some(window) = self.window.upgrade() {
            window.set_video_frame(image);
        }
    }
}
```

---

## 7. Main Entry Point

```rust
// src/main.rs

use slint::ComponentHandle;

slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    env_logger::init();
    
    // Create window
    let window = MainWindow::new()?;
    
    // Initialize mello-core
    let core = mello_core::Client::new()?;
    
    // Create app state
    let app = AppState::new(window.clone_strong(), core);
    
    // Run event loop
    window.run()?;
    
    Ok(())
}
```

---

## 8. Dependencies (Cargo.toml)

```toml
[package]
name = "mello-client"
version = "0.1.0"
edition = "2021"

[dependencies]
# UI
slint = "1.5"

# Async runtime
tokio = { version = "1", features = ["full"] }

# Core library
mello-core = { path = "../mello-core" }

# Logging
log = "0.4"
env_logger = "0.11"

# Config persistence
serde = { version = "1", features = ["derive"] }
serde_json = "1"
directories = "5"  # For config file paths

# Windows-specific
[target.'cfg(windows)'.dependencies]
windows = { version = "0.54", features = ["Win32_UI_Shell"] }

[build-dependencies]
slint-build = "1.5"
```

---

## 9. Build Configuration

```rust
// build.rs

fn main() {
    slint_build::compile_with_config(
        "ui/main.slint",
        slint_build::CompilerConfiguration::new()
            .with_style("fluent-dark".into()),
    ).unwrap();
}
```

---

## 10. Window Behavior

| Behavior | Implementation |
|----------|----------------|
| Minimum size | 1024 x 768 |
| Default size | 1280 x 800 |
| Resizable | Yes |
| System tray | Yes (minimize to tray) |
| Close button | Minimize to tray (configurable) |
| Start on boot | Optional setting |

---

## 11. Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Ctrl + M` | Toggle mute |
| `Ctrl + D` | Toggle deafen |
| `Ctrl + ,` | Open settings |
| `Escape` | Close modal / Deselect |
| `Enter` | Send message (when input focused) |

---

## 12. Performance Targets

| Metric | Target |
|--------|--------|
| Startup time | <3 seconds |
| Frame render | <16ms (60fps) |
| Input latency | <5ms |
| Memory (idle) | <50MB |
| Memory (streaming) | <100MB |
| Binary size | <10MB (client only) |

---

## 13. Testing Strategy

| Type | Tool | Coverage |
|------|------|----------|
| Unit tests | `cargo test` | State logic |
| UI tests | Slint testing utilities | Component rendering |
| Integration | Manual + automation | E2E flows |

---

## 14. Future Considerations

- **Themes:** Dark mode, custom themes
- **Animations:** Smooth transitions, micro-interactions
- **Accessibility:** Screen reader support, keyboard navigation
- **Localization:** i18n support
- **TODO:** Add optimized `[profile.release]` settings (`lto = true`, `strip = true`, `codegen-units = 1`, `opt-level = "s"`) to workspace Cargo.toml for smallest binary

---

*This spec defines the desktop client. For core logic, see [02-MELLO-CORE.md](./02-MELLO-CORE.md).*
