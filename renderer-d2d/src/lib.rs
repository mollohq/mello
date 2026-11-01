//! A Slint renderer on Direct2D (Windows). Spec: `specs/features/D2D-RENDERER.md`.
//!
//! Wiring: the vendored winit backend (`third_party/i-slint-backend-winit`,
//! `renderer/d2d.rs`) creates the window, hands over the HWND with
//! [`D2DRenderer::set_window`] and calls [`D2DRenderer::render`] per frame.
//!
//! One D3D11 and Direct2D device per thread (`device.rs`), one swapchain and
//! device context per window (`surface.rs`). A removed device drops the
//! surface, the shared device and every GPU object; the next frame recreates
//! them from the stored window handle.

#![cfg(target_os = "windows")]

use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::rc::{Rc, Weak};

use i_slint_core::graphics::{Rgba8Pixel, SharedPixelBuffer};
use i_slint_core::item_rendering::ItemRenderer;
use i_slint_core::item_tree::ItemTreeWeak;
use i_slint_core::lengths::{LogicalRect, ScaleFactor};
use i_slint_core::partial_renderer::{DirtyRegion, PartialRenderingState};
use i_slint_core::platform::PlatformError;
use i_slint_core::renderer::{DrawOutcome, RendererSealed};
use i_slint_core::textlayout::sharedparley;
use i_slint_core::window::{WindowAdapter, WindowInner};
use i_slint_core::Brush;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;

mod device;
mod fonts;
mod images;
mod itemrenderer;
mod stats;
mod surface;
mod text;

use device::SharedDevice;
use images::BitmapCache;
use itemrenderer::{BrushCache, D2DItemRenderer, ItemBitmapCache, PathCache, ShadowCache};
use surface::Surface;

/// Creates the shared D3D11 and Direct2D device once, or says why it cannot be
/// made. The client calls this before it picks the renderer: a failure means the
/// CPU renderer. The device stays cached for the renderer, so nothing is wasted.
pub fn probe() -> Result<(), String> {
    SharedDevice::get().map(|_| ()).map_err(|e| e.to_string())
}

struct WindowHandle {
    hwnd: isize,
    width: u32,
    height: u32,
}

pub struct D2DRenderer {
    window_adapter: RefCell<Option<Weak<dyn WindowAdapter>>>,
    /// The window to draw on; `None` while suspended.
    handle: RefCell<Option<WindowHandle>>,
    surface: RefCell<Option<Surface>>,
    item_cache: ItemBitmapCache,
    bitmap_cache: RefCell<BitmapCache>,
    shadow_cache: ShadowCache,
    path_cache: PathCache,
    brush_cache: RefCell<BrushCache>,
    text_layout_cache: sharedparley::TextLayoutCache,
    pre_present: RefCell<Option<Box<dyn FnMut()>>>,
    /// Set by `take_snapshot`; the next frame reads itself back here.
    snapshot: RefCell<Option<Vec<u8>>>,
    want_snapshot: Cell<bool>,
    /// `FX_D2D_STATS=1`: frame times to stderr, one line per second.
    stats: RefCell<Option<stats::FrameStats>>,
    /// When the last frame was drawn, for the idle trim.
    last_frame: Rc<Cell<Option<std::time::Instant>>>,
    /// Once a second while the window is up: after 1.5 s without a frame, Direct2D
    /// drops its cached intermediates (layers, effects, glyph atlases). They come
    /// back on the next frame; idle memory is the point (memory rule 2).
    trim_timer: i_slint_core::timers::Timer,
    /// `FX_D2D_FAKE_DEVICE_LOSS=N`: every N frames, take the lost-device path as if
    /// `Present` returned `DXGI_ERROR_DEVICE_REMOVED`. For the recovery and leak runs:
    /// Windows has no API that removes a D3D11 device on request.
    fake_device_loss: Option<u64>,
    frames: Cell<u64>,
    /// Slint's partial renderer: only the items that changed, and what they
    /// covered, are drawn. `None` with `FX_D2D_FULL_REDRAW=1`.
    partial: Option<PartialRenderingState>,
    /// The dirty region of each recent frame, newest first: the back buffer is
    /// two presents old, so the previous frame's region is drawn again as well.
    dirty_history: RefCell<[DirtyRegion; 3]>,
}

const IDLE_TRIM_AFTER: std::time::Duration = std::time::Duration::from_millis(1500);

