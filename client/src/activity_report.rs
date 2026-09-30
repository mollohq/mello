//! Reports the window state to the core for the push activity rule
//! (spec 23 §6.2): while the user is at the desktop app, their phone stays
//! quiet. The core combines this with voice, stream and game state and tells
//! the server only when the result changes, so sending every few seconds is
//! cheap.

use std::time::Duration;

use mello_core::Command;
use slint::ComponentHandle;

use crate::app_context::AppContext;
use crate::MainWindow;

const REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// Starts the periodic report. Keep the returned timer alive.
pub fn start(ctx: &AppContext) -> slint::Timer {
    let app = ctx.app.as_weak();
    let cmd = ctx.cmd_tx.clone();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, REPORT_INTERVAL, move || {
        let Some(app) = app.upgrade() else { return };
        let _ = cmd.send(Command::SetWindowActivity {
            foreground: window_foreground(&app),
            input_idle_secs: input_idle_secs(),
        });
    });
    timer
}

/// Visible and focused. A minimized or tray-hidden window, or one behind
/// another app, is not in the foreground.
fn window_foreground(app: &MainWindow) -> bool {
    let window = app.window();
    if !window.is_visible() {
        return false;
    }
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        use i_slint_backend_winit::WinitWindowAccessor;
        // No winit window (another backend): fall back to "visible".
        window
            .with_winit_window(|w: &i_slint_backend_winit::winit::window::Window| w.has_focus())
            .unwrap_or(true)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        true
    }
}

/// Seconds since the last keyboard or mouse input anywhere on the system.
/// 0 when the OS gives no answer, which reads as "in use" (the safe side: no
/// phone push while a desktop may be in use).
#[cfg(target_os = "windows")]
fn input_idle_secs() -> u64 {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `info` is a valid LASTINPUTINFO with cbSize set, as the API requires.
    if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        return 0;
    }
    // Both are milliseconds on the same 32-bit tick counter, which wraps
    // after ~49 days; wrapping_sub keeps the difference right across a wrap.
    // SAFETY: GetTickCount has no preconditions.
    let now = unsafe { GetTickCount() };
    u64::from(now.wrapping_sub(info.dwTime) / 1000)
}

#[cfg(target_os = "macos")]
fn input_idle_secs() -> u64 {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state_id: i32, event_type: u32) -> f64;
    }
    /// kCGEventSourceStateCombinedSessionState
    const COMBINED_SESSION_STATE: i32 = 0;
    /// kCGAnyInputEventType
    const ANY_INPUT_EVENT: u32 = u32::MAX;
    // SAFETY: a plain C function that takes two values and returns a double.
    let secs =
        unsafe { CGEventSourceSecondsSinceLastEventType(COMBINED_SESSION_STATE, ANY_INPUT_EVENT) };
    if secs.is_finite() && secs > 0.0 {
        secs as u64
    } else {
        0
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn input_idle_secs() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The OS call must work in the test process and give a plausible value.
    #[test]
    fn input_idle_is_readable() {
        let secs = input_idle_secs();
        assert!(secs < 365 * 24 * 3600, "implausible idle time: {secs}");
    }
}
