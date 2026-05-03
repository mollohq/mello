use crate::hud_manager::HudMode;

/// Monitors the foreground window to determine whether to show the overlay,
/// mini-player, or hide the HUD entirely.
pub struct ForegroundMonitor {
    current_mode: HudMode,
    game_pid: Option<u32>,
    in_voice: bool,
    hud_enabled: bool,
    overlay_enabled: bool,
}

struct ForegroundInfo {
    is_main_window: bool,
    pid: Option<u32>,
}

impl ForegroundMonitor {
    pub fn new(hud_enabled: bool, overlay_enabled: bool) -> Self {
        Self {
            current_mode: HudMode::Hidden,
            game_pid: None,
            in_voice: false,
            hud_enabled,
            overlay_enabled,
        }
    }

    pub fn set_game_active(&mut self, active: bool, pid: Option<u32>) {
        self.game_pid = if active { pid } else { None };
    }

    pub fn set_in_voice(&mut self, in_voice: bool) {
        self.in_voice = in_voice;
    }

    pub fn set_hud_enabled(&mut self, enabled: bool) {
        self.hud_enabled = enabled;
    }

    pub fn set_overlay_enabled(&mut self, enabled: bool) {
        self.overlay_enabled = enabled;
    }

    /// Evaluate and return the current HUD mode based on all signals.
    /// Returns Some(mode) if the mode changed, None if unchanged.
    pub fn evaluate(&mut self, main_window_visible: bool) -> Option<HudMode> {
        let fg = query_foreground(main_window_visible);
        self.evaluate_with(fg.is_main_window, fg.pid)
    }

    pub fn current_mode(&self) -> HudMode {
        self.current_mode
    }

    pub fn game_pid(&self) -> Option<u32> {
        self.game_pid
    }

    fn evaluate_with(&mut self, is_main_window: bool, fg_pid: Option<u32>) -> Option<HudMode> {
        let new_mode = self.determine_mode(is_main_window, fg_pid);
        if new_mode != self.current_mode {
            let old = self.current_mode;
            self.current_mode = new_mode;
            log::info!("[fg_monitor] mode: {:?} → {:?}", old, new_mode);
            Some(new_mode)
        } else {
            None
        }
    }

    fn determine_mode(&self, is_main_window: bool, fg_pid: Option<u32>) -> HudMode {
        if !self.hud_enabled || !self.in_voice {
            return HudMode::Hidden;
        }

        if is_main_window {
            return HudMode::Hidden;
        }

        if self.overlay_enabled {
            if let Some(game_pid) = self.game_pid {
                if fg_pid == Some(game_pid) {
                    return HudMode::Overlay;
                }
            }
        }

        HudMode::MiniPlayer
    }
}

fn query_foreground(_main_window_visible: bool) -> ForegroundInfo {
    #[cfg(target_os = "windows")]
    {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            FindWindowW, GetForegroundWindow, GetWindowThreadProcessId,
        };
        unsafe {
            let fg = GetForegroundWindow();
            if fg.0.is_null() {
                return ForegroundInfo {
                    is_main_window: false,
                    pid: None,
                };
            }

            let is_main = match FindWindowW(None, w!("Mello")) {
                Ok(main_hwnd) => fg == main_hwnd,
                Err(_) => false,
            };

            let mut fg_pid: u32 = 0;
            GetWindowThreadProcessId(fg, Some(&mut fg_pid));

            ForegroundInfo {
                is_main_window: is_main,
                pid: if fg_pid != 0 { Some(fg_pid) } else { None },
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        ForegroundInfo {
            is_main_window: _main_window_visible,
            pid: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fm() -> ForegroundMonitor {
        ForegroundMonitor::new(true, true)
    }

    // --- basic mode transitions ---

    #[test]
    fn hidden_when_not_in_voice() {
        let mut m = fm();
        assert_eq!(m.evaluate_with(false, Some(999)), None); // already Hidden
        assert_eq!(m.current_mode(), HudMode::Hidden);
    }

    #[test]
    fn hidden_when_main_window_focused() {
        let mut m = fm();
        m.set_in_voice(true);
        m.evaluate_with(false, Some(999)); // MiniPlayer
        let changed = m.evaluate_with(true, Some(999));
        assert_eq!(changed, Some(HudMode::Hidden));
    }

    #[test]
    fn mini_player_when_other_app_focused() {
        let mut m = fm();
        m.set_in_voice(true);
        let changed = m.evaluate_with(false, Some(555));
        assert_eq!(changed, Some(HudMode::MiniPlayer));
    }

    #[test]
    fn overlay_when_game_is_foreground() {
        let mut m = fm();
        m.set_in_voice(true);
        m.set_game_active(true, Some(1234));
        let changed = m.evaluate_with(false, Some(1234));
        assert_eq!(changed, Some(HudMode::Overlay));
    }

    #[test]
    fn mini_player_when_game_running_but_not_foreground() {
        let mut m = fm();
        m.set_in_voice(true);
        m.set_game_active(true, Some(1234));
        let changed = m.evaluate_with(false, Some(5678));
        assert_eq!(changed, Some(HudMode::MiniPlayer));
    }

    // --- settings ---

    #[test]
    fn hud_disabled_always_hidden() {
        let mut m = fm();
        m.set_in_voice(true);
        m.set_hud_enabled(false);
        assert_eq!(m.evaluate_with(false, Some(999)), None); // stays Hidden
    }

    #[test]
    fn overlay_disabled_falls_through_to_mini_player() {
        let mut m = fm();
        m.set_in_voice(true);
        m.set_game_active(true, Some(1234));
        m.set_overlay_enabled(false);
        let changed = m.evaluate_with(false, Some(1234));
        assert_eq!(changed, Some(HudMode::MiniPlayer));
    }

    #[test]
    fn live_toggle_hud_off_hides_immediately() {
        let mut m = fm();
        m.set_in_voice(true);
        m.evaluate_with(false, Some(999)); // MiniPlayer
        m.set_hud_enabled(false);
        let changed = m.evaluate_with(false, Some(999));
        assert_eq!(changed, Some(HudMode::Hidden));
    }

    #[test]
    fn live_toggle_overlay_off_switches_to_mini_player() {
        let mut m = fm();
        m.set_in_voice(true);
        m.set_game_active(true, Some(1234));
        m.evaluate_with(false, Some(1234)); // Overlay
        m.set_overlay_enabled(false);
        let changed = m.evaluate_with(false, Some(1234));
        assert_eq!(changed, Some(HudMode::MiniPlayer));
    }

    // --- dedup ---

    #[test]
    fn no_change_returns_none() {
        let mut m = fm();
        m.set_in_voice(true);
        assert_eq!(m.evaluate_with(false, Some(555)), Some(HudMode::MiniPlayer));
        assert_eq!(m.evaluate_with(false, Some(555)), None);
    }

    // --- game lifecycle ---

    #[test]
    fn game_exit_switches_overlay_to_mini_player() {
        let mut m = fm();
        m.set_in_voice(true);
        m.set_game_active(true, Some(1234));
        m.evaluate_with(false, Some(1234)); // Overlay
        m.set_game_active(false, None);
        let changed = m.evaluate_with(false, Some(555));
        assert_eq!(changed, Some(HudMode::MiniPlayer));
    }
}
