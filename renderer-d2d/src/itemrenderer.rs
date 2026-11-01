//! The Slint `ItemRenderer` and `GlyphRenderer` on an `ID2D1DeviceContext`.
//!
//! The drawing table of specs/features/D2D-RENDERER.md section 3: rectangles and border rectangles
//! (solid and gradient brushes, per-corner radii through a path geometry),
//! images (shared bitmaps, `colorize` through an opacity mask, nine-slice,
//! tiling through a bitmap brush), text (parley glyph runs through
//! DirectWrite, color glyph runs for COLR fonts, stroked text through glyph
//! outlines), paths (`ID2D1PathGeometry`), box shadows (the Shadow effect,
//! cached per item), rectangular clips (`PushAxisAlignedClip`), rounded clips
//! and opacity groups (`PushLayer`), conic gradients as a CPU-rasterized
//! bitmap brush. Not done: `cache-rendering-hint` layers draw their children
//! directly, inset shadows are not drawn.

use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;

use i_slint_core::graphics::boxshadowcache::BoxShadowCache;
use i_slint_core::graphics::euclid;
use i_slint_core::graphics::{BorderRadius, IntRect, ResolvedBrush};
use i_slint_core::item_rendering::{
    BorderRectLayout, CachedRenderingData, ItemCache, ItemRenderer, RenderBorderRectangle,
    RenderImage, RenderRectangle, RenderText,
};
use i_slint_core::items::{self, ImageRendering, ImageTiling, ItemRc, Opacity, RenderingResult};
use i_slint_core::lengths::{
    logical_size_from_api, LogicalBorderRadius, LogicalPoint, LogicalRect, LogicalSize,
    LogicalVector, PhysicalPx, ScaleFactor,
};
use i_slint_core::textlayout::sharedparley::{self, fontique, parley, GlyphRenderer};
use i_slint_core::{Brush, Color, ImageInner, SharedString};
use windows::core::Interface;
use windows::Win32::Foundation::DWRITE_E_NOCOLOR;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows_numerics::{Matrix3x2, Vector2};

use crate::device::SharedDevice;
use crate::fonts;
use crate::images::{self, BitmapCache, BitmapKey, CachedBitmap};

pub type PhysicalLength = euclid::Length<f32, PhysicalPx>;
pub type PhysicalRect = euclid::Rect<f32, PhysicalPx>;
pub type PhysicalSize = euclid::Size2D<f32, PhysicalPx>;
pub type PhysicalBorderRadius = BorderRadius<f32, PhysicalPx>;
/// The pixmap producer `ItemRenderer::draw_cached_pixmap` hands us.
type PixmapUpdate<'f> = &'f dyn Fn(&mut dyn FnMut(u32, u32, &[u8]));

/// Per-item GPU objects: image bitmaps, cached pixmaps and shadow bitmaps.
pub type ItemBitmapCache = ItemCache<Option<Rc<CachedBitmap>>>;
pub type ShadowCache = BoxShadowCache<Rc<CachedBitmap>>;

#[derive(Clone, Copy)]
enum Push {
    Clip,
    Layer,
}

#[derive(Clone)]
struct State {
    /// Physical pixels (item local, after the scale factor) to target pixels.
    transform: Matrix3x2,
    /// Logical, item-local clip, as the other renderers track it.
    clip: LogicalRect,
    alpha: f32,
    /// Clips and layers pushed on the device context in this state, popped on restore.
    pushes: Vec<Push>,
}

/// Time spent per kind of item in one frame (`FX_D2D_STATS=1`), microseconds.
#[derive(Default, Clone, Copy)]
pub struct Categories {
    pub rect: u32,
    pub image: u32,
    pub text: u32,
    pub path: u32,
    pub shadow: u32,
    pub pixmap: u32,
}

pub struct D2DItemRenderer<'a> {
    dc: ID2D1DeviceContext,
    categories: Option<Categories>,
    device: &'a SharedDevice,
    item_cache: &'a ItemBitmapCache,
    bitmap_cache: &'a RefCell<BitmapCache>,
    shadow_cache: &'a ShadowCache,
    brush_cache: &'a RefCell<BrushCache>,
    text_layout_cache: &'a sharedparley::TextLayoutCache,
    window: &'a i_slint_core::api::Window,
    scale_factor: ScaleFactor,
    state: Vec<State>,
}

fn color(c: &Color) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: c.red() as f32 / 255.0,
        g: c.green() as f32 / 255.0,
        b: c.blue() as f32 / 255.0,
        a: c.alpha() as f32 / 255.0,
    }
}

fn rect(r: &PhysicalRect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.min_x(),
        top: r.min_y(),
        right: r.max_x(),
        bottom: r.max_y(),
    }
}

fn rounded(r: &PhysicalRect, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect: rect(r),
        radiusX: radius,
        radiusY: radius,
    }
}

const INFINITE_RECT: D2D_RECT_F = D2D_RECT_F {
    left: f32::MIN,
    top: f32::MIN,
    right: f32::MAX,
    bottom: f32::MAX,
};

/// One shape in Direct2D terms: a plain or uniformly rounded rectangle drawn
/// with the rectangle calls, or a geometry for per-corner radii.
enum Shape {
    Rect(D2D_RECT_F),
    Rounded(D2D1_ROUNDED_RECT),
    Geometry(ID2D1Geometry),
}

/// `FX_D2D_DEBUG=1` logs failed Direct2D calls (brush creation) to stderr.
fn debug() -> bool {
    static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DEBUG.get_or_init(|| std::env::var_os("FX_D2D_DEBUG").is_some())
}

fn logged<T>(what: &str, r: windows::core::Result<T>) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            if debug() {
                eprintln!("d2d renderer: {what} failed: {e}");
            }
            None
        }
    }
}

/// Debug switches for A/B memory runs: `FX_D2D_NO_LAYERS=1` draws opacity groups
/// and rounded clips without `PushLayer`, `FX_D2D_NO_SHADOWS=1` skips box shadows.
struct Switches {
    no_layers: bool,
    no_shadows: bool,
}

fn switches() -> &'static Switches {
    static SWITCHES: std::sync::OnceLock<Switches> = std::sync::OnceLock::new();
    SWITCHES.get_or_init(|| Switches {
        no_layers: std::env::var_os("FX_D2D_NO_LAYERS").is_some(),
        no_shadows: std::env::var_os("FX_D2D_NO_SHADOWS").is_some(),
    })
}

/// Brushes are device resources; making one per draw call costs more than the
/// draw. Solid brushes are kept by color and opacity, gradient stop collections
/// by their stops. Both bounded; cleared with the other GPU caches.
#[derive(Default)]
pub struct BrushCache {
    solid: std::collections::HashMap<(u32, u32), ID2D1SolidColorBrush>,
    stops: std::collections::HashMap<Vec<(u32, u32)>, ID2D1GradientStopCollection1>,
}

