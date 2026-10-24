//! DirectComposition underlay presenter for zero-copy stream rendering.
//!
//! Creates a separate D3D11 device, swap chain, and DComp visual tree that
//! composites the decoded video frame *underneath* the Slint window.  Slint
//! continues to use its default (software/Skia) renderer for the UI, so the
//! GPU context only exists while a stream is being watched.
//!
//! Streaming-quality stage 3: two halves on two threads.
//! - [`VideoSwapChain`] presents. libmello's present thread calls it (through
//!   `mello_core::set_native_presenter`) at each frame's render time. It waits
//!   on the swap chain's frame-latency waitable object (at most one frame in
//!   the queue, so a present goes to the next vblank), reads libmello's shared
//!   RGBA texture under its keyed mutex (key 0), copies it into the back
//!   buffer and presents.
//! - [`DCompPresenter`] stays on the UI thread: the visual tree, the geometry
//!   from Slint, the pause card and the host's pointer. Slint presents nothing.
//!
//! The device is multithread-protected: the cursor upload (UI thread) and the
//! frame copy (present thread) share its immediate context.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIFactory2, IDXGIKeyedMutex,
    IDXGISwapChain1, IDXGISwapChain2, DXGI_PRESENT, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::Threading::WaitForSingleObjectEx;

/// Longest wait for the swap chain to take a frame: a little over one vblank
/// at 30 Hz. A missed wait presents anyway.
const FRAME_LATENCY_WAIT_MS: u32 = 40;
/// Longest wait for libmello to give the shared frame back. libmello holds it
/// only for one copy.
const KEYED_MUTEX_WAIT_MS: u32 = 50;

fn pack_size(width: u32, height: u32) -> u64 {
    (u64::from(width) << 32) | u64::from(height)
}

