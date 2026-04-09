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
        if !self.hud_enabled || !self.in_voice {
            return HudMode::Hidden;
        }

        let fg_info = foreground_window_info(main_window_visible);

        if fg_info.is_main_window {
            return HudMode::Hidden;
        }

        if self.overlay_enabled {
            if let Some(game_pid) = self.game_pid {
                if fg_info.pid == Some(game_pid) {
                    return HudMode::Overlay;
                }
            }
        }

        HudMode::MiniPlayer
    }
}

struct ForegroundInfo {
    is_main_window: bool,
    pid: Option<u32>,
}

fn foreground_window_info(_main_window_visible: bool) -> ForegroundInfo {
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
