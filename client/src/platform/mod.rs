#[cfg(target_os = "windows")]
pub mod crash_handler;
pub mod exe_icon;
#[cfg(target_os = "windows")]
pub mod hang_dump;
pub mod hotkeys;
#[cfg(target_os = "linux")]
pub mod linux_desktop;
#[cfg(target_os = "linux")]
pub mod linux_gpu;
#[cfg(target_os = "linux")]
pub mod linux_openh264;
#[cfg(target_os = "linux")]
pub mod linux_shortcuts;
#[cfg(target_os = "linux")]
pub mod linux_single_instance;
#[cfg(target_os = "linux")]
pub mod linux_tray;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub mod macos_url_events;
#[cfg(target_os = "windows")]
pub mod taskbar_toolbar;

use tray_icon::menu::{CheckMenuItem, Menu, MenuItem};
#[cfg(not(target_os = "linux"))]
use tray_icon::menu::{MenuId, PredefinedMenuItem};
#[cfg(not(target_os = "linux"))]
use tray_icon::TrayIconBuilder;
use tray_icon::{Icon, TrayIcon, TrayIconEvent};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VoiceState {
    Inactive,
    Connected,
    Speaking,
    Muted,
}

/// Native tray handles. Absent when the tray could not be created, or when
/// running headless (tests, CI) — the surrounding `StatusItem` degrades to
/// no-ops rather than forcing every caller to handle an `Option`.
/// Linux uses `linux_tray` instead.
#[cfg_attr(target_os = "linux", allow(dead_code))]
struct TrayHandles {
    _tray: TrayIcon,
    _menu: Menu,
    mute_item: CheckMenuItem,
    leave_item: MenuItem,
}

pub struct StatusItem {
    tray: Option<TrayHandles>,
    #[cfg(target_os = "linux")]
    linux: Option<linux_tray::LinuxTray>,
    current_state: VoiceState,
    muted: bool,
    /// Tests: a headless item that reports a tray, as on a desktop with one.
    #[cfg(any(test, feature = "testkit"))]
    stand_in: bool,
}

impl StatusItem {
    /// A `StatusItem` with no native tray. Every method becomes a no-op.
    ///
    /// Used headlessly by the test harness, and as the fallback when tray
    /// creation fails at startup — a missing tray icon is a degraded
    /// experience, not a reason to refuse to launch.
    pub fn disabled() -> Self {
        Self {
            tray: None,
            #[cfg(target_os = "linux")]
            linux: None,
            current_state: VoiceState::Inactive,
            muted: false,
            #[cfg(any(test, feature = "testkit"))]
            stand_in: false,
        }
    }

