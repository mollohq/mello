//! DirectComposition underlay presenter for zero-copy stream rendering.
//!
//! Creates a separate D3D11 device, swap chain, and DComp visual tree that
//! composites the decoded video frame *underneath* the Slint window.  Slint
//! continues to use its default (software/Skia) renderer for the UI, so the
//! GPU context only exists while a stream is being watched.

use windows::core::Interface;
use windows::Win32::Foundation::{HANDLE, HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

pub struct DCompPresenter {
    _device: ID3D11Device,
    device1: ID3D11Device1,
    device_ctx: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    dcomp_device: IDCompositionDevice,
    _dcomp_target: IDCompositionTarget,
    dcomp_visual: IDCompositionVisual,
    stream_width: u32,
    stream_height: u32,
    presented_frames: u64,
}

impl DCompPresenter {
    pub fn new(
        parent_hwnd: isize,
        stream_width: u32,
        stream_height: u32,
        offset_x: f32,
        offset_y: f32,
    ) -> Result<Self, String> {
        let hwnd = HWND(parent_hwnd as *mut _);
        let (device, device_ctx) = Self::create_d3d11_device()?;
        let device1: ID3D11Device1 = device
            .cast()
            .map_err(|e| format!("cast to ID3D11Device1: {e}"))?;
        let swap_chain = Self::create_swap_chain(&device, stream_width, stream_height)?;
        let (dcomp_device, dcomp_target, dcomp_visual) =
            Self::create_dcomp_tree(&device, hwnd, &swap_chain, offset_x, offset_y)?;

        log::info!(
            "DComp presenter initialised: {}x{} at ({:.0},{:.0}) under HWND {:?}",
            stream_width,
            stream_height,
            offset_x,
            offset_y,
            hwnd,
        );

        Ok(Self {
            _device: device,
            device1,
            device_ctx,
            swap_chain,
            dcomp_device,
            _dcomp_target: dcomp_target,
            dcomp_visual,
            stream_width,
            stream_height,
            presented_frames: 0,
        })
    }

    /// Present a shared D3D11 texture handle (NT handle) to the DComp swap chain.
    /// Returns `true` on success.
    pub fn present_shared_texture(
        &mut self,
        shared_handle: usize,
        width: u32,
        height: u32,
    ) -> bool {
        if shared_handle == 0 || width == 0 || height == 0 {
            return false;
        }
        match unsafe { self.present_inner(shared_handle) } {
            Ok(true) => {
                self.presented_frames = self.presented_frames.saturating_add(1);
                true
            }
            Ok(false) => false,
            Err(e) => {
                log::error!("DComp present failed: {}", e);
                false
            }
        }
    }

    /// Update the DComp visual position, scale, and clip to fit the card area.
    /// All coordinates are in physical pixels.
    /// `viewport_y` / `viewport_h` define the scroll container's visible bounds
    /// (physical pixels, window-client coords). The visual is clipped to the
    /// intersection of the canvas rect and the viewport, and hidden when fully
    /// outside.
    pub fn update_geometry(
        &self,
        canvas_x: f32,
        canvas_y: f32,
        canvas_w: f32,
        canvas_h: f32,
        viewport_y: f32,
        viewport_h: f32,
    ) -> bool {
        if self.stream_width == 0 || self.stream_height == 0 {
            return false;
        }

        // Inset by 2px (physical) so the card border stays visible.
        let inset = 2.0_f32;
        let canvas_x = canvas_x + inset;
        let canvas_y = canvas_y + inset;
        let canvas_w = (canvas_w - inset * 2.0).max(1.0);
        let canvas_h = (canvas_h - inset * 2.0).max(1.0);

        // Recompute intersection with the inset rect.
        let vis_top = canvas_y.max(viewport_y);
        let vis_bot = (canvas_y + canvas_h).min(viewport_y + viewport_h);
        let visible = vis_bot > vis_top && canvas_w > 0.0;

        unsafe {
            if !visible {
                let _ = self.dcomp_visual.SetContent(None);
                let _ = self.dcomp_device.Commit();
                return true;
            }

            let _ = self.dcomp_visual.SetContent(&self.swap_chain);

            // Uniform scale preserving aspect ratio (contain-fit).
            let sw = self.stream_width as f32;
            let sh = self.stream_height as f32;
            let scale = (canvas_w / sw).min(canvas_h / sh);
            let rendered_w = sw * scale;
            let rendered_h = sh * scale;

            // Center within the card area.
            let offset_x = canvas_x + (canvas_w - rendered_w) * 0.5;
            let offset_y = canvas_y + (canvas_h - rendered_h) * 0.5;

            if let Err(e) = self.dcomp_visual.SetOffsetX2(offset_x) {
                log::error!("DComp SetOffsetX: {e}");
                return false;
            }
            if let Err(e) = self.dcomp_visual.SetOffsetY2(offset_y) {
                log::error!("DComp SetOffsetY: {e}");
                return false;
            }

            let matrix = windows_numerics::Matrix3x2 {
                M11: scale,
                M12: 0.0,
                M21: 0.0,
                M22: scale,
                M31: 0.0,
                M32: 0.0,
            };
            if let Err(e) = self.dcomp_visual.SetTransform2(&matrix) {
                log::error!("DComp SetTransform: {e}");
                return false;
            }

            // Clip to visible portion (local to the visual, pre-transform).
            let clip_top = (vis_top - offset_y) / scale;
            let clip_bottom = (vis_bot - offset_y) / scale;
            let clip_right = sw;
            let rect_clip = match self.dcomp_device.CreateRectangleClip() {
                Ok(c) => c,
                Err(e) => {
                    log::error!("DComp CreateRectangleClip: {e}");
                    return false;
                }
            };
            let _ = rect_clip.SetLeft2(0.0);
            let _ = rect_clip.SetTop2(clip_top);
            let _ = rect_clip.SetRight2(clip_right);
            let _ = rect_clip.SetBottom2(clip_bottom);
            if let Err(e) = self.dcomp_visual.SetClip(&rect_clip) {
                log::error!("DComp SetClip: {e}");
                return false;
            }

            if let Err(e) = self.dcomp_device.Commit() {
                log::error!("DComp Commit: {e}");
                return false;
            }
        }
        true
    }

    pub fn presented_frames(&self) -> u64 {
        self.presented_frames
    }

    // -- private -----------------------------------------------------------

    fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
        let mut device = None;
        let mut device_ctx = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut device_ctx),
            )
        }
        .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
        Ok((
            device.ok_or("D3D11 device is None")?,
            device_ctx.ok_or("D3D11 device context is None")?,
        ))
    }

    fn create_swap_chain(
        device: &ID3D11Device,
        width: u32,
        height: u32,
    ) -> Result<IDXGISwapChain1, String> {
        let factory: IDXGIFactory2 =
            unsafe { CreateDXGIFactory1() }.map_err(|e| format!("CreateDXGIFactory1: {e}"))?;

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            BufferCount: 2,
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                ..Default::default()
            },
            AlphaMode: windows::Win32::Graphics::Dxgi::Common::DXGI_ALPHA_MODE_IGNORE,
            ..Default::default()
        };

        let swap_chain = unsafe { factory.CreateSwapChainForComposition(device, &desc, None) }
            .map_err(|e| format!("CreateSwapChainForComposition: {e}"))?;

        Ok(swap_chain)
    }

    fn create_dcomp_tree(
        device: &ID3D11Device,
        hwnd: HWND,
        swap_chain: &IDXGISwapChain1,
        offset_x: f32,
        offset_y: f32,
    ) -> Result<
        (
            IDCompositionDevice,
            IDCompositionTarget,
            IDCompositionVisual,
        ),
        String,
    > {
        let dxgi_device: windows::Win32::Graphics::Dxgi::IDXGIDevice = device
            .cast()
            .map_err(|e| format!("cast to IDXGIDevice: {e}"))?;

        let dcomp_device: IDCompositionDevice =
            unsafe { DCompositionCreateDevice(&dxgi_device) }
                .map_err(|e| format!("DCompositionCreateDevice: {e}"))?;

        let target = unsafe { dcomp_device.CreateTargetForHwnd(hwnd, true) }
            .map_err(|e| format!("CreateTargetForHwnd: {e}"))?;

        let visual =
            unsafe { dcomp_device.CreateVisual() }.map_err(|e| format!("CreateVisual: {e}"))?;

        unsafe { visual.SetContent(swap_chain) }.map_err(|e| format!("SetContent: {e}"))?;

        unsafe { visual.SetOffsetX2(offset_x) }.map_err(|e| format!("SetOffsetX: {e}"))?;

        unsafe { visual.SetOffsetY2(offset_y) }.map_err(|e| format!("SetOffsetY: {e}"))?;

        unsafe { target.SetRoot(&visual) }.map_err(|e| format!("SetRoot: {e}"))?;

        unsafe { dcomp_device.Commit() }.map_err(|e| format!("Commit: {e}"))?;

        Ok((dcomp_device, target, visual))
    }

    unsafe fn present_inner(&mut self, shared_handle: usize) -> Result<bool, String> {
        let handle = HANDLE(shared_handle as *mut _);

        let shared_tex: ID3D11Texture2D = self
            .device1
            .OpenSharedResource1(handle)
            .map_err(|e| format!("OpenSharedResource1: {e}"))?;

        // Check if shared texture dimensions differ from swap chain.
        let mut tex_desc = Default::default();
        shared_tex.GetDesc(&mut tex_desc);
        if tex_desc.Width != self.stream_width || tex_desc.Height != self.stream_height {
            log::info!(
                "DComp: stream resolution changed {}x{} → {}x{}, resizing swap chain",
                self.stream_width,
                self.stream_height,
                tex_desc.Width,
                tex_desc.Height
            );
            self.stream_width = tex_desc.Width;
            self.stream_height = tex_desc.Height;
            self.swap_chain
                .ResizeBuffers(
                    2,
                    tex_desc.Width,
                    tex_desc.Height,
                    DXGI_FORMAT_R8G8B8A8_UNORM,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
                .map_err(|e| format!("ResizeBuffers: {e}"))?;
        }

        let back_tex: ID3D11Texture2D = self
            .swap_chain
            .GetBuffer(0)
            .map_err(|e| format!("GetBuffer: {e}"))?;

        self.device_ctx.CopyResource(&back_tex, &shared_tex);

        self.swap_chain
            .Present(0, DXGI_PRESENT(0))
            .ok()
            .map_err(|e| format!("Present: {e}"))?;

        Ok(true)
    }
}

impl Drop for DCompPresenter {
    fn drop(&mut self) {
        log::info!(
            "DComp presenter destroyed (presented {} frames)",
            self.presented_frames
        );
    }
}