impl Default for D2DRenderer {
    fn default() -> Self {
        Self {
            window_adapter: Default::default(),
            handle: RefCell::new(None),
            surface: RefCell::new(None),
            item_cache: Default::default(),
            bitmap_cache: Default::default(),
            shadow_cache: Default::default(),
            path_cache: Default::default(),
            brush_cache: Default::default(),
            text_layout_cache: Default::default(),
            pre_present: RefCell::new(None),
            snapshot: RefCell::new(None),
            want_snapshot: Cell::new(false),
            stats: RefCell::new(stats::FrameStats::from_env()),
            last_frame: Rc::new(Cell::new(None)),
            trim_timer: Default::default(),
            fake_device_loss: std::env::var("FX_D2D_FAKE_DEVICE_LOSS")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|n| *n > 0),
            frames: Cell::new(0),
            partial: std::env::var_os("FX_D2D_FULL_REDRAW")
                .is_none()
                .then(PartialRenderingState::default),
            dirty_history: Default::default(),
        }
    }
}

impl D2DRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates the swapchain and Direct2D context on `hwnd` (the device is shared).
    pub fn set_window(
        &self,
        hwnd: isize,
        width: u32,
        height: u32,
        pre_present: Box<dyn FnMut()>,
    ) -> Result<(), PlatformError> {
        *self.handle.borrow_mut() = Some(WindowHandle {
            hwnd,
            width,
            height,
        });
        *self.surface.borrow_mut() = Some(Surface::new(hwnd, width, height)?);
        *self.pre_present.borrow_mut() = Some(pre_present);
        let last_frame = self.last_frame.clone();
        let trimmed = Cell::new(true);
        self.trim_timer.start(
            i_slint_core::timers::TimerMode::Repeated,
            std::time::Duration::from_secs(1),
            move || {
                let Some(last) = last_frame.get() else { return };
                let idle = last.elapsed() >= IDLE_TRIM_AFTER;
                if idle && !trimmed.get() {
                    if let Some(device) = SharedDevice::try_get() {
                        device.trim(IDLE_TRIM_AFTER.as_millis() as u32);
                        if std::env::var_os("FX_D2D_DEBUG").is_some() {
                            eprintln!("d2d renderer: idle, Direct2D resources trimmed");
                        }
                    }
                    trimmed.set(true);
                } else if !idle {
                    trimmed.set(false);
                }
            },
        );
        Ok(())
    }

    /// Drops the swapchain, the context and every bitmap. Font faces stay
    /// (DirectWrite, not the device).
    pub fn suspend(&self) -> Result<(), PlatformError> {
        self.trim_timer.stop();
        self.clear_gpu_caches();
        *self.surface.borrow_mut() = None;
        *self.handle.borrow_mut() = None;
        *self.pre_present.borrow_mut() = None;
        self.last_frame.set(None);
        SharedDevice::release_if_unused();
        Ok(())
    }

    fn clear_gpu_caches(&self) {
        self.item_cache.clear_all();
        self.bitmap_cache.borrow_mut().clear();
        self.shadow_cache.clear();
        // Geometries belong to the Direct2D factory, which a lost device replaces.
        self.path_cache.clear_all();
        self.brush_cache.borrow_mut().clear();
    }

    /// The device is gone: forget everything made from it. The next frame recreates
    /// the surface on the stored window.
    fn lose_device(&self, what: &str, e: &windows::core::Error) {
        eprintln!("d2d renderer: device lost in {what} ({e}); recreating");
        self.clear_gpu_caches();
        *self.surface.borrow_mut() = None;
        SharedDevice::reset();
        // The swapchain is gone, so the window shows nothing until the next
        // frame. Ask for that frame now instead of waiting for a change.
        if let Some(window_adapter) = self.window_adapter() {
            window_adapter.request_redraw();
        }
    }

    fn window_adapter_rc(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        self.window_adapter
            .borrow()
            .as_ref()
            .and_then(|w| w.upgrade())
            .ok_or_else(|| "d2d renderer: no window adapter".into())
    }

    pub fn render(&self) -> Result<DrawOutcome, PlatformError> {
        let window_adapter = self.window_adapter_rc()?;
        let window = window_adapter.window();
        let size = window.size();
        if size.width == 0 || size.height == 0 {
            return Ok(DrawOutcome::Success);
        }
        let mut surface_ref = self.surface.borrow_mut();
        if surface_ref.is_none() {
            // After a lost device, or a resume without a surface.
            let Some(handle) = self
                .handle
                .borrow()
                .as_ref()
                .map(|h| (h.hwnd, h.width, h.height))
            else {
                return Ok(DrawOutcome::Success);
            };
            let started = std::time::Instant::now();
            *surface_ref = Some(Surface::new(handle.0, handle.1, handle.2)?);
            if std::env::var_os("FX_D2D_DEBUG").is_some() {
                eprintln!(
                    "d2d renderer: surface recreated in {:.1} ms",
                    started.elapsed().as_secs_f64() * 1000.0
                );
            }
        }
        let surface = surface_ref.as_mut().unwrap();
        surface.ensure_size(size.width, size.height)?;

        let frame_start = std::time::Instant::now();
        let window_inner = WindowInner::from_pub(window);
        self.brush_cache.borrow_mut().begin_frame();
        self.item_cache.clear_cache_if_scale_factor_changed(window);
        self.path_cache.clear_cache_if_scale_factor_changed(window);
        self.shadow_cache
            .clear_cache_if_scale_factor_changed(window);
        let target = surface.begin_frame()?;
        let dc = surface.dc.clone();
        let device = surface.device.clone();

        let logical_size = i_slint_core::lengths::logical_size_from_api(
            window.size().to_logical(window_inner.scale_factor()),
        );
        let buffer_age = surface.buffer_age();
        let mut present_rect: Option<RECT> = None;

        let categories = window_inner.draw_contents(|components, post_render| {
            let background = window_inner
                .window_item()
                .map(|w| w.as_pin_ref().background());
            let clear = match &background {
                Some(Brush::SolidColor(c)) => D2D1_COLOR_F {
                    r: c.red() as f32 / 255.0,
                    g: c.green() as f32 / 255.0,
                    b: c.blue() as f32 / 255.0,
                    a: c.alpha() as f32 / 255.0,
                },
                _ => D2D1_COLOR_F {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.0,
                },
            };
            // The clear respects the frame clip: only the dirty area is cleared.
            let draw = |item_renderer: &mut dyn ItemRenderer| {
                unsafe { dc.Clear(Some(&clear)) };
                if let Some(window_item_rc) = window_inner.window_item_rc() {
                    let window_item = window_item_rc
                        .downcast::<i_slint_core::items::WindowItem>()
                        .unwrap();
                    if !matches!(window_item.as_pin_ref().background(), Brush::SolidColor(..)) {
                        // A gradient background: drawn as a rectangle over the transparent clear.
                        item_renderer.draw_rectangle(
                            window_item.as_pin_ref(),
                            &window_item_rc,
                            logical_size,
                            &window_item.as_pin_ref().cached_rendering_data,
                        );
                    }
                }
                for (component, origin) in components {
                    if let Some(component) = ItemTreeWeak::upgrade(component) {
                        i_slint_core::item_rendering::render_component_items(
                            &component,
                            item_renderer,
                            *origin,
                            &window_adapter,
                        );
                    }
                }
                post_render(item_renderer);
            };

            let item_renderer = D2DItemRenderer::new(
                &dc,
                &device,
                &self.item_cache,
                &self.bitmap_cache,
                &self.shadow_cache,
                &self.path_cache,
                &self.brush_cache,
                &self.text_layout_cache,
                window,
                size.width,
                size.height,
            );

            let mut item_renderer = match &self.partial {
                Some(partial) => {
                    let mut partial_renderer = partial.create_partial_renderer(item_renderer);
                    let whole: DirtyRegion = LogicalRect::from_size(logical_size).into();
                    let buffer_region = {
                        let history = self.dirty_history.borrow();
                        if buffer_age > 0 && (buffer_age as usize) <= history.len() {
                            history[..buffer_age as usize - 1]
                                .iter()
                                .fold(DirtyRegion::default(), |acc, r| acc.union(r))
                        } else {
                            whole
                        }
                    };
                    let this_frame = partial.apply_dirty_region(
                        &mut partial_renderer,
                        components,
                        logical_size,
                        Some(buffer_region),
                    );
                    {
                        let mut history = self.dirty_history.borrow_mut();
                        history.rotate_right(1);
                        history[0] = this_frame;
                    }
                    // The region to draw, rounded out to whole pixels.
                    let sf = window_inner.scale_factor();
                    let b = partial_renderer.dirty_region.bounding_rect();
                    let (w, h) = (size.width as f32, size.height as f32);
                    let x0 = (b.min_x() * sf).floor().clamp(0.0, w);
                    let y0 = (b.min_y() * sf).floor().clamp(0.0, h);
                    let x1 = (b.max_x() * sf).ceil().clamp(0.0, w);
                    let y1 = (b.max_y() * sf).ceil().clamp(0.0, h);
                    partial_renderer.actual_renderer.clip_frame(x0, y0, x1, y1);
                    // The clip is one box, so every item in the box must be drawn:
                    // the region can hold up to three rectangles, and an item
                    // between them would be cleared and not drawn again.
                    partial_renderer.dirty_region = LogicalRect::new(
                        i_slint_core::lengths::LogicalPoint::new(x0 / sf, y0 / sf),
                        i_slint_core::lengths::LogicalSize::new((x1 - x0) / sf, (y1 - y0) / sf),
                    )
                    .into();
                    let all = x0 <= 0.0 && y0 <= 0.0 && x1 >= w && y1 >= h;
                    if !all && x1 > x0 && y1 > y0 {
                        present_rect = Some(RECT {
                            left: x0 as i32,
                            top: y0 as i32,
                            right: x1 as i32,
                            bottom: y1 as i32,
                        });
                    }
                    draw(&mut partial_renderer);
                    partial_renderer.into_inner()
                }
                None => {
                    let mut item_renderer = item_renderer;
                    draw(&mut item_renderer);
                    item_renderer
                }
            };
            item_renderer.finish();
            item_renderer.categories()
        });

        let mut read_back = if self.want_snapshot.take() {
            Some(Vec::new())
        } else {
            None
        };
        let ended = surface.end_frame(
            target,
            &mut self.pre_present.borrow_mut(),
            read_back.as_mut(),
            present_rect,
        );
        if let Some(pixels) = read_back {
            *self.snapshot.borrow_mut() = Some(pixels);
        }
        let frames = self.frames.get() + 1;
        self.frames.set(frames);
        let ended = match (ended, self.fake_device_loss) {
            (Ok(_), Some(every)) if frames % every == 0 => Err(windows::core::Error::from(
                windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED,
            )),
            (ended, _) => ended,
        };
        match ended {
            Ok((flush, present)) => {
                self.last_frame.set(Some(std::time::Instant::now()));
                if let Some(stats) = self.stats.borrow_mut().as_mut() {
                    stats.frame(frame_start, flush, present, categories.flatten());
                }
            }
            Err(e) if device::is_device_lost(&e) => {
                drop(surface_ref);
                self.lose_device("end_frame", &e);
            }
            Err(e) => return Err(device::err("end_frame", e)),
        }
        Ok(DrawOutcome::Success)
    }
}