impl BrushCache {
    pub fn clear(&mut self) {
        self.solid.clear();
        self.stops.clear();
    }
}

fn is_translation(m: &Matrix3x2) -> bool {
    m.M11 == 1.0 && m.M12 == 0.0 && m.M21 == 0.0 && m.M22 == 1.0
}

impl<'a> D2DItemRenderer<'a> {
    /// One renderer per frame over the caches the `D2DRenderer` owns (FemtoVG's
    /// constructor has the same shape).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dc: &ID2D1DeviceContext,
        device: &'a SharedDevice,
        item_cache: &'a ItemBitmapCache,
        bitmap_cache: &'a RefCell<BitmapCache>,
        shadow_cache: &'a ShadowCache,
        brush_cache: &'a RefCell<BrushCache>,
        text_layout_cache: &'a sharedparley::TextLayoutCache,
        window: &'a i_slint_core::api::Window,
        width: u32,
        height: u32,
    ) -> Self {
        let scale_factor = ScaleFactor::new(window.scale_factor());
        unsafe { dc.SetTransform(&Matrix3x2::identity()) };
        Self {
            dc: dc.clone(),
            categories: std::env::var_os("FX_D2D_STATS").map(|_| Categories::default()),
            device,
            item_cache,
            bitmap_cache,
            shadow_cache,
            brush_cache,
            text_layout_cache,
            window,
            scale_factor,
            state: vec![State {
                transform: Matrix3x2::identity(),
                clip: LogicalRect::new(
                    LogicalPoint::default(),
                    PhysicalSize::new(width as f32, height as f32) / scale_factor,
                ),
                alpha: 1.0,
                pushes: Vec::new(),
            }],
        }
    }

    pub fn categories(&self) -> Option<Categories> {
        self.categories
    }

    fn timed(&mut self, pick: fn(&mut Categories) -> &mut u32, f: impl FnOnce(&mut Self)) {
        if self.categories.is_none() {
            return f(self);
        }
        let t = std::time::Instant::now();
        f(self);
        let us = t.elapsed().as_micros() as u32;
        if let Some(c) = self.categories.as_mut() {
            *pick(c) += us;
        }
    }

    /// Pops the clips and layers of the states Slint did not restore (the bottom state).
    pub fn finish(&mut self) {
        while let Some(s) = self.state.pop() {
            Self::pop_all(&self.dc, &s);
        }
    }

    fn pop_all(dc: &ID2D1DeviceContext, s: &State) {
        for push in s.pushes.iter().rev() {
            unsafe {
                match push {
                    Push::Clip => dc.PopAxisAlignedClip(),
                    Push::Layer => dc.PopLayer(),
                }
            }
        }
    }

    fn state(&self) -> &State {
        self.state.last().unwrap()
    }

    fn state_mut(&mut self) -> &mut State {
        self.state.last_mut().unwrap()
    }

    fn set_transform(&self) {
        unsafe { self.dc.SetTransform(&self.state().transform) };
    }

    /// Rounds a translation-only transform to whole pixels for the duration of `f`
    /// (text, images), as Skia and FemtoVG do.
    fn pixel_aligned<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let t = self.state().transform;
        if !is_translation(&t) || (t.M31.fract() == 0.0 && t.M32.fract() == 0.0) {
            return f(self);
        }
        let aligned = Matrix3x2 {
            M31: t.M31.round(),
            M32: t.M32.round(),
            ..t
        };
        unsafe { self.dc.SetTransform(&aligned) };
        let r = f(self);
        unsafe { self.dc.SetTransform(&t) };
        r
    }

    /// A brush for `brush` over a shape of physical `size`, with the state's alpha.
    fn brush(&self, brush: &Brush, size: PhysicalSize) -> Option<ID2D1Brush> {
        let resolved = i_slint_core::graphics::resolve_brush(brush, size, self.scale_factor)?;
        self.brush_resolved(&resolved, size, self.state().alpha)
    }

    /// Direct2D has no conic brush: the gradient is rasterized on the CPU at the
    /// shape's size (capped at 512 px a side) and used as a bitmap brush. Per call,
    /// no cache: mello uses no conic gradient, the lab's beam card animates three.
    fn conic_brush(
        &self,
        g: &i_slint_core::graphics::ResolvedConicGradient,
        size: PhysicalSize,
        alpha: f32,
    ) -> Option<ID2D1Brush> {
        if g.stops.is_empty() || size.width < 1.0 || size.height < 1.0 {
            return None;
        }
        let w = (size.width.ceil() as u32).clamp(1, 512);
        let h = (size.height.ceil() as u32).clamp(1, 512);
        let (sx, sy) = (size.width / w as f32, size.height / h as f32);
        let (cx, cy) = (g.center.x / sx, g.center.y / sy);
        let stops: Vec<(f32, [u8; 4])> = g
            .stops
            .iter()
            .map(|s| {
                let c = s.color.to_argb_u8();
                (s.position, [c.alpha, c.red, c.green, c.blue])
            })
            .collect();
        let mut bgra = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                // The software renderer's mapping: 0 at north, clockwise.
                let dx = x as f32 + 0.5 - cx;
                let dy = y as f32 + 0.5 - cy;
                let mut angle = dy.atan2(dx) + std::f32::consts::FRAC_PI_2;
                if angle < 0.0 {
                    angle += 2.0 * std::f32::consts::PI;
                }
                let position = angle / (2.0 * std::f32::consts::PI);
                let mut color = stops[0].1;
                for pair in stops.windows(2) {
                    let (p1, c1) = pair[0];
                    let (p2, c2) = pair[1];
                    if position >= p1 && position <= p2 {
                        let t = if p2 == p1 {
                            0.0
                        } else {
                            (position - p1) / (p2 - p1)
                        };
                        let lerp = |a: u8, b: u8| ((1.0 - t) * a as f32 + t * b as f32) as u8;
                        color = [
                            lerp(c1[0], c2[0]),
                            lerp(c1[1], c2[1]),
                            lerp(c1[2], c2[2]),
                            lerp(c1[3], c2[3]),
                        ];
                        break;
                    } else if position > p2 {
                        color = c2;
                    }
                }
                let a = color[0] as u32;
                let m = |c: u8| ((c as u32 * a + 127) / 255) as u8;
                bgra.extend_from_slice(&[m(color[3]), m(color[2]), m(color[1]), color[0]]);
            }
        }
        let bitmap = images::bitmap_from_bgra(&self.dc, &bgra, euclid::size2(w, h))?;
        let bprops = D2D1_BITMAP_BRUSH_PROPERTIES1 {
            extendModeX: D2D1_EXTEND_MODE_CLAMP,
            extendModeY: D2D1_EXTEND_MODE_CLAMP,
            interpolationMode: D2D1_INTERPOLATION_MODE_LINEAR,
        };
        let props = D2D1_BRUSH_PROPERTIES {
            opacity: alpha,
            transform: Matrix3x2::scale(sx, sy),
        };
        unsafe {
            logged(
                "CreateBitmapBrush",
                self.dc
                    .CreateBitmapBrush(&bitmap.bitmap, Some(&bprops), Some(&props)),
            )?
            .cast()
            .ok()
        }
    }

    fn solid(&self, c: &Color, alpha: f32) -> Option<ID2D1Brush> {
        let key = (c.as_argb_encoded(), alpha.to_bits());
        let mut cache = self.brush_cache.borrow_mut();
        if let Some(b) = cache.solid.get(&key) {
            return b.cast().ok();
        }
        if cache.solid.len() >= 256 {
            cache.solid.clear();
        }
        let props = D2D1_BRUSH_PROPERTIES {
            opacity: alpha,
            transform: Matrix3x2::identity(),
        };
        let brush = unsafe {
            logged(
                "CreateSolidColorBrush",
                self.dc.CreateSolidColorBrush(&color(c), Some(&props)),
            )?
        };
        let out = brush.cast().ok();
        cache.solid.insert(key, brush);
        out
    }

    fn stops(
        &self,
        stops: &[i_slint_core::graphics::GradientStop],
    ) -> Option<ID2D1GradientStopCollection1> {
        let key: Vec<(u32, u32)> = stops
            .iter()
            .map(|s| (s.position.to_bits(), s.color.as_argb_encoded()))
            .collect();
        let mut cache = self.brush_cache.borrow_mut();
        if let Some(c) = cache.stops.get(&key) {
            return Some(c.clone());
        }
        if cache.stops.len() >= 64 {
            cache.stops.clear();
        }
        let d2d_stops: Vec<D2D1_GRADIENT_STOP> = stops
            .iter()
            .map(|s| D2D1_GRADIENT_STOP {
                position: s.position,
                color: color(&s.color),
            })
            .collect();
        if debug() {
            eprintln!(
                "d2d renderer: stops {:?}",
                d2d_stops
                    .iter()
                    .map(|s| (s.position, s.color.a))
                    .collect::<Vec<_>>()
            );
        }
        let collection = unsafe {
            logged(
                "CreateGradientStopCollection",
                self.dc.CreateGradientStopCollection(
                    &d2d_stops,
                    D2D1_COLOR_SPACE_SRGB,
                    D2D1_COLOR_SPACE_SRGB,
                    D2D1_BUFFER_PRECISION_8BPC_UNORM,
                    D2D1_EXTEND_MODE_CLAMP,
                    D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED,
                ),
            )?
        };
        cache.stops.insert(key, collection.clone());
        Some(collection)
    }

    fn brush_resolved(
        &self,
        resolved: &ResolvedBrush,
        size: PhysicalSize,
        alpha: f32,
    ) -> Option<ID2D1Brush> {
        if alpha <= 0.0 {
            return None;
        }
        let props = D2D1_BRUSH_PROPERTIES {
            opacity: alpha,
            transform: Matrix3x2::identity(),
        };
        unsafe {
            match resolved {
                ResolvedBrush::SolidColor(c) => self.solid(c, alpha),
                ResolvedBrush::LinearGradient(g) => {
                    let collection = self.stops(&g.stops)?;
                    let lp = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                        startPoint: Vector2 {
                            X: g.start.x,
                            Y: g.start.y,
                        },
                        endPoint: Vector2 {
                            X: g.end.x,
                            Y: g.end.y,
                        },
                    };
                    logged(
                        "CreateLinearGradientBrush",
                        self.dc
                            .CreateLinearGradientBrush(&lp, Some(&props), &collection),
                    )?
                    .cast()
                    .ok()
                }
                ResolvedBrush::RadialGradient(g) => {
                    let collection = self.stops(&g.stops)?;
                    let rp = D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
                        center: Vector2 {
                            X: g.center.x,
                            Y: g.center.y,
                        },
                        gradientOriginOffset: Vector2 { X: 0.0, Y: 0.0 },
                        radiusX: g.radius.get(),
                        radiusY: g.radius.get(),
                    };
                    logged(
                        "CreateRadialGradientBrush",
                        self.dc
                            .CreateRadialGradientBrush(&rp, Some(&props), &collection),
                    )?
                    .cast()
                    .ok()
                }
                ResolvedBrush::ConicGradient(g) => self.conic_brush(g, size, alpha),
            }
        }
    }

    /// A rounded rectangle with four radii as a path geometry (arcs), for the
    /// corners the rectangle calls cannot draw and for layer masks.
    fn rounded_geometry(
        &self,
        r: &PhysicalRect,
        radius: &PhysicalBorderRadius,
    ) -> Option<ID2D1Geometry> {
        let factory = &self.device.d2d_factory;
        unsafe {
            if let Some(u) = radius.as_uniform() {
                return factory
                    .CreateRoundedRectangleGeometry(&rounded(r, u.max(0.0)))
                    .ok()?
                    .cast()
                    .ok();
            }
            let geometry = factory.CreatePathGeometry().ok()?;
            let sink = geometry.Open().ok()?;
            let (l, t, rt, b) = (r.min_x(), r.min_y(), r.max_x(), r.max_y());
            let clamp = |v: f32| v.max(0.0).min(r.width() / 2.0).min(r.height() / 2.0);
            let (tl, tr, br, bl) = (
                clamp(radius.top_left),
                clamp(radius.top_right),
                clamp(radius.bottom_right),
                clamp(radius.bottom_left),
            );
            let arc = |to: Vector2, rad: f32| D2D1_ARC_SEGMENT {
                point: to,
                size: D2D_SIZE_F {
                    width: rad,
                    height: rad,
                },
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: D2D1_ARC_SIZE_SMALL,
            };
            sink.BeginFigure(Vector2 { X: l + tl, Y: t }, D2D1_FIGURE_BEGIN_FILLED);
            sink.AddLine(Vector2 { X: rt - tr, Y: t });
            if tr > 0.0 {
                sink.AddArc(&arc(Vector2 { X: rt, Y: t + tr }, tr));
            }
            sink.AddLine(Vector2 { X: rt, Y: b - br });
            if br > 0.0 {
                sink.AddArc(&arc(Vector2 { X: rt - br, Y: b }, br));
            }
            sink.AddLine(Vector2 { X: l + bl, Y: b });
            if bl > 0.0 {
                sink.AddArc(&arc(Vector2 { X: l, Y: b - bl }, bl));
            }
            sink.AddLine(Vector2 { X: l, Y: t + tl });
            if tl > 0.0 {
                sink.AddArc(&arc(Vector2 { X: l + tl, Y: t }, tl));
            }
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            sink.Close().ok()?;
            geometry.cast().ok()
        }
    }

    fn shape(&self, r: &PhysicalRect, radius: &PhysicalBorderRadius) -> Option<Shape> {
        if radius.is_zero() {
            Some(Shape::Rect(rect(r)))
        } else if let Some(u) = radius.as_uniform() {
            Some(Shape::Rounded(rounded(r, u.max(0.0))))
        } else {
            self.rounded_geometry(r, radius).map(Shape::Geometry)
        }
    }

    fn fill_shape(&self, shape: &Shape, brush: &ID2D1Brush) {
        unsafe {
            match shape {
                Shape::Rect(r) => self.dc.FillRectangle(r, brush),
                Shape::Rounded(r) => self.dc.FillRoundedRectangle(r, brush),
                Shape::Geometry(g) => self.dc.FillGeometry(g, brush, None),
            }
        }
    }

    fn stroke_shape(&self, shape: &Shape, width: f32, brush: &ID2D1Brush) {
        unsafe {
            match shape {
                Shape::Rect(r) => self.dc.DrawRectangle(r, brush, width, None),
                Shape::Rounded(r) => self.dc.DrawRoundedRectangle(r, brush, width, None),
                Shape::Geometry(g) => self.dc.DrawGeometry(g, brush, width, None),
            }
        }
    }

    /// `bounds` (physical, item local) sizes the layer's intermediate surface; without
    /// it Direct2D allocates the whole target per nesting level.
    fn push_layer(
        &mut self,
        mask: Option<&ID2D1Geometry>,
        opacity: f32,
        bounds: Option<PhysicalRect>,
    ) {
        let mut params = D2D1_LAYER_PARAMETERS1 {
            contentBounds: bounds.as_ref().map(rect).unwrap_or(INFINITE_RECT),
            geometricMask: std::mem::ManuallyDrop::new(mask.cloned()),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::identity(),
            opacity,
            opacityBrush: std::mem::ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS1_NONE,
        };
        unsafe {
            self.dc.PushLayer(&params, None);
            // The mask clone belongs to `params`; release it now that the call copied it.
            std::mem::ManuallyDrop::drop(&mut params.geometricMask);
        }
        self.state_mut().pushes.push(Push::Layer);
    }

    fn draw_bitmap(
        &self,
        bitmap: &CachedBitmap,
        dest: &D2D_RECT_F,
        src: &D2D_RECT_F,
        rendering: ImageRendering,
        colorize: &Brush,
    ) {
        let alpha = self.state().alpha;
        let mode = match rendering {
            ImageRendering::Pixelated => D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            _ => D2D1_INTERPOLATION_MODE_LINEAR,
        };
        unsafe {
            if !colorize.is_transparent() {
                let size = PhysicalSize::new(dest.right - dest.left, dest.bottom - dest.top);
                let Some(brush) = self.brush(colorize, size) else {
                    return;
                };
                // Gradient brushes are resolved over the item at (0, 0); move them onto the image.
                brush.SetTransform(&Matrix3x2::translation(dest.left, dest.top));
                // The opacity mask fill needs an aliased context; the mask edges carry their own alpha.
                self.dc.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
                self.dc
                    .FillOpacityMask(&bitmap.bitmap, &brush, Some(dest), Some(src));
                self.dc.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            } else {
                self.dc
                    .DrawBitmap(&bitmap.bitmap, Some(dest), alpha, mode, Some(src), None);
            }
        }
    }

    /// Tiled: a bitmap brush that repeats `src` across `dest`.
    fn draw_tiled(
        &self,
        bitmap: &CachedBitmap,
        dest: &D2D_RECT_F,
        src: &D2D_RECT_F,
        scale: (f32, f32),
        phase: (f32, f32),
        rendering: ImageRendering,
    ) {
        let mode = match rendering {
            ImageRendering::Pixelated => D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            _ => D2D1_INTERPOLATION_MODE_LINEAR,
        };
        let bprops = D2D1_BITMAP_BRUSH_PROPERTIES1 {
            extendModeX: D2D1_EXTEND_MODE_WRAP,
            extendModeY: D2D1_EXTEND_MODE_WRAP,
            interpolationMode: mode,
        };
        // Bitmap space to item space: the tile's source origin to the destination
        // origin minus the phase, then the source-to-target scale.
        let transform = Matrix3x2::translation(-src.left - phase.0, -src.top - phase.1)
            * Matrix3x2::scale(scale.0, scale.1)
            * Matrix3x2::translation(dest.left, dest.top);
        let props = D2D1_BRUSH_PROPERTIES {
            opacity: self.state().alpha,
            transform,
        };
        unsafe {
            let Ok(brush) = self
                .dc
                .CreateBitmapBrush(&bitmap.bitmap, Some(&bprops), Some(&props))
            else {
                return;
            };
            self.dc.FillRectangle(dest, &brush);
        }
    }

    fn bitmap_for(
        &self,
        item_rc: &ItemRc,
        item: Pin<&dyn RenderImage>,
    ) -> Option<Rc<CachedBitmap>> {
        let scale = self.scale_factor;
        let dc = self.dc.clone();
        self.item_cache.get_or_update_cache_entry(item_rc, || {
            let image = item.source();
            let inner: &ImageInner = (&image).into();
            let target = if inner.is_svg() {
                Some(i_slint_core::graphics::scalable_render_size(
                    image.size(),
                    item.image_fit(),
                    item.target_size() * scale,
                    scale,
                    item.tiling(),
                )?)
            } else {
                None
            };
            match BitmapKey::new(inner, target) {
                Some(key) => self
                    .bitmap_cache
                    .borrow_mut()
                    .get_or_create(key, || images::bitmap_from_image(&dc, inner, target)),
                None => images::bitmap_from_image(&dc, inner, target).map(Rc::new),
            }
        })
    }

    fn draw_image_impl(
        &mut self,
        item_rc: &ItemRc,
        item: Pin<&dyn RenderImage>,
        size: LogicalSize,
    ) {
        if size.width <= 0.0 || size.height <= 0.0 {
            return;
        }
        let Some(cached) = self.bitmap_for(item_rc, item) else {
            return;
        };
        let image = item.source();
        let orig = image.size();
        if orig.width == 0 || orig.height == 0 {
            return;
        }
        let scale = self.scale_factor;
        let tiling = item.tiling();
        let inner: &ImageInner = (&image).into();
        let fits = if let ImageInner::NineSlice(nine) = inner {
            i_slint_core::graphics::fit9slice(
                orig,
                nine.1,
                size * scale,
                scale,
                item.alignment(),
                tiling,
            )
            .collect::<Vec<_>>()
        } else {
            let source_clip = item
                .source_clip()
                .unwrap_or(IntRect::from_size(orig.cast()));
            vec![i_slint_core::graphics::fit(
                item.image_fit(),
                size * scale,
                source_clip,
                scale,
                item.alignment(),
                tiling,
            )]
        };
        let sx = cached.size.width as f32 / orig.width as f32;
        let sy = cached.size.height as f32 / orig.height as f32;
        let rendering = item.rendering();
        let colorize = item.colorize();
        self.pixel_aligned(|this| {
            for fit in fits {
                let clip = fit.clip_rect.cast::<f32>();
                let src = D2D_RECT_F {
                    left: clip.min_x() * sx,
                    top: clip.min_y() * sy,
                    right: clip.max_x() * sx,
                    bottom: clip.max_y() * sy,
                };
                let dest = D2D_RECT_F {
                    left: fit.offset.x,
                    top: fit.offset.y,
                    right: fit.offset.x + fit.size.width,
                    bottom: fit.offset.y + fit.size.height,
                };
                match fit.tiled {
                    Some(phase) if tiling != (ImageTiling::None, ImageTiling::None) => this
                        .draw_tiled(
                            &cached,
                            &dest,
                            &src,
                            (fit.source_to_target_x / sx, fit.source_to_target_y / sy),
                            (phase.x as f32 * sx, phase.y as f32 * sy),
                            rendering,
                        ),
                    _ => this.draw_bitmap(&cached, &dest, &src, rendering, &colorize),
                }
            }
        });
    }

    /// The blurred, colored shadow of a rounded rectangle as a bitmap, rendered once
    /// per item with the Shadow effect on the offscreen context.
    fn shadow_bitmap(
        &self,
        options: &i_slint_core::graphics::boxshadowcache::BoxShadowOptions,
    ) -> Option<Rc<CachedBitmap>> {
        let texture = options.drop_texture_size();
        let (w, h) = (texture.width.ceil() as u32, texture.height.ceil() as u32);
        if w == 0 || h == 0 {
            return None;
        }
        let dc = &self.device.offscreen;
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        unsafe {
            let size = D2D_SIZE_U {
                width: w,
                height: h,
            };
            let shape = dc.CreateBitmap(size, None, 0, &props).ok()?;
            // The shape, opaque; the effect reads its alpha.
            dc.SetTarget(&shape);
            dc.BeginDraw();
            dc.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            let white = dc
                .CreateSolidColorBrush(
                    &D2D1_COLOR_F {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 1.0,
                    },
                    None,
                )
                .ok()?;
            let shape_rect = PhysicalRect::new(options.shape_origin(), options.shape_size());
            let radius = options.outer_radius();
            match self.shape(&shape_rect, &radius)? {
                Shape::Rect(r) => dc.FillRectangle(&r, &white),
                Shape::Rounded(r) => dc.FillRoundedRectangle(&r, &white),
                Shape::Geometry(g) => dc.FillGeometry(&g, &white, None),
            }
            dc.EndDraw(None, None).ok()?;
            // The shadow: blur and color, into the result bitmap.
            let result = dc.CreateBitmap(size, None, 0, &props).ok()?;
            let effect = dc.CreateEffect(&CLSID_D2D1Shadow).ok()?;
            effect.SetInput(0, &shape, true);
            let sigma = options.blur_sigma();
            effect
                .SetValue(
                    D2D1_SHADOW_PROP_BLUR_STANDARD_DEVIATION.0 as u32,
                    D2D1_PROPERTY_TYPE_FLOAT,
                    &sigma.to_ne_bytes(),
                )
                .ok()?;
            let c = color(&options.color);
            let mut color_bytes = Vec::with_capacity(16);
            for v in [c.r, c.g, c.b, c.a] {
                color_bytes.extend_from_slice(&v.to_ne_bytes());
            }
            effect
                .SetValue(
                    D2D1_SHADOW_PROP_COLOR.0 as u32,
                    D2D1_PROPERTY_TYPE_VECTOR4,
                    &color_bytes,
                )
                .ok()?;
            let output = effect.GetOutput().ok()?;
            dc.SetTarget(&result);
            dc.BeginDraw();
            dc.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            dc.DrawImage(
                &output,
                None,
                None,
                D2D1_INTERPOLATION_MODE_LINEAR,
                D2D1_COMPOSITE_MODE_SOURCE_OVER,
            );
            dc.EndDraw(None, None).ok()?;
            dc.SetTarget(None);
            Some(Rc::new(CachedBitmap {
                bitmap: result,
                size: euclid::size2(w, h),
            }))
        }
    }

    /// Draws a glyph run as its outline, stroked.
    fn stroke_glyphs(
        &self,
        face: &IDWriteFontFace,
        run: &DWRITE_GLYPH_RUN,
        brush: &ID2D1Brush,
        width: f32,
    ) {
        unsafe {
            let Ok(geometry) = self.device.d2d_factory.CreatePathGeometry() else {
                return;
            };
            let Ok(sink) = geometry.Open() else { return };
            let ok = face
                .GetGlyphRunOutline(
                    run.fontEmSize,
                    run.glyphIndices,
                    Some(run.glyphAdvances),
                    Some(run.glyphOffsets),
                    run.glyphCount,
                    false,
                    false,
                    &sink,
                )
                .is_ok();
            if sink.Close().is_err() || !ok {
                return;
            }
            self.dc.DrawGeometry(&geometry, brush, width, None);
        }
    }

    /// Draws a glyph run, as color layers when the font has them (COLR), else plain.
    fn fill_glyphs(&self, run: &DWRITE_GLYPH_RUN, brush: &ID2D1Brush) {
        unsafe {
            let origin = Vector2 { X: 0.0, Y: 0.0 };
            let Ok(factory4) = self.device.dwrite.cast::<IDWriteFactory4>() else {
                self.dc
                    .DrawGlyphRun(origin, run, None, brush, DWRITE_MEASURING_MODE_NATURAL);
                return;
            };
            let formats = DWRITE_GLYPH_IMAGE_FORMATS_TRUETYPE
                | DWRITE_GLYPH_IMAGE_FORMATS_CFF
                | DWRITE_GLYPH_IMAGE_FORMATS_COLR;
            match factory4.TranslateColorGlyphRun(
                origin,
                run,
                None,
                formats,
                DWRITE_MEASURING_MODE_NATURAL,
                None,
                0,
            ) {
                Ok(layers) => {
                    while layers.MoveNext().map(|b| b.as_bool()).unwrap_or(false) {
                        let Ok(layer) = layers.GetCurrentRun() else {
                            break;
                        };
                        let layer = &*layer;
                        if layer.glyphImageFormat == DWRITE_GLYPH_IMAGE_FORMATS_NONE {
                            continue;
                        }
                        let at = Vector2 {
                            X: layer.Base.baselineOriginX,
                            Y: layer.Base.baselineOriginY,
                        };
                        if layer.Base.paletteIndex == 0xFFFF {
                            self.dc.DrawGlyphRun(
                                at,
                                &layer.Base.glyphRun,
                                None,
                                brush,
                                DWRITE_MEASURING_MODE_NATURAL,
                            );
                        } else {
                            let c = layer.Base.runColor;
                            let Some(layer_brush) = self.solid(
                                &Color::from_argb_f32(c.a, c.r, c.g, c.b),
                                self.state().alpha,
                            ) else {
                                continue;
                            };
                            self.dc.DrawGlyphRun(
                                at,
                                &layer.Base.glyphRun,
                                None,
                                &layer_brush,
                                DWRITE_MEASURING_MODE_NATURAL,
                            );
                        }
                    }
                }
                Err(e) if e.code() == DWRITE_E_NOCOLOR => {
                    self.dc
                        .DrawGlyphRun(origin, run, None, brush, DWRITE_MEASURING_MODE_NATURAL);
                }
                Err(_) => {
                    self.dc
                        .DrawGlyphRun(origin, run, None, brush, DWRITE_MEASURING_MODE_NATURAL);
                }
            }
        }
    }

    fn draw_path_impl(&mut self, path: Pin<&items::Path>, item_rc: &ItemRc, size: LogicalSize) {
        // One geometry per draw; caps and joins take Direct2D's defaults.
        let Some((offset, events)) = path.fitted_path_events(item_rc) else {
            return;
        };
        let sf = self.scale_factor.get();
        let Ok(geometry) = (unsafe { self.device.d2d_factory.CreatePathGeometry() }) else {
            return;
        };
        let Ok(sink) = (unsafe { geometry.Open() }) else {
            return;
        };
        let v = |p: lyon_path::math::Point| Vector2 {
            X: p.x * sf,
            Y: p.y * sf,
        };
        unsafe {
            sink.SetFillMode(match path.fill_rule() {
                items::FillRule::Evenodd => D2D1_FILL_MODE_ALTERNATE,
                _ => D2D1_FILL_MODE_WINDING,
            });
            let mut open = false;
            for event in events.iter() {
                match event {
                    lyon_path::Event::Begin { at } => {
                        sink.BeginFigure(v(at), D2D1_FIGURE_BEGIN_FILLED);
                        open = true;
                    }
                    lyon_path::Event::Line { to, .. } => sink.AddLine(v(to)),
                    lyon_path::Event::Quadratic { ctrl, to, .. } => {
                        sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT {
                            point1: v(ctrl),
                            point2: v(to),
                        })
                    }
                    lyon_path::Event::Cubic {
                        ctrl1, ctrl2, to, ..
                    } => sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                        point1: v(ctrl1),
                        point2: v(ctrl2),
                        point3: v(to),
                    }),
                    lyon_path::Event::End { close, .. } => {
                        if open {
                            sink.EndFigure(if close {
                                D2D1_FIGURE_END_CLOSED
                            } else {
                                D2D1_FIGURE_END_OPEN
                            });
                            open = false;
                        }
                    }
                }
            }
            if open {
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
            }
            if sink.Close().is_err() {
                return;
            }
        }
        let phys_size = size * self.scale_factor;
        let fill = self.brush(&path.fill(), phys_size);
        let stroke_width = (path.stroke_width() * self.scale_factor).get();
        let stroke = if stroke_width > 0.0 {
            self.brush(&path.stroke(), phys_size)
        } else {
            None
        };
        if fill.is_none() && stroke.is_none() {
            return;
        }
        let offset = offset * self.scale_factor;
        let saved = self.state().transform;
        let moved = Matrix3x2::translation(offset.x, offset.y) * saved;
        unsafe {
            self.dc.SetTransform(&moved);
            if let Some(fill) = &fill {
                self.dc.FillGeometry(&geometry, fill, None);
            }
            if let Some(stroke) = &stroke {
                self.dc.DrawGeometry(&geometry, stroke, stroke_width, None);
            }
            self.dc.SetTransform(&saved);
        }
    }

    fn draw_box_shadow_impl(
        &mut self,
        box_shadow: Pin<&items::BoxShadow>,
        item_rc: &ItemRc,
        _: LogicalSize,
    ) {
        if box_shadow.color().alpha() == 0
            || (box_shadow.blur().get() == 0.0
                && box_shadow.offset_x().get() == 0.0
                && box_shadow.offset_y().get() == 0.0)
        {
            return;
        }
        // Inset shadows are not drawn (FemtoVG skips them too).
        if box_shadow.inset() || switches().no_shadows {
            return;
        }
        let scale = self.scale_factor;
        let Some(shadow) = self.shadow_cache.get_box_shadow(
            item_rc,
            self.item_cache,
            box_shadow,
            scale,
            |options| self.shadow_bitmap(options),
        ) else {
            return;
        };
        let blur = (box_shadow.blur() * scale).get();
        let offset =
            LogicalPoint::from_lengths(box_shadow.offset_x(), box_shadow.offset_y()) * scale;
        let dest = D2D_RECT_F {
            left: offset.x - blur,
            top: offset.y - blur,
            right: offset.x - blur + shadow.size.width as f32,
            bottom: offset.y - blur + shadow.size.height as f32,
        };
        let src = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: shadow.size.width as f32,
            bottom: shadow.size.height as f32,
        };
        unsafe {
            self.dc.DrawBitmap(
                &shadow.bitmap,
                Some(&dest),
                self.state().alpha,
                D2D1_INTERPOLATION_MODE_LINEAR,
                Some(&src),
                None,
            );
        }
    }

    fn draw_cached_pixmap_impl(&mut self, item_rc: &ItemRc, update_fn: PixmapUpdate<'_>) {
        let dc = self.dc.clone();
        let Some(cached) = self.item_cache.get_or_update_cache_entry(item_rc, || {
            let mut result = None;
            update_fn(&mut |width, height, data| {
                // RGBA premultiplied in, BGRA premultiplied out.
                let mut bgra = Vec::with_capacity(data.len());
                for p in data.chunks_exact(4) {
                    bgra.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
                }
                result =
                    images::bitmap_from_bgra(&dc, &bgra, euclid::size2(width, height)).map(Rc::new);
            });
            result
        }) else {
            return;
        };
        let (w, h) = (cached.size.width as f32, cached.size.height as f32);
        let r = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: w,
            bottom: h,
        };
        self.pixel_aligned(|this| {
            this.draw_bitmap(&cached, &r, &r, ImageRendering::Smooth, &Brush::default())
        });
    }
}