    /// Linux: a StatusNotifierItem (`linux_tray`). tray-icon 0.21 builds its
    /// Linux menu with GTK, which needs a GTK main loop the client does not
    /// run; its menu constructor panicked.
    #[cfg(target_os = "linux")]
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let linux = linux_tray::LinuxTray::spawn(VoiceState::Inactive)?;
        Ok(Self {
            linux: Some(linux),
            ..Self::disabled()
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let icon = Self::render_icon(VoiceState::Inactive);

        let menu = Menu::new();
        menu.append(&MenuItem::with_id(
            MenuId::new("tray_open"),
            format!("Open {}", crate::APP_NAME),
            true,
            None,
        ))
        .ok();
        menu.append(&PredefinedMenuItem::separator()).ok();
        let mute_item =
            CheckMenuItem::with_id(MenuId::new("tray_mute"), "Mute", false, false, None);
        menu.append(&mute_item).ok();
        let leave_item = MenuItem::with_id(MenuId::new("tray_leave"), "Leave Voice", false, None);
        menu.append(&leave_item).ok();
        menu.append(&PredefinedMenuItem::separator()).ok();
        menu.append(&MenuItem::with_id(
            MenuId::new("tray_quit"),
            format!("Quit {}", crate::APP_NAME),
            true,
            None,
        ))
        .ok();

        let builder = TrayIconBuilder::new()
            .with_icon(icon)
            .with_tooltip(crate::APP_NAME)
            .with_menu(Box::new(menu.clone()))
            .with_menu_on_left_click(false);

        let tray = builder.build()?;

        Ok(Self {
            tray: Some(TrayHandles {
                _tray: tray,
                _menu: menu,
                mute_item,
                leave_item,
            }),
            current_state: VoiceState::Inactive,
            muted: false,
            #[cfg(any(test, feature = "testkit"))]
            stand_in: false,
        })
    }

    /// Tests: no native tray, every method a no-op like [`Self::disabled`],
    /// but [`Self::is_present`] is true, so close hides to the tray as on a
    /// desktop that has one.
    #[cfg(any(test, feature = "testkit"))]
    pub fn stand_in() -> Self {
        Self {
            stand_in: true,
            ..Self::disabled()
        }
    }

    /// True when a native tray icon exists. Without one, hiding the window
    /// would leave the user no way to bring it back.
    pub fn is_present(&self) -> bool {
        #[cfg(any(test, feature = "testkit"))]
        if self.stand_in {
            return true;
        }
        #[cfg(target_os = "linux")]
        if let Some(linux) = self.linux.as_ref() {
            // A StatusNotifierItem without a host (GNOME without the
            // AppIndicator extension) is not on screen.
            return linux.is_shown();
        }
        self.tray.is_some()
    }

    /// Linux: the tray menu ids since the last call, for
    /// `poll_loop::handle_menu_item` (and `linux_tray::TOGGLE_WINDOW` for a
    /// click on the icon). UI thread.
    #[cfg(target_os = "linux")]
    pub fn poll_linux(&self) -> Vec<String> {
        self.linux.as_ref().map(|l| l.poll()).unwrap_or_default()
    }

    pub fn set_voice_state(&mut self, state: VoiceState) {
        if state == self.current_state {
            return;
        }
        self.current_state = state;
        self.muted = matches!(state, VoiceState::Muted);

        #[cfg(target_os = "linux")]
        if let Some(linux) = self.linux.as_ref() {
            linux.set_state(state, self.muted);
        }

        let Some(tray) = self.tray.as_ref() else {
            return;
        };
        tray._tray.set_icon(Some(Self::render_icon(state))).ok();

        let in_voice = matches!(
            state,
            VoiceState::Connected | VoiceState::Speaking | VoiceState::Muted
        );
        tray.mute_item.set_enabled(in_voice);
        tray.leave_item.set_enabled(in_voice);
        tray.mute_item
            .set_checked(matches!(state, VoiceState::Muted));
    }

    pub fn set_mute_checked(&mut self, muted: bool) {
        self.muted = muted;
        #[cfg(target_os = "linux")]
        if let Some(linux) = self.linux.as_ref() {
            linux.set_muted(muted);
        }
        if let Some(tray) = self.tray.as_ref() {
            tray.mute_item.set_checked(muted);
        }
    }

    /// Poll for tray icon click events.
    pub fn poll_tray_event() -> Option<TrayIconEvent> {
        TrayIconEvent::receiver().try_recv().ok()
    }

    fn render_icon(state: VoiceState) -> Icon {
        let (rgba, size) = state_icon_rgba(state);
        Icon::from_rgba(rgba, size, size).expect("icon render failed")
    }
}

/// The tray icon of a voice state: a filled circle in a 22x22 RGBA buffer.
/// Returns the pixels and the side length.
fn state_icon_rgba(state: VoiceState) -> (Vec<u8>, u32) {
    let (r, g, b, a): (u8, u8, u8, u8) = match state {
        VoiceState::Inactive => (255, 255, 255, 153), // white 60%
        VoiceState::Connected => (255, 255, 255, 255), // white 100%
        VoiceState::Speaking => (68, 204, 68, 255),   // green
        VoiceState::Muted => (255, 68, 68, 255),      // red
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

    (rgba, size as u32)
}

/// The main window's handle. `None` until the event loop creates the window.
///
/// Use this, not `FindWindowW` by title: a title lookup also matches any
/// other top-level window with the same title, such as an Explorer folder.
#[cfg(target_os = "windows")]
pub fn main_window_hwnd(app: &crate::MainWindow) -> Option<windows::Win32::Foundation::HWND> {
    use i_slint_backend_winit::WinitWindowAccessor;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::ComponentHandle as _;
    use windows::Win32::Foundation::HWND;
    app.window()
        .with_winit_window(|w: &i_slint_backend_winit::winit::window::Window| {
            match w.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(h)) => Some(HWND(h.hwnd.get() as *mut _)),
                _ => None,
            }
        })
        .flatten()
}

