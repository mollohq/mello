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
/// On non-Windows, we just check visibility as an approximation.
fn is_main_window_focused(main_window_visible: bool) -> bool {
    // When the main window is visible and focused, HUD hides.
    // The Slint event loop on the main thread means we're focused when visible.
    // A more precise check can use GetForegroundWindow + compare HWNDs,
    // but for now visibility is a good proxy since the mini-player/overlay
    // use WS_EX_NOACTIVATE and never take focus.
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        unsafe {
            let fg = GetForegroundWindow();
            if fg.0.is_null() {
                return false;
            }
            // We can't easily compare HWNDs with Slint's window here.
            // Use the main_window_visible flag as primary signal, plus check
            // if the foreground window belongs to our process.
            if !main_window_visible {
                return false;
            }
            let mut fg_pid: u32 = 0;
            windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                fg,
                Some(&mut fg_pid),
            );
            fg_pid == std::process::id()
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        main_window_visible
    }
}