impl<'a> ItemRenderer for D2DItemRenderer<'a> {
    fn global_alpha_transparent(&self) -> bool {
        self.state().alpha == 0.0
    }

    fn draw_rectangle(
        &mut self,
        rect_item: Pin<&dyn RenderRectangle>,
        _: &ItemRc,
        size: LogicalSize,
        _: &CachedRenderingData,
    ) {
        self.timed(
            |c| &mut c.rect,
            |this| {
                let geometry = PhysicalRect::from(size * this.scale_factor);
                if geometry.is_empty() {
                    return;
                }
                let Some(brush) = this.brush(&rect_item.background(), geometry.size) else {
                    return;
                };
                // A rectangle on whole pixels under a pure translation has no edge to
                // antialias; the aliased fill skips Direct2D's per-primitive tessellation
                // (the belt's dot grids are hundreds of these per frame).
                let t = this.state().transform;
                let on_pixels = is_translation(&t) && {
                    let near = |v: f32| (v - v.round()).abs() < 0.01;
                    near(t.M31 + geometry.min_x())
                        && near(t.M32 + geometry.min_y())
                        && near(t.M31 + geometry.max_x())
                        && near(t.M32 + geometry.max_y())
                };
                unsafe {
                    if on_pixels {
                        this.dc.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
                        this.dc.FillRectangle(&rect(&geometry), &brush);
                        this.dc.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
                    } else {
                        this.dc.FillRectangle(&rect(&geometry), &brush);
                    }
                }
            },
        );
    }