/// Put the exe's icon on a window's title bar, taskbar button and Alt+Tab
/// entry. Winit registers its window class without an icon, so Windows draws
/// its default icon. Runs when winit creates the window, at its scale.
#[cfg(target_os = "windows")]
pub fn set_exe_icon<T: slint::ComponentHandle + 'static>(component: &T) {
    use i_slint_backend_winit::winit::dpi::PhysicalSize;
    use i_slint_backend_winit::winit::platform::windows::{IconExtWindows, WindowExtWindows};
    use i_slint_backend_winit::winit::window::Icon;
    use i_slint_backend_winit::WinitWindowAccessor;

    // build.rs embeds assets/icons/mello.ico with winresource's default
    // application icon ID.
    const EXE_ICON_ID: u16 = 1;

    let weak = component.as_weak();
    let spawned = slint::spawn_local(async move {
        let Some(component) = weak.upgrade() else {
            return;
        };
        let window = match component.window().winit_window().await {
            Ok(w) => w,
            Err(e) => {
                log::warn!("[window] no native window for the icon: {e}");
                return;
            }
        };
        // The small icon (title bar) is 16 px and the large icon (taskbar,
        // Alt+Tab) 32 px at 100 %. Windows picks the nearest frame of the .ico.
        let scale = window.scale_factor();
        let size = |logical: f64| {
            let px = (logical * scale).round() as u32;
            Some(PhysicalSize::new(px, px))
        };
        match (
            Icon::from_resource(EXE_ICON_ID, size(16.0)),
            Icon::from_resource(EXE_ICON_ID, size(32.0)),
        ) {
            (Ok(small), Ok(big)) => {
                window.set_window_icon(Some(small));
                window.set_taskbar_icon(Some(big));
                log::info!("[window] exe icon set, scale {scale}");
            }
            (small, big) => log::warn!(
                "[window] exe icon not loaded: small {:?}, big {:?}",
                small.err(),
                big.err()
            ),
        }
    });
    if let Err(e) = spawned {
        log::warn!("[window] could not schedule the window icon: {e}");
    }
}

/// Let the running instance take the foreground. Call from a second
/// instance before it relays its message: the user started this process, so
/// Windows lets it pass the right on. Without it, Windows can refuse the
/// window of the running instance and flash its taskbar button.
#[cfg(target_os = "windows")]
pub fn allow_running_instance_to_foreground() {
    use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};
    if let Err(e) = unsafe { AllowSetForegroundWindow(ASFW_ANY) } {
        log::warn!("[ipc] AllowSetForegroundWindow failed: {e}");
    }
}

/// Bring the main window to the foreground on Windows.
/// On other platforms, this is a no-op (Slint's show() is sufficient).
pub fn bring_main_window_to_front(app: &crate::MainWindow) {
    #[cfg(not(target_os = "windows"))]
    let _ = app;
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
        use windows::Win32::UI::WindowsAndMessaging::*;

        let Some(hwnd) = main_window_hwnd(app) else {
            return;
        };
        unsafe {
            let fg = GetForegroundWindow();
            let fg_thread = GetWindowThreadProcessId(fg, None);
            let our_thread = GetCurrentThreadId();

            // Temporarily attach to the foreground thread's input queue
            // so Windows allows us to call SetForegroundWindow.
            if fg_thread != our_thread {
                let _ = AttachThreadInput(our_thread, fg_thread, true);
            }

            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
            let _ = BringWindowToTop(hwnd);

            if fg_thread != our_thread {
                let _ = AttachThreadInput(our_thread, fg_thread, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_status_item_has_no_tray() {
        assert!(!StatusItem::disabled().is_present());
    }

    /// Without a session bus or a StatusNotifierWatcher (CI, GNOME without
    /// the extension) the tray does not start and close must not hide the
    /// window. With a watcher, the started tray is on screen.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_tray_is_present_exactly_when_it_started() {
        match StatusItem::new() {
            Ok(item) => assert!(item.is_present()),
            Err(_) => assert!(!StatusItem::disabled().is_present()),
        }
    }
}
