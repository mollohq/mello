use crate::protocol::HudMode;

/// Tracks which HUD window is currently visible and performs show/hide
/// transitions. Both windows are created at startup; only one is ever visible.
pub struct ModeManager {
    current: HudMode,
}

impl ModeManager {
    pub fn new() -> Self {
        Self {
            current: HudMode::Hidden,
        }
    }

    pub fn current(&self) -> HudMode {
        self.current
    }

    /// Apply a mode change. Returns true if the mode actually changed.
    pub fn set_mode(&mut self, mode: HudMode) -> bool {
        if mode == self.current {
            return false;
        }
        log::info!("[mode] {:?} → {:?}", self.current, mode);
        self.current = mode;
        true
    }
}
