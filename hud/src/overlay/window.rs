#[cfg(target_os = "windows")]
use crate::protocol::HudState;

#[cfg(target_os = "windows")]
use super::renderer::D2DRenderer;

/// Win32 overlay window with D2D rendering.
/// Uses WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW
#[cfg(target_os = "windows")]
pub struct Win32OverlayWindow {
    hwnd: windows::Win32::Foundation::HWND,
    renderer: D2DRenderer,
    state: HudState,
    needs_render: bool,
    width: u32,
    height: u32,
}

#[cfg(target_os = "windows")]
impl Win32OverlayWindow {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        use windows::core::*;
        use windows::Win32::UI::WindowsAndMessaging::*;

        let renderer = D2DRenderer::new()?;
        let width = 300u32;
        let height = 200u32;

        unsafe {
            let class_name = w!("m3llo_overlay");
            let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(overlay_wndproc),
                hInstance: hinstance.into(),
                lpszClassName: class_name,
                ..Default::default()
            };
            RegisterClassExW(&wc);

            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW,
                class_name,
                w!("m3llo Overlay"),
                WS_POPUP,
                16,
                16,
                width as i32,
                height as i32,
                None,
                None,
                Some(hinstance.into()),
                None,
            )?;

            log::info!("[overlay] window created hwnd={:?}", hwnd);

            Ok(Self {
                hwnd,
                renderer,
                state: HudState::default(),
                needs_render: false,
                width,
                height,
            })
        }
    }

    pub fn show(&self) {
        use windows::Win32::UI::WindowsAndMessaging::*;
        log::info!("[overlay] show hwnd={:?}", self.hwnd);
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let visible = windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(self.hwnd);
            log::info!("[overlay] after show: visible={}", visible.as_bool());
        }
    }

    pub fn hide(&self) {
        use windows::Win32::UI::WindowsAndMessaging::*;
        log::debug!("[overlay] hide");
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    pub fn update_state(&mut self, state: &HudState) {
        self.state = state.clone();
        self.needs_render = true;

        // Resize window to fit content
        let new_height = self.renderer.compute_height(&self.state);
        if new_height != self.height && new_height > 0 {
            self.height = new_height;
            unsafe {
                use windows::Win32::UI::WindowsAndMessaging::*;
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    self.width as i32,
                    self.height as i32,
                    SWP_NOMOVE | SWP_NOACTIVATE,
                );
            }
        }
    }

    /// Re-assert TOPMOST positioning. Called on each tick while overlay is visible
    /// to stay above game windows that continuously reclaim the top z-order.
    pub fn ensure_topmost(&self) {
        use windows::Win32::UI::WindowsAndMessaging::*;
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    pub fn render(&mut self) {
        if !self.needs_render {
            return;
        }
        self.needs_render = false;

        match self.render_to_layered_window() {
            Ok(()) => {
                log::debug!(
                    "[overlay] rendered {}x{} at hwnd={:?}",
                    self.width,
                    self.height,
                    self.hwnd
                );
            }
            Err(e) => {
                log::warn!("[overlay] render failed: {}, recreating D2D resources", e);
                match D2DRenderer::new() {
                    Ok(r) => {
                        self.renderer = r;
                        self.needs_render = true;
                    }
                    Err(e2) => {
                        log::error!("[overlay] D2D recreation failed: {}", e2);
                    }
                }
            }
        }
    }

    fn render_to_layered_window(&self) -> Result<(), Box<dyn std::error::Error>> {
        use windows::Win32::Foundation::*;
        use windows::Win32::Graphics::Gdi::*;
        use windows::Win32::UI::WindowsAndMessaging::*;

        let w = self.width as i32;
        let h = self.height as i32;

        unsafe {
            let screen_dc = GetDC(None);
            let mem_dc = CreateCompatibleDC(Some(screen_dc));

            // Create a 32-bit DIB section for per-pixel alpha
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let old = SelectObject(mem_dc, dib.into());

            // Render D2D content into the memory DC
            self.renderer
                .render(&self.state, mem_dc, self.width, self.height)?;

            // Premultiply alpha for UpdateLayeredWindow
            if !bits.is_null() {
                let pixel_count = (w * h) as usize;
                let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, pixel_count);
                for px in pixels.iter_mut() {
                    let b = (*px & 0xFF) as u16;
                    let g = ((*px >> 8) & 0xFF) as u16;
                    let r = ((*px >> 16) & 0xFF) as u16;
                    let a = ((*px >> 24) & 0xFF) as u16;
                    if a < 255 && a > 0 {
                        let rb = ((r * a + 127) / 255) as u32;
                        let gb = ((g * a + 127) / 255) as u32;
                        let bb = ((b * a + 127) / 255) as u32;
                        *px = bb | (gb << 8) | (rb << 16) | ((a as u32) << 24);
                    }
                }
            }

            // Update the layered window with per-pixel alpha
            let pt_src = POINT { x: 0, y: 0 };
            let size = SIZE { cx: w, cy: h };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };

            let ulw_result = UpdateLayeredWindow(
                self.hwnd,
                Some(screen_dc),
                None,
                Some(&size),
                Some(mem_dc),
                Some(&pt_src),
                COLORREF(0),
                Some(&blend as *const _),
                ULW_ALPHA,
            );
            if let Err(ref e) = ulw_result {
                log::error!("[overlay] UpdateLayeredWindow failed: {}", e);
            }

            SelectObject(mem_dc, old);
            let _ = DeleteObject(dib.into());
            let _ = DeleteDC(mem_dc);
            ReleaseDC(None, screen_dc);
        }

        Ok(())
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn overlay_wndproc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::*;
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
