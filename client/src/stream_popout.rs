//! The watched stream in its own window (streaming-quality stage 9).
//!
//! The card's Pop out button opens a normal top-level window: a title bar
//! ("<host> - Mello"), resizable, movable to another monitor, with the
//! system's maximize. On Windows the DComp video visual moves to a second
//! target on that window (`DCompPresenter::show_in_window`) and fits its
//! client area with black bars; the swap chain, libmello's present thread and
//! the decoder stay, so the stream plays on through the switch. The card in
//! the feed then shows "Playing in a window" with a button that brings the
//! stream back.
//!
//! Closing the window brings the stream back to the card, as do a pause, a
//! leave and the end of the stream (the card explains those states, the
//! window has no Slint content). The pop-out and the expanded view
//! (`StreamCardUi.expanded`) exclude each other.
//!
//! The native window is behind [`PopoutSurface`], so the UI harness can
//! drive the state with a fake.

use crate::app_context::AppContext;

/// What shows the stream in its own window.
pub trait PopoutSurface {
    /// Open the window with `title` and move the stream to it. An error
    /// leaves the stream on the card.
    fn open(&mut self, ctx: &AppContext, title: &str) -> Result<(), String>;
    /// Bring the stream back to the card and close the window.
    fn close(&mut self, ctx: &AppContext);
}

/// The pop-out state of the watched stream.
pub struct StreamPopout {
    active: bool,
    surface: Box<dyn PopoutSurface>,
}

impl Default for StreamPopout {
    fn default() -> Self {
        Self {
            active: false,
            surface: platform_surface(),
        }
    }
}

impl StreamPopout {
    /// Use another surface (tests).
    #[cfg(any(test, feature = "testkit"))]
    pub fn set_surface(&mut self, surface: Box<dyn PopoutSurface>) {
        self.surface = surface;
    }
}

/// Show the watched stream in its own window. Nothing happens when no
/// stream is watched or it is paused (the card shows the pause card then).
/// The expanded view closes first: the two exclude each other.
pub fn open(ctx: &AppContext) {
    use slint::ComponentHandle as _;
    if !ctx.app.get_is_watching() || ctx.app.get_stream_paused() {
        log::info!("Stream pop-out: no watched picture to show");
        return;
    }
    if ctx.stream_popout.borrow().active {
        return;
    }
    let ui = ctx.app.global::<crate::StreamCardUi>();
    if ui.get_expanded() {
        crate::handlers::streaming::set_expanded(ctx, false);
    }
    let title = format!("{} - Mello", ctx.app.get_active_streamer_name());
    let result = {
        let mut popout = ctx.stream_popout.borrow_mut();
        let result = popout.surface.open(ctx, &title);
        popout.active = result.is_ok();
        result
    };
    match result {
        Ok(()) => {
            log::info!("Stream pop-out: open");
            ui.set_popover("".into());
            ui.set_popped_out(true);
        }
        Err(e) => log::warn!("Stream pop-out did not open: {e}"),
    }
}

/// Bring the stream back to the card. `why` goes to the log.
pub fn close(ctx: &AppContext, why: &str) {
    use slint::ComponentHandle as _;
    {
        let mut popout = ctx.stream_popout.borrow_mut();
        if !popout.active {
            return;
        }
        popout.active = false;
        popout.surface.close(ctx);
    }
    log::info!("Stream pop-out: closed ({why})");
    ctx.app
        .global::<crate::StreamCardUi>()
        .set_popped_out(false);
}

#[cfg(target_os = "windows")]
fn platform_surface() -> Box<dyn PopoutSurface> {
    Box::new(native::NativePopout::default())
}

#[cfg(not(target_os = "windows"))]
fn platform_surface() -> Box<dyn PopoutSurface> {
    Box::new(Unsupported)
}

/// macOS has no compositor path for the stream yet (spec 12 §14).
#[cfg(not(target_os = "windows"))]
struct Unsupported;

#[cfg(not(target_os = "windows"))]
impl PopoutSurface for Unsupported {
    fn open(&mut self, _ctx: &AppContext, _title: &str) -> Result<(), String> {
        Err("the pop-out needs the DComp presenter (Windows)".into())
    }

    fn close(&mut self, _ctx: &AppContext) {}
}

#[cfg(target_os = "windows")]
mod native {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Once;
    use std::time::Duration;

