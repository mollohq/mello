#[cfg(target_os = "macos")]
pub mod macos;
pub mod hotkeys;

use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VoiceState {
    Inactive,
    Connected,
    Speaking,
    Muted,
}

pub struct StatusItem {
    _tray: TrayIcon,
    current_state: VoiceState,
}

impl StatusItem {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let icon = Self::render_icon(VoiceState::Inactive);

        let tray = TrayIconBuilder::new()
            .with_icon(icon)
            .with_tooltip("Mello")
            .build()?;

        Ok(Self {
            _tray: tray,
            current_state: VoiceState::Inactive,
        })
    }

    pub fn set_voice_state(&mut self, state: VoiceState) {
        if state == self.current_state {
            return;
        }
        self.current_state = state;
        self._tray.set_icon(Some(Self::render_icon(state))).ok();
    }

    /// Poll for tray icon click events.
    pub fn poll_tray_event() -> Option<TrayIconEvent> {
        TrayIconEvent::receiver().try_recv().ok()
    }

    fn render_icon(state: VoiceState) -> Icon {
        let (r, g, b, a): (u8, u8, u8, u8) = match state {
            VoiceState::Inactive => (255, 255, 255, 153),  // white 60%
            VoiceState::Connected => (255, 255, 255, 255), // white 100%
            VoiceState::Speaking => (68, 204, 68, 255),    // green
            VoiceState::Muted => (255, 68, 68, 255),       // red
        };

        // Rasterise a filled circle into a 22x22 RGBA buffer
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
                    rgba[i] = r;
                    rgba[i + 1] = g;
                    rgba[i + 2] = b;
                    rgba[i + 3] = a;
                }
            }
        }

        Icon::from_rgba(rgba, size as u32, size as u32).expect("icon render failed")
    }
}
