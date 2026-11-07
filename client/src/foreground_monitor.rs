use crate::hud_manager::HudMode;

/// Monitors the foreground window to determine whether to show the overlay
/// or hide the HUD entirely.
pub struct ForegroundMonitor {
    current_mode: HudMode,
    in_voice: bool,
    hud_enabled: bool,
    /// Tracked game PID — used by the streaming feature, not by HUD mode logic.
    game_pid: Option<u32>,
    /// The sensor's game id and exe basename of `game_pid`. They come from
    /// the same `GameDetected` event as the pid, so the quick stream start
    /// can send the identity of the process that it captures.
    game_id: String,
    game_exe: String,
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
            game_id: String::new(),
            game_exe: String::new(),
        }
    }

    pub fn set_game_active(&mut self, active: bool, pid: Option<u32>) {
        self.game_pid = if active { pid } else { None };
        self.game_id.clear();
        self.game_exe.clear();
    }

    /// Track the detected game: its pid, the sensor's game id and the exe
    /// basename, all from one `GameDetected` event.
    pub fn set_detected_game(&mut self, pid: u32, game_id: &str, exe_path: &str) {
        self.game_pid = Some(pid);
        self.game_id = game_id.to_string();
        self.game_exe = exe_basename(exe_path).to_string();
    }

    pub fn game_pid(&self) -> Option<u32> {
        self.game_pid
    }

    /// The sensor's game id of the tracked game. Empty when none is tracked.
    pub fn game_id(&self) -> &str {
        &self.game_id
    }

    /// The exe basename of the tracked game. Empty when it is not known.
    pub fn game_exe(&self) -> &str {
        &self.game_exe
    }

    pub fn set_in_voice(&mut self, in_voice: bool) {
        self.in_voice = in_voice;
    }

    pub fn set_hud_enabled(&mut self, enabled: bool) {
        self.hud_enabled = enabled;
    }

    /// Evaluate and return the current HUD mode based on all signals.
    /// Returns Some(mode) if the mode changed, None if unchanged.
    pub fn evaluate(&mut self, app: &crate::MainWindow) -> Option<HudMode> {
        let fg = query_foreground(app);
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

/// The file name of an exe path. The sensor reports Windows paths, so both
/// separators count on every platform.
fn exe_basename(exe_path: &str) -> &str {
    exe_path.rsplit(['\\', '/']).next().unwrap_or(exe_path)
}

fn query_foreground(app: &crate::MainWindow) -> ForegroundInfo {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        let fg = unsafe { GetForegroundWindow() };
        if fg.0.is_null() {
            return ForegroundInfo {
                is_main_window: false,
            };
        }

        ForegroundInfo {
            is_main_window: crate::platform::main_window_hwnd(app).is_some_and(|main| fg == main),
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        use slint::ComponentHandle as _;
        ForegroundInfo {
            is_main_window: app.window().is_visible(),
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

    // --- detected game ---

    #[test]
    fn detected_game_keeps_the_exe_basename_and_game_id() {
        let mut m = fm();
        m.set_detected_game(
            4242,
            "hearthstone",
            r"C:\Program Files (x86)\Hearthstone\Hearthstone.exe",
        );
        assert_eq!(m.game_pid(), Some(4242));
        assert_eq!(m.game_id(), "hearthstone");
        assert_eq!(m.game_exe(), "Hearthstone.exe");

        m.set_game_active(false, None);
        assert_eq!(m.game_pid(), None);
        assert_eq!(m.game_id(), "");
        assert_eq!(m.game_exe(), "");
    }
}
