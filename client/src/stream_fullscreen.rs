//! Fullscreen for the watched stream (streaming-quality stage 9).
//!
//! The card's Fullscreen button (or a double-click on the picture) shows the
//! stream on a borderless window that covers the monitor of the app window.
//! On Windows the DComp video visual moves to a second target on that window
//! (`DCompPresenter::show_fullscreen`); the swap chain, libmello's present
//! thread and the decoder stay, so the stream plays on through the switch.
//! Escape, a double-click or Alt+F4 on that window brings the stream back to
//! the card. A pause, a stop or an end of the stream also brings it back,
//! because the card explains those states and the fullscreen window cannot.
//!
//! The native window is behind [`FullscreenSurface`], so the UI harness can
//! drive the state with a fake.

use crate::app_context::AppContext;

/// What shows the stream fullscreen.
pub trait FullscreenSurface {
    /// Show the stream fullscreen. An error leaves it on the card.
    fn enter(&mut self, ctx: &AppContext) -> Result<(), String>;
    /// Bring the stream back to the card.
    fn exit(&mut self, ctx: &AppContext);
}

/// The fullscreen state of the watched stream.
pub struct StreamFullscreen {
    active: bool,
    surface: Box<dyn FullscreenSurface>,
}

impl Default for StreamFullscreen {
    fn default() -> Self {
        Self {
            active: false,
            surface: platform_surface(),
        }
    }
}

impl StreamFullscreen {
    /// Use another surface (tests).
    #[cfg(any(test, feature = "testkit"))]
    pub fn set_surface(&mut self, surface: Box<dyn FullscreenSurface>) {
        self.surface = surface;
    }
}

/// Show the watched stream fullscreen. Nothing happens when no stream is
/// watched or it is paused (the card shows the pause card then).
pub fn enter(ctx: &AppContext) {
    use slint::ComponentHandle as _;
    if !ctx.app.get_is_watching() || ctx.app.get_stream_paused() {
        log::info!("Stream fullscreen: no watched picture to show");
        return;
    }
    let result = {
        let mut fullscreen = ctx.stream_fullscreen.borrow_mut();
        if fullscreen.active {
            return;
        }
        let result = fullscreen.surface.enter(ctx);
        fullscreen.active = result.is_ok();
        result
    };
    let ui = ctx.app.global::<crate::StreamCardUi>();
    match result {
        Ok(()) => {
            log::info!("Stream fullscreen: on");
            ui.set_popover("".into());
            ui.set_fullscreen(true);
        }
        Err(e) => log::warn!("Stream fullscreen did not open: {e}"),
    }
}

/// Bring the stream back to the card. `why` goes to the log.
pub fn exit(ctx: &AppContext, why: &str) {
    use slint::ComponentHandle as _;
    {
        let mut fullscreen = ctx.stream_fullscreen.borrow_mut();
        if !fullscreen.active {
            return;
        }
        fullscreen.active = false;
        fullscreen.surface.exit(ctx);
    }
    log::info!("Stream fullscreen: off ({why})");
    ctx.app
        .global::<crate::StreamCardUi>()
        .set_fullscreen(false);
}

#[cfg(target_os = "windows")]
fn platform_surface() -> Box<dyn FullscreenSurface> {
    Box::new(native::NativeFullscreen::default())
}

#[cfg(not(target_os = "windows"))]
fn platform_surface() -> Box<dyn FullscreenSurface> {
    Box::new(Unsupported)
}

/// macOS has no compositor path for the stream yet (spec 12 §14).
#[cfg(not(target_os = "windows"))]
struct Unsupported;

#[cfg(not(target_os = "windows"))]
impl FullscreenSurface for Unsupported {
    fn enter(&mut self, _ctx: &AppContext) -> Result<(), String> {
        Err("fullscreen needs the DComp presenter (Windows)".into())
    }

    fn exit(&mut self, _ctx: &AppContext) {}
}