    fn draw_border_rectangle(
        &mut self,
        rect_item: Pin<&dyn RenderBorderRectangle>,
        _: &ItemRc,
        size: LogicalSize,
        _: &CachedRenderingData,
    ) {
        self.timed(
            |c| &mut c.rect,
            |this| {
                let Some(layout) = BorderRectLayout::new(rect_item, size, this.scale_factor) else {
                    return;
                };
                if let Some(fill) = this.brush(&rect_item.background(), layout.brush_size) {
                    if let Some(shape) =
                        this.shape(&layout.background_rect, &layout.background_radius)
                    {
                        this.fill_shape(&shape, &fill);
                    }
                }
                if layout.border_width.get() > 0.0 {
                    if let Some(border) = this.brush(&layout.border_color, layout.brush_size) {
                        if let Some(shape) = this.shape(&layout.border_rect, &layout.border_radius)
                        {
                            this.stroke_shape(&shape, layout.border_width.get(), &border);
                        }
                    }
                }
            },
        );
    }

    fn draw_window_background(
        &mut self,
        rect_item: Pin<&dyn RenderRectangle>,
        _: &ItemRc,
        _: LogicalSize,
        _: &CachedRenderingData,
    ) {
        // The frame clears with the solid background; this registers the dependency.
        let _ = rect_item.background();
    }

