use std::cell::RefCell;
use std::rc::Rc;

use crate::hud_manager::HudMode;

/// Monitors the foreground window to determine whether to show the overlay,
/// mini-player, or hide the HUD entirely.
pub struct ForegroundMonitor {
    current_mode: HudMode,
    game_active: Rc<RefCell<bool>>,
    in_voice: bool,
}

impl ForegroundMonitor {
    pub fn new() -> Self {
        Self {
            current_mode: HudMode::Hidden,
            game_active: Rc::new(RefCell::new(false)),
            in_voice: false,
        }
    }

    pub fn set_game_active(&mut self, active: bool) {
        *self.game_active.borrow_mut() = active;
    }

    pub fn set_in_voice(&mut self, in_voice: bool) {
        self.in_voice = in_voice;
    }

    /// Evaluate and return the current HUD mode based on all signals.
    /// Returns Some(mode) if the mode changed, None if unchanged.
    pub fn evaluate(&mut self, main_window_visible: bool) -> Option<HudMode> {
        let new_mode = self.determine_mode(main_window_visible);
        if new_mode != self.current_mode {
            let old = self.current_mode;
            self.current_mode = new_mode;
            log::info!("[fg_monitor] mode: {:?} → {:?}", old, new_mode);
            Some(new_mode)
        } else {
            None
        }
    }

    pub fn current_mode(&self) -> HudMode {
        self.current_mode
    }

    fn determine_mode(&self, main_window_visible: bool) -> HudMode {
        if !self.in_voice {
            return HudMode::Hidden;
        }

        if is_main_window_focused(main_window_visible) {
            return HudMode::Hidden;
        }

        if *self.game_active.borrow() {
            HudMode::Overlay
        } else {
            HudMode::MiniPlayer
        }
    }
}

/// Check whether the main m3llo window is the foreground window.
/// Compares the foreground HWND directly against the known "Mello" window
/// to avoid false positives from other windows in our process.
fn is_main_window_focused(_main_window_visible: bool) -> bool {
    #[cfg(target_os = "windows")]
    {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetForegroundWindow};
        unsafe {
            let fg = GetForegroundWindow();
            if fg.0.is_null() {
                return false;
            }
            match FindWindowW(None, w!("Mello")) {
                Ok(main_hwnd) => fg == main_hwnd,
                Err(_) => false,
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        _main_window_visible
    }
}