#[cfg(target_os = "windows")]
mod native {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Once;
    use std::time::Duration;

    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, GetStockObject, MonitorFromWindow, BLACK_BRUSH, HBRUSH, MONITORINFO,
        MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, LoadCursorW, RegisterClassExW,
        SetForegroundWindow, CS_DBLCLKS, IDC_ARROW, WINDOW_EX_STYLE, WM_CLOSE, WM_KEYDOWN,
        WM_LBUTTONDBLCLK, WNDCLASSEXW, WS_POPUP, WS_VISIBLE,
    };

    use super::FullscreenSurface;
    use crate::app_context::AppContext;

    thread_local! {
        /// What Escape, a double-click or Alt+F4 on the window runs. The
        /// window procedure runs on the UI thread, as the app does.
        static ON_EXIT: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
    }

    /// The borderless window and the second DComp target on it.
    #[derive(Default)]
    pub struct NativeFullscreen {
        window: Option<HWND>,
        owner: Option<HWND>,
    }

    impl FullscreenSurface for NativeFullscreen {
        fn enter(&mut self, ctx: &AppContext) -> Result<(), String> {
            let owner = main_hwnd(ctx).ok_or("no window handle for the app")?;
            let presenter = ctx.dcomp_presenter.borrow();
            let presenter = presenter.as_ref().ok_or("no stream presenter")?;
            let (hwnd, width, height) = open_window(owner)?;
            let exit_ctx = ctx.clone();
            ON_EXIT.with(|slot| {
                *slot.borrow_mut() = Some(Rc::new(move || {
                    super::exit(&exit_ctx, "the fullscreen window asked");
                }))
            });
            if let Err(e) = presenter.show_fullscreen(hwnd.0 as isize, width, height) {
                close_window(hwnd, owner);
                return Err(e);
            }
            self.window = Some(hwnd);
            self.owner = Some(owner);
            Ok(())
        }

        fn exit(&mut self, ctx: &AppContext) {
            // The video goes back first, so the card never shows a gap.
            if let Some(presenter) = ctx.dcomp_presenter.borrow().as_ref() {
                presenter.show_in_card();
            }
            if let (Some(hwnd), Some(owner)) = (self.window.take(), self.owner.take()) {
                close_window(hwnd, owner);
            }
        }
    }

    fn main_hwnd(ctx: &AppContext) -> Option<HWND> {
        use i_slint_backend_winit::WinitWindowAccessor;
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use slint::ComponentHandle as _;
        ctx.app
            .window()
            .with_winit_window(|w: &i_slint_backend_winit::winit::window::Window| {
                match w.window_handle().map(|h| h.as_raw()) {
                    Ok(RawWindowHandle::Win32(h)) => Some(HWND(h.hwnd.get() as *mut _)),
                    _ => None,
                }
            })
            .flatten()
    }

    const CLASS_NAME: windows::core::PCWSTR = w!("MelloStreamFullscreen");

    /// A borderless, black window over the whole monitor of `owner`, owned
    /// by it (it stays above the app and goes with it). Returns its handle
    /// and its size in physical pixels.
    fn open_window(owner: HWND) -> Result<(HWND, u32, u32), String> {
        static REGISTER: Once = Once::new();
        unsafe {
            let instance = GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW: {e}"))?;
            REGISTER.call_once(|| {
                let class = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    style: CS_DBLCLKS,
                    lpfnWndProc: Some(wndproc),
                    hInstance: instance.into(),
                    hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                    // Black bars where the picture's aspect leaves room.
                    hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
                    lpszClassName: CLASS_NAME,
                    ..Default::default()
                };
                if RegisterClassExW(&class) == 0 {
                    log::error!("Stream fullscreen: RegisterClassExW failed");
                }
            });
            let monitor = MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                return Err("GetMonitorInfoW failed".into());
            }
            let rect = info.rcMonitor;
            let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
            if width <= 0 || height <= 0 {
                return Err(format!("monitor of size {width}x{height}"));
            }
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS_NAME,
                w!("Mello stream"),
                WS_POPUP | WS_VISIBLE,
                rect.left,
                rect.top,
                width,
                height,
                Some(owner),
                None,
                Some(instance.into()),
                None,
            )
            .map_err(|e| format!("CreateWindowExW: {e}"))?;
            // The user clicked in the app, so it may take the foreground; the
            // window needs the keyboard for Escape.
            let _ = SetForegroundWindow(hwnd);
            let _ = SetFocus(Some(hwnd));
            log::info!(
                "Stream fullscreen window {hwnd:?}: {width}x{height} at ({}, {})",
                rect.left,
                rect.top
            );
            Ok((hwnd, width as u32, height as u32))
        }
    }

    fn close_window(hwnd: HWND, owner: HWND) {
        ON_EXIT.with(|slot| slot.borrow_mut().take());
        unsafe {
            let _ = DestroyWindow(hwnd);
            let _ = SetForegroundWindow(owner);
        }
    }

    /// Run the exit after the window procedure returns: the exit destroys
    /// this window.
    fn request_exit() {
        if let Some(on_exit) = ON_EXIT.with(|slot| slot.borrow().clone()) {
            slint::Timer::single_shot(Duration::ZERO, move || on_exit());
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_KEYDOWN if wparam.0 == usize::from(VK_ESCAPE.0) => {
                request_exit();
                LRESULT(0)
            }
            WM_LBUTTONDBLCLK | WM_CLOSE => {
                request_exit();
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