    fn draw_image(
        &mut self,
        image: Pin<&dyn RenderImage>,
        item_rc: &ItemRc,
        size: LogicalSize,
        _: &CachedRenderingData,
    ) {
        self.timed(
            |c| &mut c.image,
            |this| this.draw_image_impl(item_rc, image, size),
        );
    }

    fn draw_text(
        &mut self,
        text: Pin<&dyn RenderText>,
        self_rc: &ItemRc,
        size: LogicalSize,
        _: &CachedRenderingData,
    ) {
        self.timed(
            |c| &mut c.text,
            |this| {
                this.pixel_aligned(|this| {
                    sharedparley::draw_text(
                        this,
                        text,
                        Some(self_rc),
                        size,
                        Some(this.text_layout_cache),
                    );
                })
            },
        );
    }

    fn draw_text_input(
        &mut self,
        text_input: Pin<&items::TextInput>,
        self_rc: &ItemRc,
        size: LogicalSize,
    ) {
        self.timed(
            |c| &mut c.text,
            |this| {
                this.pixel_aligned(|this| {
                    sharedparley::draw_text_input(
                        this,
                        text_input,
                        self_rc,
                        size,
                        this.text_layout_cache,
                    );
                })
            },
        );
    }

    fn draw_path(&mut self, path: Pin<&items::Path>, item_rc: &ItemRc, size: LogicalSize) {
        self.timed(
            |c| &mut c.path,
            |this| this.draw_path_impl(path, item_rc, size),
        );
    }

