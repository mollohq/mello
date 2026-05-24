#[cfg(target_os = "windows")]
use crate::protocol::HudState;

#[cfg(target_os = "windows")]
use super::renderer::D2DRenderer;

#[cfg(target_os = "windows")]
pub struct Win32OverlayWindow {
    hwnd: windows::Win32::Foundation::HWND,
    renderer: D2DRenderer,
    swap_chain: windows::Win32::Graphics::Dxgi::IDXGISwapChain1,
    dcomp_device: windows::Win32::Graphics::DirectComposition::IDCompositionDevice,
    #[allow(dead_code)]
    dcomp_target: windows::Win32::Graphics::DirectComposition::IDCompositionTarget,
    d2d_dc: windows::Win32::Graphics::Direct2D::ID2D1DeviceContext,
    state: HudState,
    needs_render: bool,
    width: u32,
    height: u32,
    opacity: f32,
}

#[cfg(target_os = "windows")]
impl Win32OverlayWindow {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        use windows::core::*;
        use windows::Win32::Graphics::Direct2D::*;
        use windows::Win32::Graphics::Direct3D::*;
        use windows::Win32::Graphics::Direct3D11::*;
        use windows::Win32::Graphics::DirectComposition::*;
        use windows::Win32::Graphics::Dxgi::Common::*;
        use windows::Win32::Graphics::Dxgi::*;
        use windows::Win32::UI::WindowsAndMessaging::*;

        let width = 230u32;
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
                WS_EX_NOREDIRECTIONBITMAP
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