fn unpack_size(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

/// The presenting half: runs on libmello's present thread.
pub struct VideoSwapChain {
    device1: ID3D11Device1,
    device_ctx: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    /// Signalled when the swap chain can take the next frame.
    waitable: HANDLE,
    /// The swap chain size, read by the UI thread for the geometry.
    size: Arc<AtomicU64>,
    /// The opened shared texture: libmello makes a new one (a new handle) when
    /// the picture size changes, so the key is handle and size.
    cached: Option<(usize, u32, u32, ID3D11Texture2D, IDXGIKeyedMutex)>,
    presented_frames: u64,
    // A present failure repeats at the display rate. Logging every one buried
    // an hour of client log under 60 identical lines a second and hid the
    // events around it, so the same message is logged once and then counted.
    last_present_error: String,
    repeated_present_errors: u64,
}

// SAFETY: the COM objects are used from one thread at a time (the Mutex in
// DCompPresenter), and the D3D11 device is multithread-protected for the
// immediate context the UI thread also uses (the cursor upload).
unsafe impl Send for VideoSwapChain {}

impl VideoSwapChain {
    /// Present one frame of libmello's shared RGBA texture. Returns `true`
    /// when it went to the swap chain.
    pub fn present(&mut self, frame: &mello_core::NativeSurfaceFrame) -> bool {
        if frame.shared_handle == 0 || frame.width == 0 || frame.height == 0 {
            return false;
        }
        match unsafe { self.present_inner(frame) } {
            Ok(()) => {
                self.presented_frames = self.presented_frames.saturating_add(1);
                if !self.last_present_error.is_empty() {
                    log::info!(
                        "DComp present recovered after {} failed frames",
                        self.repeated_present_errors.saturating_add(1)
                    );
                    self.last_present_error.clear();
                    self.repeated_present_errors = 0;
                }
                true
            }
            Err(e) => {
                if self.last_present_error == e {
                    self.repeated_present_errors = self.repeated_present_errors.saturating_add(1);
                    // One line per 300 frames, about five seconds, so a stuck
                    // presenter stays visible without drowning the log.
                    if self.repeated_present_errors.is_multiple_of(300) {
                        log::error!(
                            "DComp present still failing after {} frames: {}",
                            self.repeated_present_errors,
                            e
                        );
                    }
                } else {
                    log::error!("DComp present failed: {}", e);
                    self.last_present_error = e;
                    self.repeated_present_errors = 0;
                }
                false
            }
        }
    }

    pub fn presented_frames(&self) -> u64 {
        self.presented_frames
    }

    unsafe fn present_inner(
        &mut self,
        frame: &mello_core::NativeSurfaceFrame,
    ) -> Result<(), String> {
        let key = (frame.shared_handle, frame.width, frame.height);
        let fresh = !matches!(&self.cached, Some((h, w, hh, _, _)) if (*h, *w, *hh) == key);
        if fresh {
            self.cached = None;
            let handle = HANDLE(frame.shared_handle as *mut _);
            let opened: ID3D11Texture2D = self
                .device1
                .OpenSharedResource1(handle)
                .map_err(|e| format!("OpenSharedResource1: {e}"))?;
            let mutex: IDXGIKeyedMutex = opened
                .cast()
                .map_err(|e| format!("shared frame without a keyed mutex: {e}"))?;
            self.cached = Some((key.0, key.1, key.2, opened, mutex));
        }
        let (_, _, _, shared_tex, mutex) = self.cached.as_ref().expect("cached above");
        let (shared_tex, mutex) = (shared_tex.clone(), mutex.clone());

        let mut tex_desc = Default::default();
        shared_tex.GetDesc(&mut tex_desc);
        let (sw, sh) = unpack_size(self.size.load(Ordering::Acquire));
        if tex_desc.Width != sw || tex_desc.Height != sh {
            log::info!(
                "DComp: stream resolution changed {}x{} → {}x{}, resizing swap chain",
                sw,
                sh,
                tex_desc.Width,
                tex_desc.Height
            );
            self.swap_chain
                .ResizeBuffers(
                    2,
                    tex_desc.Width,
                    tex_desc.Height,
                    DXGI_FORMAT_R8G8B8A8_UNORM,
                    DXGI_SWAP_CHAIN_FLAG(DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0),
                )
                .map_err(|e| format!("ResizeBuffers: {e}"))?;
            self.size.store(
                pack_size(tex_desc.Width, tex_desc.Height),
                Ordering::Release,
            );
        }

        // The vsync gate: the swap chain holds at most one frame, so this
        // returns when the last one is on its way to the screen.
        if !self.waitable.is_invalid() {
            let _ = WaitForSingleObjectEx(self.waitable, FRAME_LATENCY_WAIT_MS, true);
        }

        let back_tex: ID3D11Texture2D = self
            .swap_chain
            .GetBuffer(0)
            .map_err(|e| format!("GetBuffer: {e}"))?;

        // Key 0 on both sides: libmello writes the frame only while it holds
        // the mutex, so this copy never reads half a frame.
        mutex
            .AcquireSync(0, KEYED_MUTEX_WAIT_MS)
            .map_err(|e| format!("AcquireSync: {e}"))?;
        self.device_ctx.CopyResource(&back_tex, &shared_tex);
        let _ = mutex.ReleaseSync(0);

        self.swap_chain
            .Present(1, DXGI_PRESENT(0))
            .ok()
            .map_err(|e| format!("Present: {e}"))?;
        Ok(())
    }
}

impl Drop for VideoSwapChain {
    fn drop(&mut self) {
        self.cached = None;
        if !self.waitable.is_invalid() {
            // SAFETY: the handle came from GetFrameLatencyWaitableObject and
            // nothing else closes it.
            unsafe {
                let _ = CloseHandle(self.waitable);
            }
        }
        log::info!(
            "DComp swap chain destroyed (presented {} frames)",
            self.presented_frames
        );
    }
}

pub struct DCompPresenter {
    _device: ID3D11Device,
    device_ctx: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    dcomp_device: IDCompositionDevice,
    _dcomp_target: IDCompositionTarget,
    dcomp_visual: IDCompositionVisual,
    /// The presenting half, shared with libmello's present thread.
    video: Arc<Mutex<VideoSwapChain>>,
    /// The swap chain size (the stream size); the present thread changes it.
    stream_size: Arc<AtomicU64>,
    /// The size the geometry was last computed for, and the last geometry.
    geometry_size: std::cell::Cell<u64>,
    last_geometry: std::cell::Cell<Option<[f32; 6]>>,
    /// The host's pointer, drawn over the video from cursor packets (the
    /// game capture hook's frames have none; `mello_core::stream::cursor`).
    /// A child of the video visual, so the video's scale and clip apply and
    /// its offset is in stream pixels.
    cursor_visual: IDCompositionVisual,
    cursor_swap: Option<IDXGISwapChain1>,
    cursor_swap_size: (u32, u32),
    cursor_shape: Option<std::sync::Arc<mello_core::stream::cursor::CursorShape>>,
    cursor_serial: u64,
    cursor_shown: bool,
    /// Set by `set_content_visible` (the pause card), which takes `&self`.
    content_visible: std::cell::Cell<bool>,
}

impl DCompPresenter {
    /// `adapter_luid` must be the adapter libmello decodes on
    /// (`mello_core::video_adapter_luid()`), because this presenter opens the
    /// shared textures that libmello creates there.
    pub fn new(
        parent_hwnd: isize,
        stream_width: u32,
        stream_height: u32,
        offset_x: f32,
        offset_y: f32,
        adapter_luid: u64,
    ) -> Result<Self, String> {
        let hwnd = HWND(parent_hwnd as *mut _);
        let (device, device_ctx) = Self::create_d3d11_device(adapter_luid)?;
        let device1: ID3D11Device1 = device
            .cast()
            .map_err(|e| format!("cast to ID3D11Device1: {e}"))?;
        let swap_chain = Self::create_swap_chain(&device, stream_width, stream_height)?;
        let waitable = match swap_chain.cast::<IDXGISwapChain2>() {
            Ok(chain2) => unsafe {
                let _ = chain2.SetMaximumFrameLatency(1);
                chain2.GetFrameLatencyWaitableObject()
            },
            Err(e) => {
                log::warn!("DComp: no IDXGISwapChain2 ({e}); presents are not vsync-gated");
                HANDLE::default()
            }
        };
        if !waitable.is_invalid() {
            // The waitable object starts signalled; consume it like the
            // first frame would.
            let _ = unsafe { WaitForSingleObjectEx(waitable, 0, true) };
        }
        let stream_size = Arc::new(AtomicU64::new(pack_size(stream_width, stream_height)));
        let (dcomp_device, dcomp_target, dcomp_visual) =
            Self::create_dcomp_tree(&device, hwnd, &swap_chain, offset_x, offset_y)?;
        let cursor_visual = unsafe { dcomp_device.CreateVisual() }
            .map_err(|e| format!("CreateVisual (cursor): {e}"))?;
        unsafe { dcomp_visual.AddVisual(&cursor_visual, true, None::<&IDCompositionVisual>) }
            .map_err(|e| format!("AddVisual (cursor): {e}"))?;

        log::info!(
            "DComp presenter initialised: {}x{} at ({:.0},{:.0}) under HWND {:?}",
            stream_width,
            stream_height,
            offset_x,
            offset_y,
            hwnd,
        );

        let video = Arc::new(Mutex::new(VideoSwapChain {
            device1,
            device_ctx: device_ctx.clone(),
            swap_chain: swap_chain.clone(),
            waitable,
            size: Arc::clone(&stream_size),
            cached: None,
            presented_frames: 0,
            last_present_error: String::new(),
            repeated_present_errors: 0,
        }));

        Ok(Self {
            _device: device,
            device_ctx,
            swap_chain,
            dcomp_device,
            _dcomp_target: dcomp_target,
            dcomp_visual,
            video,
            geometry_size: std::cell::Cell::new(pack_size(stream_width, stream_height)),
            stream_size,
            last_geometry: std::cell::Cell::new(None),
            cursor_visual,
            cursor_swap: None,
            cursor_swap_size: (0, 0),
            cursor_shape: None,
            cursor_serial: 0,
            cursor_shown: false,
            content_visible: std::cell::Cell::new(true),
        })
    }

    /// The native frame presenter for `mello_core::set_native_presenter`:
    /// libmello's present thread calls it at each frame's render time.
    pub fn frame_presenter(&self) -> mello_core::NativePresentFn {
        let video = Arc::clone(&self.video);
        Box::new(move |frame| match video.lock() {
            Ok(mut chain) => chain.present(frame),
            Err(_) => false,
        })
    }

    /// Compute the geometry again when the present thread resized the swap
    /// chain (a new stream size). The UI thread calls it on its timer; cheap
    /// when nothing changed.
    pub fn refresh_geometry_if_resized(&self) {
        let size = self.stream_size.load(Ordering::Acquire);
        if size == self.geometry_size.get() {
            return;
        }
        if let Some([x, y, w, h, vy, vh]) = self.last_geometry.get() {
            self.update_geometry(x, y, w, h, vy, vh);
        } else {
            self.geometry_size.set(size);
        }
    }

    /// Show or hide the video content without destroying the presenter.
    /// Pause UX calls this: hiding reveals the Slint pause card underneath
    /// (the DComp layer composites above Slint content, so a Slint overlay
    /// alone would hide behind the frozen frame). Presenting resumes into
    /// the same swap chain — no re-init, no geometry replay.
    pub fn set_content_visible(&self, visible: bool) {
        self.content_visible.set(visible);
        unsafe {
            if visible {
                let _ = self.dcomp_visual.SetContent(&self.swap_chain);
            } else {
                let _ = self.dcomp_visual.SetContent(None);
            }
            // The pointer is a child visual: it stays on screen unless it
            // goes with the video.
            self.apply_cursor_content(visible);
            let _ = self.dcomp_device.Commit();
        }
    }

    /// Draw the host's pointer over the video, or hide it. Cheap when nothing
    /// changed (`serial`); a move only sets the visual offset; a new shape is
    /// uploaded into a small premultiplied swap chain once.
    pub fn update_cursor(&mut self, cursor: &mello_core::stream::cursor::ViewerCursor) {
        if cursor.serial == self.cursor_serial {
            return;
        }
        self.cursor_serial = cursor.serial;
        let shape = match (&cursor.shape, cursor.visible) {
            (Some(shape), true) if shape.width > 0 && shape.height > 0 => shape.clone(),
            _ => {
                if self.cursor_shown {
                    self.cursor_shown = false;
                    unsafe {
                        let _ = self.cursor_visual.SetContent(None);
                        let _ = self.dcomp_device.Commit();
                    }
                }
                return;
            }
        };
        let new_shape = !self
            .cursor_shape
            .as_ref()
            .is_some_and(|current| std::sync::Arc::ptr_eq(current, &shape));
        if new_shape {
            if let Err(e) = unsafe { self.upload_cursor(&shape) } {
                log::warn!("DComp cursor upload failed: {e}");
                return;
            }
            self.cursor_shape = Some(shape);
        }
        unsafe {
            let _ = self.cursor_visual.SetOffsetX2(cursor.x as f32);
            let _ = self.cursor_visual.SetOffsetY2(cursor.y as f32);
            self.cursor_shown = true;
            self.apply_cursor_content(self.content_visible.get());
            let _ = self.dcomp_device.Commit();
        }
    }

    unsafe fn apply_cursor_content(&self, video_visible: bool) {
        match (&self.cursor_swap, self.cursor_shown && video_visible) {
            (Some(swap), true) => {
                let _ = self.cursor_visual.SetContent(swap);
            }
            _ => {
                let _ = self.cursor_visual.SetContent(None);
            }
        }
    }

    unsafe fn upload_cursor(
        &mut self,
        shape: &mello_core::stream::cursor::CursorShape,
    ) -> Result<(), String> {
        let (w, h) = (u32::from(shape.width), u32::from(shape.height));
        let expected = w as usize * h as usize * 4;
        if shape.rgba.len() < expected {
            return Err("short shape".into());
        }
        if self.cursor_swap.is_none() || self.cursor_swap_size != (w, h) {
            let factory: IDXGIFactory2 =
                CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w,
                Height: h,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                BufferCount: 2,
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    ..Default::default()
                },
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            let swap = factory
                .CreateSwapChainForComposition(&self._device, &desc, None)
                .map_err(|e| format!("CreateSwapChainForComposition (cursor): {e}"))?;
            self.cursor_swap = Some(swap);
            self.cursor_swap_size = (w, h);
        }
        // RGBA straight alpha in, BGRA premultiplied out: what a composition
        // swap chain with premultiplied alpha takes.
        let mut bgra = vec![0u8; expected];
        for (out, px) in bgra.chunks_exact_mut(4).zip(shape.rgba.chunks_exact(4)) {
            let a = u16::from(px[3]);
            out[0] = ((u16::from(px[2]) * a + 127) / 255) as u8;
            out[1] = ((u16::from(px[1]) * a + 127) / 255) as u8;
            out[2] = ((u16::from(px[0]) * a + 127) / 255) as u8;
            out[3] = px[3];
        }
        let swap = self.cursor_swap.as_ref().ok_or("no cursor swap chain")?;
        let back: ID3D11Texture2D = swap
            .GetBuffer(0)
            .map_err(|e| format!("GetBuffer (cursor): {e}"))?;
        self.device_ctx
            .UpdateSubresource(&back, 0, None, bgra.as_ptr().cast(), w * 4, 0);
        swap.Present(0, DXGI_PRESENT(0))
            .ok()
            .map_err(|e| format!("Present (cursor): {e}"))?;
        Ok(())
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
        self.last_geometry.set(Some([
            canvas_x, canvas_y, canvas_w, canvas_h, viewport_y, viewport_h,
        ]));
        let packed = self.stream_size.load(Ordering::Acquire);
        self.geometry_size.set(packed);
        let (stream_width, stream_height) = unpack_size(packed);
        if stream_width == 0 || stream_height == 0 {
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
                self.apply_cursor_content(false);
                let _ = self.dcomp_device.Commit();
                return true;
            }

            let _ = self.dcomp_visual.SetContent(&self.swap_chain);
            self.apply_cursor_content(self.content_visible.get());

            // Uniform scale preserving aspect ratio (contain-fit).
            let sw = stream_width as f32;
            let sh = stream_height as f32;
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
        self.video.lock().map(|v| v.presented_frames()).unwrap_or(0)
    }

    // -- private -----------------------------------------------------------

    /// The adapter whose LUID matches `luid`, or `None` to let D3D11 pick the
    /// system default.
    ///
    /// The presenter opens shared textures that libmello created, and a shared
    /// handle is only valid on the adapter that made it. Passing `None` to
    /// `D3D11CreateDevice` takes the *first* enumerated adapter, which on a
    /// laptop is the integrated GPU, while libmello decodes on the discrete one.
    /// Every `OpenSharedResource1` then fails with `E_INVALIDARG` and the viewer
    /// shows black while the decoder reports healthy frames. Measured on a
    /// laptop with AMD integrated plus an RTX 3050 Ti on 2026-09-18: 524 frames
    /// decoded, 0 presented.
    fn find_adapter(luid: u64) -> Option<IDXGIAdapter> {
        if luid == 0 {
            log::warn!(
                "no video adapter LUID from libmello; the presenter falls back to the default \
                 adapter, which is wrong on a machine with two GPUs"
            );
            return None;
        }
        let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
            Ok(f) => f,
            Err(e) => {
                log::warn!("CreateDXGIFactory1 while matching adapter LUID: {e}");
                return None;
            }
        };
        for index in 0.. {
            let adapter: IDXGIAdapter = match unsafe { factory.EnumAdapters(index) } {
                Ok(a) => a,
                Err(_) => break,
            };
            let Ok(desc) = (unsafe { adapter.GetDesc() }) else {
                continue;
            };
            let found =
                ((desc.AdapterLuid.HighPart as u32 as u64) << 32) | desc.AdapterLuid.LowPart as u64;
            if found == luid {
                let name = String::from_utf16_lossy(&desc.Description)
                    .trim_end_matches('\0')
                    .to_string();
                log::info!("DComp presenter uses adapter \"{name}\" (luid 0x{luid:016X})");
                return Some(adapter);
            }
        }
        log::warn!(
            "no adapter with luid 0x{luid:016X}; the presenter falls back to the default adapter \
             and shared frames will not open"
        );
        None
    }

    fn create_d3d11_device(
        adapter_luid: u64,
    ) -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
        let adapter = Self::find_adapter(adapter_luid);
        // D3D_DRIVER_TYPE must be UNKNOWN when an adapter is given, and
        // HARDWARE when it is not; D3D11CreateDevice rejects any other pairing.
        let driver_type = if adapter.is_some() {
            D3D_DRIVER_TYPE_UNKNOWN
        } else {
            D3D_DRIVER_TYPE_HARDWARE
        };
        let mut device = None;
        let mut device_ctx = None;
        unsafe {
            D3D11CreateDevice(
                adapter.as_ref(),
                driver_type,
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
        let device = device.ok_or("D3D11 device is None")?;
        // Two threads use the immediate context: the present thread (frame
        // copy) and the UI thread (pointer upload).
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = mt.SetMultithreadProtected(true);
            }
        }
        Ok((device, device_ctx.ok_or("D3D11 device context is None")?))
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
            // The present thread waits on it before each frame (vsync gate).
            Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
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
}