    fn draw_box_shadow(
        &mut self,
        box_shadow: Pin<&items::BoxShadow>,
        item_rc: &ItemRc,
        size: LogicalSize,
    ) {
        self.timed(
            |c| &mut c.shadow,
            |this| this.draw_box_shadow_impl(box_shadow, item_rc, size),
        );
    }

    fn draw_cached_pixmap(
        &mut self,
        item_rc: &ItemRc,
        update_fn: &dyn Fn(&mut dyn FnMut(u32, u32, &[u8])),
    ) {
        self.timed(
            |c| &mut c.pixmap,
            |this| this.draw_cached_pixmap_impl(item_rc, update_fn),
        );
    }

    fn visit_opacity(
        &mut self,
        opacity_item: Pin<&Opacity>,
        item_rc: &ItemRc,
        _: LogicalSize,
    ) -> RenderingResult {
        let opacity = opacity_item.opacity();
        if Opacity::need_layer(item_rc, opacity) && !switches().no_layers {
            // Overlapping children: a layer composited at the opacity, so they do not
            // show through each other. Popped when this item's state is restored. The
            // layer covers the children's bounds within the clip, as core's layer
            // renderer sizes its textures; the children's own walk tracks the properties.
            let bounds = i_slint_core::properties::evaluate_no_tracking(|| {
                i_slint_core::item_rendering::item_children_bounding_rect(
                    item_rc,
                    &self.window().window_adapter(),
                )
                .intersection(
                    &self
                        .get_current_clip()
                        .union(&LogicalRect::from_size(item_rc.geometry().size)),
                )
                .unwrap_or_default()
            });
            let bounds = bounds * self.scale_factor;
            self.push_layer(None, opacity, Some(bounds));
        } else {
            self.apply_opacity(opacity);
        }
        RenderingResult::ContinueRenderingChildren
    }

