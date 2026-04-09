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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_hidden() {
        let m = ModeManager::new();
        assert_eq!(m.current(), HudMode::Hidden);
    }

    #[test]
    fn change_returns_true() {
        let mut m = ModeManager::new();
        assert!(m.set_mode(HudMode::MiniPlayer));
        assert_eq!(m.current(), HudMode::MiniPlayer);
    }

    #[test]
    fn same_mode_returns_false() {
        let mut m = ModeManager::new();
        assert!(!m.set_mode(HudMode::Hidden));
    }

    #[test]
    fn full_cycle() {
        let mut m = ModeManager::new();
        assert!(m.set_mode(HudMode::MiniPlayer));
        assert!(m.set_mode(HudMode::Overlay));
        assert!(m.set_mode(HudMode::Hidden));
        assert_eq!(m.current(), HudMode::Hidden);
    }
}
