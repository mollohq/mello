//! Slint images as Direct2D bitmaps: decoded pixels (PNG) or the SVG rendered
//! at the target size by core (resvg), converted once to premultiplied BGRA
//! and uploaded. The bitmap lives in the D3D11 device; no copy stays in
//! system memory after the upload.
//!
//! `BitmapCache` shares one bitmap between every item that shows the same
//! image at the same size (the same icon appears up to 24 times in mello), with
//! a byte budget: over it, bitmaps no item holds any more are dropped, oldest
//! first (memory rule 3 in specs/features/D2D-RENDERER.md).

use std::collections::HashMap;
use std::rc::Rc;

use i_slint_core::graphics::{euclid, ImageCacheKey, IntSize, SharedImageBuffer};
use i_slint_core::lengths::PhysicalPx;
use i_slint_core::ImageInner;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::*;

pub struct CachedBitmap {
    pub bitmap: ID2D1Bitmap1,
    /// Size of the uploaded pixels.
    pub size: IntSize,
}

impl CachedBitmap {
    pub fn bytes(&self) -> usize {
        (self.size.width * self.size.height * 4) as usize
    }
}

fn to_bgra_premultiplied(buffer: &SharedImageBuffer) -> (Vec<u8>, IntSize) {
    let size = buffer.size();
    let n = (size.width * size.height) as usize;
    let mut out = Vec::with_capacity(n * 4);
    match buffer {
        SharedImageBuffer::RGB8(px) => {
            for p in px.as_bytes().chunks_exact(3) {
                out.extend_from_slice(&[p[2], p[1], p[0], 255]);
            }
        }
        SharedImageBuffer::RGBA8(px) => {
            for p in px.as_bytes().chunks_exact(4) {
                let a = p[3] as u32;
                let m = |c: u8| ((c as u32 * a + 127) / 255) as u8;
                out.extend_from_slice(&[m(p[2]), m(p[1]), m(p[0]), p[3]]);
            }
        }
        SharedImageBuffer::RGBA8Premultiplied(px) => {
            for p in px.as_bytes().chunks_exact(4) {
                out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
            }
        }
    }
    (out, size)
}

pub fn bitmap_from_bgra(
    dc: &ID2D1DeviceContext,
    bgra: &[u8],
    size: IntSize,
) -> Option<CachedBitmap> {
    if size.width == 0 || size.height == 0 {
        return None;
    }
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
        colorContext: std::mem::ManuallyDrop::new(None),
    };
    let bitmap = unsafe {
        dc.CreateBitmap(
            D2D_SIZE_U {
                width: size.width,
                height: size.height,
            },
            Some(bgra.as_ptr() as *const _),
            size.width * 4,
            &props,
        )
    }
    .ok()?;
    Some(CachedBitmap { bitmap, size })
}

/// `target_size` is the render size for scalable sources (SVG); `None` for pixel images.
pub fn bitmap_from_image(
    dc: &ID2D1DeviceContext,
    image: &ImageInner,
    target_size: Option<euclid::Size2D<u32, PhysicalPx>>,
) -> Option<CachedBitmap> {
    let image = match image {
        ImageInner::NineSlice(nine) => {
            let inner = nine.image();
            let inner: &ImageInner = (&inner).into();
            return bitmap_from_image(dc, inner, target_size);
        }
        other => other,
    };
    let buffer = image.render_to_buffer(target_size)?;
    let (bgra, size) = to_bgra_premultiplied(&buffer);
    bitmap_from_bgra(dc, &bgra, size)
}

#[derive(Hash, PartialEq, Eq, Clone)]
pub struct BitmapKey {
    pub image: ImageCacheKey,
    /// The render size for scalable sources.
    pub target: Option<(u32, u32)>,
}

impl BitmapKey {
    pub fn new(
        image: &ImageInner,
        target: Option<euclid::Size2D<u32, PhysicalPx>>,
    ) -> Option<Self> {
        let inner = match image {
            ImageInner::NineSlice(nine) => {
                let inner = nine.image();
                return Self::new((&inner).into(), target);
            }
            other => other,
        };
        Some(Self {
            image: ImageCacheKey::new(inner)?,
            target: target.map(|t| (t.width, t.height)),
        })
    }
}

/// Shared bitmaps with a byte budget. Entries stay while an item holds them.
#[derive(Default)]
pub struct BitmapCache {
    entries: HashMap<BitmapKey, Rc<CachedBitmap>>,
    order: Vec<BitmapKey>,
    bytes: usize,
}

/// 32 MB of GPU bitmaps before unused ones go.
const BUDGET: usize = 32 << 20;

impl BitmapCache {
    pub fn get_or_create(
        &mut self,
        key: BitmapKey,
        create: impl FnOnce() -> Option<CachedBitmap>,
    ) -> Option<Rc<CachedBitmap>> {
        if let Some(found) = self.entries.get(&key) {
            return Some(found.clone());
        }
        let bitmap = Rc::new(create()?);
        self.bytes += bitmap.bytes();
        self.entries.insert(key.clone(), bitmap.clone());
        self.order.push(key);
        if self.bytes > BUDGET {
            self.evict();
        }
        Some(bitmap)
    }

    fn evict(&mut self) {
        let mut kept = Vec::with_capacity(self.order.len());
        for key in std::mem::take(&mut self.order) {
            let unused = self
                .entries
                .get(&key)
                .is_some_and(|b| Rc::strong_count(b) == 1);
            if unused && self.bytes > BUDGET {
                if let Some(b) = self.entries.remove(&key) {
                    self.bytes -= b.bytes();
                }
            } else {
                kept.push(key);
            }
        }
        self.order = kept;
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }
}