impl RendererSealed for D2DRenderer {
    fn text_layout_cache(&self) -> Option<&sharedparley::TextLayoutCache> {
        Some(&self.text_layout_cache)
    }

    fn free_graphics_resources(
        &self,
        component: i_slint_core::item_tree::ItemTreeRef,
        items: &mut dyn Iterator<Item = Pin<i_slint_core::items::ItemRef<'_>>>,
    ) -> Result<(), PlatformError> {
        if let Some(partial) = &self.partial {
            partial.free_graphics_resources(component, items);
        }
        self.text_layout_cache.component_destroyed(component);
        // Shadow bitmaps live in the item cache; the shadow cache only maps options to them.
        self.item_cache.component_destroyed(component);
        self.path_cache.component_destroyed(component);
        Ok(())
    }

    fn set_window_adapter(&self, window_adapter: &Rc<dyn WindowAdapter>) {
        *self.window_adapter.borrow_mut() = Some(Rc::downgrade(window_adapter));
        self.text_layout_cache.clear_all();
        self.clear_gpu_caches();
        if let Some(partial) = &self.partial {
            partial.clear_cache();
        }
    }

    fn mark_dirty_region(&self, region: DirtyRegion) {
        if let Some(partial) = &self.partial {
            partial.mark_dirty_region(region);
        }
    }