impl Drop for DCompPresenter {
    fn drop(&mut self) {
        // libmello's present thread must not reach this swap chain any more.
        // The caller clears the presenter first; this is the backstop.
        mello_core::set_native_presenter(None);
        log::info!(
            "DComp presenter destroyed (presented {} frames)",
            self.presented_frames()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The presenter must land on the adapter libmello decodes on, not on
    /// whichever adapter Windows enumerates first. Passing `None` to
    /// `D3D11CreateDevice` takes adapter 0, and on a machine where that is not
    /// libmello's adapter every shared texture fails to open, which the viewer
    /// shows as a black stream.
    #[test]
    fn adapter_lookup_returns_the_adapter_libmello_decodes_on() {
        let luid = mello_core::video_adapter_luid();
        if luid == 0 {
            eprintln!("skipped: no usable GPU adapter on this machine");
            return;
        }

        let adapter = DCompPresenter::find_adapter(luid).expect("adapter with libmello's LUID");
        let desc = unsafe { adapter.GetDesc() }.expect("adapter description");
        let found =
            ((desc.AdapterLuid.HighPart as u32 as u64) << 32) | desc.AdapterLuid.LowPart as u64;
        assert_eq!(found, luid);
    }

    /// Without a LUID there is nothing to match, so the caller falls back to
    /// the default adapter rather than guessing.
    #[test]
    fn adapter_lookup_without_a_luid_falls_back() {
        assert!(DCompPresenter::find_adapter(0).is_none());
    }
}