    fn combine_clip(&mut self, clip_rect: LogicalRect, radius: LogicalBorderRadius) -> bool {
        let phys = clip_rect * self.scale_factor;
        let valid = {
            let state = self.state_mut();
            match state.clip.intersection(&clip_rect) {
                Some(r) => {
                    state.clip = r;
                    true
                }
                None => {
                    state.clip = LogicalRect::default();
                    false
                }
            }
        };
        if radius.is_zero() || switches().no_layers {
            unsafe {
                self.dc
                    .PushAxisAlignedClip(&rect(&phys), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE)
            };
            self.state_mut().pushes.push(Push::Clip);
        } else {
            let mask = self.rounded_geometry(&phys, &(radius * self.scale_factor));
            self.push_layer(mask.as_ref(), 1.0, Some(phys));
        }
        valid
    }

    fn get_current_clip(&self) -> LogicalRect {
        self.state().clip
    }

    fn translate(&mut self, distance: LogicalVector) {
        let phys = distance * self.scale_factor;
        let state = self.state_mut();
        state.transform = Matrix3x2::translation(phys.x, phys.y) * state.transform;
        state.clip = state.clip.translate(-distance);
        self.set_transform();
    }

    fn rotate(&mut self, angle_in_degrees: f32) {
        let state = self.state_mut();
        state.transform = Matrix3x2::rotation(angle_in_degrees) * state.transform;
        // Bounding box of the rotated clip, as FemtoVG does.
        let rad = angle_in_degrees.to_radians();
        let (sin, cos) = (-rad).sin_cos();
        let clip = state.clip;
        let rot = |p: LogicalPoint| (p.x * cos - p.y * sin, p.x * sin + p.y * cos);
        let corners = [
            rot(clip.origin),
            rot(clip.origin + euclid::vec2(clip.width(), 0.0)),
            rot(clip.origin + euclid::vec2(0.0, clip.height())),
            rot(clip.origin + clip.size),
        ];
        let min_x = corners.iter().fold(f32::MAX, |a, b| b.0.min(a));
        let min_y = corners.iter().fold(f32::MAX, |a, b| b.1.min(a));
        let max_x = corners.iter().fold(f32::MIN, |a, b| b.0.max(a));
        let max_y = corners.iter().fold(f32::MIN, |a, b| b.1.max(a));
        state.clip = LogicalRect::new(
            LogicalPoint::new(min_x, min_y),
            LogicalSize::new(max_x - min_x, max_y - min_y),
        );
        self.set_transform();
    }

