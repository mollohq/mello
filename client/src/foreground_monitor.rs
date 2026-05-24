use crate::hud_manager::HudMode;

/// Monitors the foreground window to determine whether to show the overlay
/// or hide the HUD entirely.
pub struct ForegroundMonitor {
    current_mode: HudMode,
    in_voice: bool,
    hud_enabled: bool,
    /// Tracked game PID — used by the streaming feature, not by HUD mode logic.
    game_pid: Option<u32>,
}

struct ForegroundInfo {
    is_main_window: bool,
}

impl ForegroundMonitor {
    pub fn new(hud_enabled: bool) -> Self {
        Self {
            current_mode: HudMode::Hidden,
            in_voice: false,
            hud_enabled,
            game_pid: None,
        }
    }

    pub fn set_game_active(&mut self, active: bool, pid: Option<u32>) {
        self.game_pid = if active { pid } else { None };
    }

    pub fn game_pid(&self) -> Option<u32> {
        self.game_pid
    }

    pub fn set_in_voice(&mut self, in_voice: bool) {
        self.in_voice = in_voice;
    }

    pub fn set_hud_enabled(&mut self, enabled: bool) {
        self.hud_enabled = enabled;
    }

    /// Evaluate and return the current HUD mode based on all signals.
    /// Returns Some(mode) if the mode changed, None if unchanged.
    pub fn evaluate(&mut self, main_window_visible: bool) -> Option<HudMode> {
        let fg = query_foreground(main_window_visible);
        self.evaluate_with(fg.is_main_window)
    }

    pub fn current_mode(&self) -> HudMode {
        self.current_mode
    }

    fn evaluate_with(&mut self, is_main_window: bool) -> Option<HudMode> {
        let new_mode = self.determine_mode(is_main_window);
        if new_mode != self.current_mode {
            let old = self.current_mode;
            self.current_mode = new_mode;
            log::info!("[fg_monitor] mode: {:?} → {:?}", old, new_mode);
            Some(new_mode)
        } else {
            None
        }
    }

    fn determine_mode(&self, is_main_window: bool) -> HudMode {
        if !self.hud_enabled || !self.in_voice {
            return HudMode::Hidden;
        }

        if is_main_window {
            return HudMode::Hidden;
        }

        HudMode::Overlay
    }
}

fn query_foreground(_main_window_visible: bool) -> ForegroundInfo {
    #[cfg(target_os = "windows")]
    {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetForegroundWindow};
        unsafe {
            let fg = GetForegroundWindow();
            if fg.0.is_null() {
                return ForegroundInfo {
                    is_main_window: false,
                };
            }

            let is_main = match FindWindowW(None, w!("Mello")) {
                Ok(main_hwnd) => fg == main_hwnd,
                Err(_) => false,
            };

            ForegroundInfo {
                is_main_window: is_main,
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        ForegroundInfo {
            is_main_window: _main_window_visible,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fm() -> ForegroundMonitor {
        ForegroundMonitor::new(true)
    }

    // --- basic mode transitions ---

    #[test]
    fn hidden_when_not_in_voice() {
        let mut m = fm();
        assert_eq!(m.evaluate_with(false), None); // already Hidden
        assert_eq!(m.current_mode(), HudMode::Hidden);
    }

    #[test]
    fn hidden_when_main_window_focused() {
        let mut m = fm();
        m.set_in_voice(true);
        m.evaluate_with(false); // Overlay
        let changed = m.evaluate_with(true);
        assert_eq!(changed, Some(HudMode::Hidden));
    }

    #[test]
    fn overlay_when_other_app_focused() {
        let mut m = fm();
        m.set_in_voice(true);
        let changed = m.evaluate_with(false);
        assert_eq!(changed, Some(HudMode::Overlay));
    }

    // --- settings ---

    #[test]
    fn hud_disabled_always_hidden() {
        let mut m = fm();
        m.set_in_voice(true);
        m.set_hud_enabled(false);
        assert_eq!(m.evaluate_with(false), None); // stays Hidden
    }

    #[test]
    fn live_toggle_hud_off_hides_immediately() {
        let mut m = fm();
        m.set_in_voice(true);
        m.evaluate_with(false); // Overlay
        m.set_hud_enabled(false);
        let changed = m.evaluate_with(false);
        assert_eq!(changed, Some(HudMode::Hidden));
    }

    // --- dedup ---

    #[test]
    fn no_change_returns_none() {
        let mut m = fm();
        m.set_in_voice(true);
        assert_eq!(m.evaluate_with(false), Some(HudMode::Overlay));
        assert_eq!(m.evaluate_with(false), None);
    }
}