            // D3D11 device
            let mut d3d_device: Option<ID3D11Device> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                windows::Win32::Foundation::HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d3d_device),
                None,
                None,
            )?;
            let d3d_device = d3d_device.ok_or("D3D11CreateDevice returned None")?;
            let dxgi_device: IDXGIDevice = d3d_device.cast()?;

            // DirectComposition
            let dcomp_device: IDCompositionDevice = DCompositionCreateDevice(&dxgi_device)?;
            let dcomp_target = dcomp_device.CreateTargetForHwnd(hwnd, true)?;
            let dcomp_visual = dcomp_device.CreateVisual()?;

            // DXGI swap chain for composition (supports premultiplied alpha)
            let adapter = dxgi_device.GetAdapter()?;
            let dxgi_factory: IDXGIFactory2 = adapter.GetParent()?;

            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            let swap_chain: IDXGISwapChain1 =
                dxgi_factory.CreateSwapChainForComposition(&d3d_device, &desc, None)?;

            // Bind swap chain to DComp visual tree
            dcomp_visual.SetContent(&swap_chain)?;
            dcomp_target.SetRoot(&dcomp_visual)?;
            dcomp_device.Commit()?;

            // D2D device context for rendering to the swap chain
            let d2d_factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d_device = d2d_factory.CreateDevice(&dxgi_device)?;
            let d2d_dc = d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            d2d_dc.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            d2d_dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);

            let renderer = D2DRenderer::new(d2d_factory.into())?;

            log::info!("[overlay] DComp pipeline initialized");

            Ok(Self {
                hwnd,
                renderer,
                swap_chain,
                dcomp_device,
                dcomp_target,
                d2d_dc,
                state: HudState::default(),
                needs_render: false,
                width,
                height,
                opacity: 0.8,
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
            let visible = IsWindowVisible(self.hwnd);
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

    pub fn set_opacity(&mut self, opacity: f32) {
        if (self.opacity - opacity).abs() > f32::EPSILON {
            self.opacity = opacity;
            self.needs_render = true;
        }
    }

    pub fn update_state(&mut self, state: &HudState) {
        self.state = state.clone();
        self.needs_render = true;

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
            self.resize_swap_chain();
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

    pub fn log_diagnostics(&self) {
        #[cfg(target_os = "windows")]
        unsafe {
            use windows::Win32::Graphics::Dwm::*;
            use windows::Win32::Graphics::Gdi::*;
            use windows::Win32::UI::WindowsAndMessaging::*;

            match DwmIsCompositionEnabled() {
                Ok(enabled) => log::info!("[diag] DWM composition enabled: {}", enabled.as_bool()),
                Err(e) => log::warn!("[diag] DwmIsCompositionEnabled failed: {}", e),
            }

            let fg = GetForegroundWindow();
            let mut title_buf = [0u16; 256];
            let len = GetWindowTextW(fg, &mut title_buf);
            let title = String::from_utf16_lossy(&title_buf[..len as usize]);
            let ex_style = GetWindowLongW(fg, GWL_EXSTYLE) as u32;
            let style = GetWindowLongW(fg, GWL_STYLE) as u32;

            let mut fg_rect = windows::Win32::Foundation::RECT::default();
            let _ = GetWindowRect(fg, &mut fg_rect);

            let mut cloaked: u32 = 0;
            let _ = DwmGetWindowAttribute(
                fg,
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut _,
                std::mem::size_of::<u32>() as u32,
            );

            log::info!(
                "[diag] foreground hwnd={:?} title={:?} style=0x{:08X} ex_style=0x{:08X} cloaked={} rect={}x{}",
                fg,
                title,
                style,
                ex_style,
                cloaked,
                fg_rect.right - fg_rect.left,
                fg_rect.bottom - fg_rect.top,
            );

            let our_visible = IsWindowVisible(self.hwnd).as_bool();
            let our_ex_style = GetWindowLongW(self.hwnd, GWL_EXSTYLE) as u32;
            let mut our_rect = windows::Win32::Foundation::RECT::default();
            let _ = GetWindowRect(self.hwnd, &mut our_rect);

            log::info!(
                "[diag] overlay hwnd={:?} visible={} ex_style=0x{:08X} pos=({},{}) size={}x{}",
                self.hwnd,
                our_visible,
                our_ex_style,
                our_rect.left,
                our_rect.top,
                our_rect.right - our_rect.left,
                our_rect.bottom - our_rect.top,
            );

            let mut z_pos = 0u32;
            let mut found = false;
            if let Ok(mut w) = GetTopWindow(None) {
                loop {
                    if w == self.hwnd {
                        found = true;
                        break;
                    }
                    z_pos += 1;
                    match GetWindow(w, GW_HWNDNEXT) {
                        Ok(next) => w = next,
                        Err(_) => break,
                    }
                }
            }
            if found {
                log::info!("[diag] overlay z-order position: {} (0 = topmost)", z_pos);
            } else {
                log::warn!("[diag] overlay hwnd not found in z-order walk!");
            }

            if let Ok(above_fg) = GetWindow(fg, GW_HWNDPREV) {
                let mut above_title_buf = [0u16; 256];
                let above_len = GetWindowTextW(above_fg, &mut above_title_buf);
                let above_title = String::from_utf16_lossy(&above_title_buf[..above_len as usize]);
                log::info!(
                    "[diag] window above foreground: hwnd={:?} title={:?}",
                    above_fg,
                    above_title
                );
            }

            let monitor = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor, &mut mi).as_bool() {
                let mon_w = mi.rcMonitor.right - mi.rcMonitor.left;
                let mon_h = mi.rcMonitor.bottom - mi.rcMonitor.top;
                let fg_w = fg_rect.right - fg_rect.left;
                let fg_h = fg_rect.bottom - fg_rect.top;
                let covers_monitor = fg_w >= mon_w && fg_h >= mon_h;
                log::info!(
                    "[diag] monitor: {}x{}, fg covers monitor: {} (likely {})",
                    mon_w,
                    mon_h,
                    covers_monitor,
                    if covers_monitor {
                        "fullscreen/borderless"
                    } else {
                        "windowed"
                    }
                );
            }
        }
    }

    pub fn render(&mut self) {
        if !self.needs_render {
            return;
        }
        self.needs_render = false;

        match self.render_to_swapchain() {
            Ok(()) => {
                log::debug!(
                    "[overlay] rendered {}x{} via DComp",
                    self.width,
                    self.height,
                );
            }
            Err(e) => {
                log::warn!("[overlay] DComp render failed: {}", e);
            }
        }
    }

    fn render_to_swapchain(&self) -> Result<(), Box<dyn std::error::Error>> {
        use windows::Win32::Graphics::Direct2D::Common as D2DCommon;
        use windows::Win32::Graphics::Direct2D::*;
        use windows::Win32::Graphics::Dxgi::Common::*;
        use windows::Win32::Graphics::Dxgi::*;

        unsafe {
            let surface: IDXGISurface = self.swap_chain.GetBuffer(0)?;

            let bmp_props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2DCommon::D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2DCommon::D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: std::mem::ManuallyDrop::new(None),
            };

            let bitmap = self
                .d2d_dc
                .CreateBitmapFromDxgiSurface(&surface, Some(&bmp_props))?;

            self.d2d_dc.SetTarget(&bitmap);

            let rt: &ID2D1RenderTarget = &self.d2d_dc;
            self.renderer
                .render(rt, &self.state, self.width, self.height, self.opacity)?;

            self.d2d_dc.SetTarget(None::<&ID2D1Image>);
            drop(bitmap);
            drop(surface);

            self.swap_chain.Present(1, DXGI_PRESENT(0)).ok()?;
            self.dcomp_device.Commit()?;
        }

        Ok(())
    }

    fn resize_swap_chain(&self) {
        use windows::Win32::Graphics::Dxgi::Common::*;
        use windows::Win32::Graphics::Dxgi::*;

        unsafe {
            self.d2d_dc
                .SetTarget(None::<&windows::Win32::Graphics::Direct2D::ID2D1Image>);

            if let Err(e) = self.swap_chain.ResizeBuffers(
                2,
                self.width,
                self.height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(0),
            ) {
                log::warn!("[overlay] ResizeBuffers failed: {}", e);
            }
        }
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn overlay_wndproc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::WindowsAndMessaging::*;

    match msg {
        WM_NCHITTEST => LRESULT(-1),    // HTTRANSPARENT
        WM_MOUSEACTIVATE => LRESULT(4), // MA_NOACTIVATEANDEAT
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