    fn scale(&mut self, x: f32, y: f32) {
        let state = self.state_mut();
        state.transform = Matrix3x2::scale(x, y) * state.transform;
        state.clip.origin.x /= x;
        state.clip.origin.y /= y;
        state.clip.size.width /= x;
        state.clip.size.height /= y;
        self.set_transform();
    }

    fn apply_opacity(&mut self, opacity: f32) {
        self.state_mut().alpha *= opacity;
    }

    fn save_state(&mut self) {
        let mut s = self.state().clone();
        s.pushes = Vec::new();
        self.state.push(s);
    }

    fn restore_state(&mut self) {
        if let Some(s) = self.state.pop() {
            Self::pop_all(&self.dc, &s);
        }
        if self.state.is_empty() {
            self.state.push(State {
                transform: Matrix3x2::identity(),
                clip: LogicalRect::default(),
                alpha: 1.0,
                pushes: Vec::new(),
            });
        }
        self.set_transform();
    }

    fn scale_factor(&self) -> ScaleFactor {
        self.scale_factor
    }

    fn draw_string(&mut self, string: &str, color: Color) {
        sharedparley::draw_text(
            self,
            std::pin::pin!((SharedString::from(string), Brush::from(color))),
            None,
            logical_size_from_api(self.window.size().to_logical(self.scale_factor.get())),
            None,
        );
    }

    fn draw_image_direct(&mut self, image: i_slint_core::graphics::Image) {
        let inner: &ImageInner = (&image).into();
        let size = image.size();
        let target = inner
            .is_svg()
            .then(|| euclid::size2(size.width, size.height));
        let Some(bitmap) = images::bitmap_from_image(&self.dc, inner, target) else {
            return;
        };
        let r = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: size.width as f32,
            bottom: size.height as f32,
        };
        let src = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: bitmap.size.width as f32,
            bottom: bitmap.size.height as f32,
        };
        self.draw_bitmap(&bitmap, &r, &src, ImageRendering::Smooth, &Brush::default());
    }

    fn window(&self) -> &i_slint_core::window::WindowInner {
        i_slint_core::window::WindowInner::from_pub(self.window)
    }

    fn as_any(&mut self) -> Option<&mut dyn core::any::Any> {
        None
    }
}

#[derive(Clone)]
pub enum GlyphBrush {
    Fill(ID2D1Brush),
    Stroke(ID2D1Brush, f32),
}

impl GlyphBrush {
    fn brush(&self) -> &ID2D1Brush {
        match self {
            GlyphBrush::Fill(b) | GlyphBrush::Stroke(b, _) => b,
        }
    }
}

impl<'a> GlyphRenderer for D2DItemRenderer<'a> {
    type PlatformBrush = GlyphBrush;

    fn platform_text_fill_brush(
        &mut self,
        brush: Brush,
        size: LogicalSize,
    ) -> Option<Self::PlatformBrush> {
        self.brush(&brush, size * self.scale_factor)
            .map(GlyphBrush::Fill)
    }

    fn platform_brush_for_color(&mut self, c: &Color) -> Option<Self::PlatformBrush> {
        if c.alpha() == 0 {
            return None;
        }
        self.solid(c, self.state().alpha).map(GlyphBrush::Fill)
    }

    fn platform_text_stroke_brush(
        &mut self,
        brush: Brush,
        width: f32,
        size: LogicalSize,
    ) -> Option<Self::PlatformBrush> {
        if width <= 0.0 {
            return None;
        }
        self.brush(&brush, size * self.scale_factor)
            .map(|b| GlyphBrush::Stroke(b, width))
    }

    fn draw_glyph_run(
        &mut self,
        font: &parley::FontData,
        font_size: PhysicalLength,
        _normalized_coords: &[i16],
        synthesis: &fontique::Synthesis,
        brush: Self::PlatformBrush,
        y_offset: PhysicalLength,
        glyphs_it: &mut dyn Iterator<Item = parley::layout::Glyph>,
    ) {
        let Some(face) =
            fonts::with_font_cache(&self.device.dwrite, |cache| cache.face(font, synthesis))
                .flatten()
        else {
            return;
        };
        let mut indices: Vec<u16> = Vec::new();
        let mut offsets: Vec<DWRITE_GLYPH_OFFSET> = Vec::new();
        let snap_y = crate::text::tuning().snap_y;
        for g in glyphs_it {
            indices.push(g.id as u16);
            // Parley positions every glyph; zero advances, positions as offsets. The
            // baseline snaps to a whole pixel as Skia's does; x keeps its fraction.
            let y = g.y + y_offset.get();
            let y = if snap_y { y.round() } else { y };
            offsets.push(DWRITE_GLYPH_OFFSET {
                advanceOffset: g.x,
                ascenderOffset: -y,
            });
        }
        if indices.is_empty() {
            return;
        }
        let advances = vec![0.0f32; indices.len()];
        let mut run = DWRITE_GLYPH_RUN {
            fontFace: std::mem::ManuallyDrop::new(Some(face.clone())),
            fontEmSize: font_size.get(),
            glyphCount: indices.len() as u32,
            glyphIndices: indices.as_ptr(),
            glyphAdvances: advances.as_ptr(),
            glyphOffsets: offsets.as_ptr(),
            isSideways: false.into(),
            bidiLevel: 0,
        };
        match &brush {
            GlyphBrush::Fill(b) => self.fill_glyphs(&run, b),
            GlyphBrush::Stroke(b, width) => self.stroke_glyphs(&face, &run, b, *width),
        }
        unsafe { std::mem::ManuallyDrop::drop(&mut run.fontFace) };
    }

    fn fill_rectangle(
        &mut self,
        physical_rect: sharedparley::PhysicalRect,
        brush: Self::PlatformBrush,
        radius: sharedparley::PhysicalLength,
        border: Option<sharedparley::RectangleBorder<Self::PlatformBrush>>,
    ) {
        let radius = PhysicalBorderRadius::new_uniform(radius.get().max(0.0));
        let Some(shape) = self.shape(&physical_rect, &radius) else {
            return;
        };
        self.fill_shape(&shape, brush.brush());
        if let Some(sharedparley::RectangleBorder { brush, width }) = border {
            if width.get() > 0.0 {
                self.stroke_shape(&shape, width.get(), brush.brush());
            }
        }
    }
}