    fn window_adapter(&self) -> Option<Rc<dyn WindowAdapter>> {
        self.window_adapter
            .borrow()
            .as_ref()
            .and_then(|w| w.upgrade())
    }

    fn scale_factor(&self) -> Option<ScaleFactor> {
        self.window_adapter()
            .map(|w| ScaleFactor::new(w.window().scale_factor()))
    }

    fn resize(&self, _size: i_slint_core::api::PhysicalSize) -> Result<(), PlatformError> {
        // The swapchain follows the window size at the next frame (Surface::ensure_size).
        Ok(())
    }

    fn take_snapshot(&self) -> Result<SharedPixelBuffer<Rgba8Pixel>, PlatformError> {
        let size = self.window_adapter_rc()?.window().size();
        if size.width == 0 || size.height == 0 {
            return Err("take_snapshot: window size is zero".into());
        }
        if self.surface.borrow().is_none() && self.handle.borrow().is_none() {
            return Err("take_snapshot: the renderer is suspended".into());
        }
        self.want_snapshot.set(true);
        let _ = self.render()?;
        let Some(bgra) = self.snapshot.borrow_mut().take() else {
            return Err("take_snapshot: no frame was read back".into());
        };
        let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(size.width, size.height);
        let out = buffer.make_mut_bytes();
        if out.len() != bgra.len() {
            return Err("take_snapshot: size changed during the frame".into());
        }
        // Premultiplied BGRA to straight RGBA; the window is opaque, so alpha is 255.
        for (dst, src) in out.chunks_exact_mut(4).zip(bgra.chunks_exact(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = 255;
        }
        Ok(buffer)
    }

    fn supports_transformations(&self) -> bool {
        true
    }
}
