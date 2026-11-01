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
use i_slint_core::lengths::ScaleFactor;
use i_slint_core::platform::PlatformError;
use i_slint_core::renderer::{DrawOutcome, RendererSealed};
use i_slint_core::textlayout::sharedparley;
use i_slint_core::window::{WindowAdapter, WindowInner};
use i_slint_core::Brush;
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
use itemrenderer::{BrushCache, D2DItemRenderer, ItemBitmapCache, ShadowCache};
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
            brush_cache: Default::default(),
            text_layout_cache: Default::default(),
            pre_present: RefCell::new(None),
            snapshot: RefCell::new(None),
            want_snapshot: Cell::new(false),
            stats: RefCell::new(stats::FrameStats::from_env()),
            last_frame: Rc::new(Cell::new(None)),
            trim_timer: Default::default(),
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
        self.brush_cache.borrow_mut().clear();
    }

    /// The device is gone: forget everything made from it. The next frame recreates
    /// the surface on the stored window.
    fn lose_device(&self, what: &str, e: &windows::core::Error) {
        eprintln!("d2d renderer: device lost in {what} ({e}); recreating");
        self.clear_gpu_caches();
        *self.surface.borrow_mut() = None;
        SharedDevice::reset();
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
            *surface_ref = Some(Surface::new(handle.0, handle.1, handle.2)?);
        }
        let surface = surface_ref.as_mut().unwrap();
        surface.ensure_size(size.width, size.height)?;

        let frame_start = std::time::Instant::now();
        let window_inner = WindowInner::from_pub(window);
        self.item_cache.clear_cache_if_scale_factor_changed(window);
        self.shadow_cache
            .clear_cache_if_scale_factor_changed(window);
        let target = surface.begin_frame()?;
        let dc = surface.dc.clone();
        let device = surface.device.clone();

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
            unsafe { dc.Clear(Some(&clear)) };

            let mut item_renderer = D2DItemRenderer::new(
                &dc,
                &device,
                &self.item_cache,
                &self.bitmap_cache,
                &self.shadow_cache,
                &self.brush_cache,
                &self.text_layout_cache,
                window,
                size.width,
                size.height,
            );

            if let Some(window_item_rc) = window_inner.window_item_rc() {
                let window_item = window_item_rc
                    .downcast::<i_slint_core::items::WindowItem>()
                    .unwrap();
                if !matches!(window_item.as_pin_ref().background(), Brush::SolidColor(..)) {
                    // A gradient background: drawn as a rectangle over the transparent clear.
                    item_renderer.draw_rectangle(
                        window_item.as_pin_ref(),
                        &window_item_rc,
                        i_slint_core::lengths::logical_size_from_api(
                            window.size().to_logical(window_inner.scale_factor()),
                        ),
                        &window_item.as_pin_ref().cached_rendering_data,
                    );
                }
            }

            for (component, origin) in components {
                if let Some(component) = ItemTreeWeak::upgrade(component) {
                    i_slint_core::item_rendering::render_component_items(
                        &component,
                        &mut item_renderer,
                        *origin,
                        &window_adapter,
                    );
                }
            }
            post_render(&mut item_renderer);
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
        );
        if let Some(pixels) = read_back {
            *self.snapshot.borrow_mut() = Some(pixels);
        }
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
        _items: &mut dyn Iterator<Item = Pin<i_slint_core::items::ItemRef<'_>>>,
    ) -> Result<(), PlatformError> {
        self.text_layout_cache.component_destroyed(component);
        // Shadow bitmaps live in the item cache; the shadow cache only maps options to them.
        self.item_cache.component_destroyed(component);
        Ok(())
    }

    fn set_window_adapter(&self, window_adapter: &Rc<dyn WindowAdapter>) {
        *self.window_adapter.borrow_mut() = Some(Rc::downgrade(window_adapter));
        self.text_layout_cache.clear_all();
        self.clear_gpu_caches();
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