    use windows::core::{w, HSTRING};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH, HBRUSH};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect,
        LoadCursorW, RegisterClassExW, SendMessageW, SetForegroundWindow, SetWindowPos,
        CW_USEDEFAULT, IDC_ARROW, SIZE_MINIMIZED, SWP_NOACTIVATE, SWP_NOZORDER, WINDOW_EX_STYLE,
        WM_CLOSE, WM_DPICHANGED, WM_GETICON, WM_SETICON, WM_SIZE, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
        WS_VISIBLE,
    };

    use super::PopoutSurface;
    use crate::app_context::AppContext;

    /// What the window procedure calls. It runs on the UI thread, as the
    /// app does.
    #[derive(Default)]
    struct Handlers {
        on_close: Option<Rc<dyn Fn()>>,
        on_resize: Option<Rc<dyn Fn(u32, u32)>>,
    }

    thread_local! {
        static HANDLERS: RefCell<Handlers> = RefCell::new(Handlers::default());
    }

    /// The pop-out window and the second DComp target on it.
    #[derive(Default)]
    pub struct NativePopout {
        window: Option<HWND>,
        owner: Option<HWND>,
    }

    impl PopoutSurface for NativePopout {
        fn open(&mut self, ctx: &AppContext, title: &str) -> Result<(), String> {
            let app_window = main_hwnd(ctx).ok_or("no window handle for the app")?;
            let presenter = ctx.dcomp_presenter.borrow();
            let presenter = presenter.as_ref().ok_or("no stream presenter")?;
            let hwnd = open_window(app_window, title, presenter.stream_size())?;
            let (width, height) = client_size(hwnd);
            let close_ctx = ctx.clone();
            let resize_ctx = ctx.clone();
            HANDLERS.with(|h| {
                *h.borrow_mut() = Handlers {
                    on_close: Some(Rc::new(move || {
                        super::close(&close_ctx, "the window closed");
                    })),
                    on_resize: Some(Rc::new(move |w, h| {
                        // A resize runs inside the system's move loop: never
                        // wait on a presenter borrow there.
                        if let Ok(presenter) = resize_ctx.dcomp_presenter.try_borrow() {
                            if let Some(presenter) = presenter.as_ref() {
                                presenter.resize_window(w, h);
                            }
                        }
                    })),
                }
            });
            if let Err(e) = presenter.show_in_window(hwnd.0 as isize, width, height) {
                close_window(hwnd, app_window);
                return Err(e);
            }
            self.window = Some(hwnd);
            self.owner = Some(app_window);
            Ok(())
        }

        fn close(&mut self, ctx: &AppContext) {
            // The video goes back first, so the card never shows a gap.
            if let Some(presenter) = ctx.dcomp_presenter.borrow().as_ref() {
                presenter.show_in_card();
            }
            if let (Some(hwnd), Some(app_window)) = (self.window.take(), self.owner.take()) {
                close_window(hwnd, app_window);
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

    fn client_size(hwnd: HWND) -> (u32, u32) {
        let mut rect = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut rect);
        }
        (
            (rect.right - rect.left).max(0) as u32,
            (rect.bottom - rect.top).max(0) as u32,
        )
    }

    const CLASS_NAME: windows::core::PCWSTR = w!("MelloStreamPopout");

    /// The client size of a new pop-out: 60 % of the app window's client
    /// width (so it has the app's DPI), the stream's aspect.
    fn initial_client_size(app_window: HWND, stream: (u32, u32)) -> (i32, i32) {
        let (app_w, _) = client_size(app_window);
        let width = (f64::from(app_w.max(640)) * 0.6).round();
        let (sw, sh) = (f64::from(stream.0.max(1)), f64::from(stream.1.max(1)));
        (width as i32, (width * sh / sw).round() as i32)
    }

    /// A normal top-level window (title bar, resizable, the system's
    /// maximize), black where the picture's aspect leaves room, with the
    /// app's icons.
    fn open_window(app_window: HWND, title: &str, stream: (u32, u32)) -> Result<HWND, String> {
        static REGISTER: Once = Once::new();
        let style = WS_OVERLAPPEDWINDOW;
        unsafe {
            let instance = GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW: {e}"))?;
            REGISTER.call_once(|| {
                let class = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(wndproc),
                    hInstance: instance.into(),
                    hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                    hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
                    lpszClassName: CLASS_NAME,
                    ..Default::default()
                };
                if RegisterClassExW(&class) == 0 {
                    log::error!("Stream pop-out: RegisterClassExW failed");
                }
            });
            let (cw, ch) = initial_client_size(app_window, stream);
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: cw,
                bottom: ch,
            };
            let _ = AdjustWindowRectEx(&mut rect, style, false, WINDOW_EX_STYLE(0));
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS_NAME,
                &HSTRING::from(title),
                style | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                rect.right - rect.left,
                rect.bottom - rect.top,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .map_err(|e| format!("CreateWindowExW: {e}"))?;
            // The app's icons on the title bar and the taskbar.
            for kind in [0usize, 1usize] {
                let icon = SendMessageW(app_window, WM_GETICON, Some(WPARAM(kind)), None);
                if icon.0 != 0 {
                    let _ =
                        SendMessageW(hwnd, WM_SETICON, Some(WPARAM(kind)), Some(LPARAM(icon.0)));
                }
            }
            let _ = SetForegroundWindow(hwnd);
            log::info!("Stream pop-out window {hwnd:?}: client {cw}x{ch}, \"{title}\"");
            Ok(hwnd)
        }
    }

    fn close_window(hwnd: HWND, app_window: HWND) {
        HANDLERS.with(|h| *h.borrow_mut() = Handlers::default());
        unsafe {
            let _ = DestroyWindow(hwnd);
            let _ = SetForegroundWindow(app_window);
        }
    }

    /// Close after the window procedure returns: the close destroys this
    /// window.
    fn request_close() {
        if let Some(on_close) = HANDLERS.with(|h| h.borrow().on_close.clone()) {
            slint::Timer::single_shot(Duration::ZERO, move || on_close());
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_CLOSE => {
                request_close();
                LRESULT(0)
            }
            WM_SIZE => {
                if wparam.0 != SIZE_MINIMIZED as usize {
                    let (w, h) = (
                        (lparam.0 & 0xFFFF) as u32,
                        ((lparam.0 >> 16) & 0xFFFF) as u32,
                    );
                    if let Some(on_resize) = HANDLERS.with(|h| h.borrow().on_resize.clone()) {
                        on_resize(w, h);
                    }
                }
                LRESULT(0)
            }
            // Moved to a monitor with another scale: take the size Windows
            // suggests for it.
            WM_DPICHANGED => {
                let suggested = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
