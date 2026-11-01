//! DirectWrite font faces from the font blobs parley hands the renderer.
//!
//! Slint lays text out in core (shared parley); this cache only turns a font
//! blob plus face index plus synthesis (fake bold, oblique) into an
//! `IDWriteFontFace` for `DrawGlyphRun`. The in-memory loader copies the file
//! bytes once per face. Bounded: cleared when it holds more than `CAPACITY`
//! faces (Skia keeps 64). One per thread, shared by every window; the
//! DirectWrite factory is process-shared and survives a lost D3D device.

use i_slint_common::sharedfontique::HashedBlob;
use i_slint_core::textlayout::sharedparley::{fontique, parley};
use std::cell::RefCell;
use std::collections::HashMap;
use windows::core::Interface;
use windows::Win32::Graphics::DirectWrite::*;

const CAPACITY: usize = 32;

pub struct FontCache {
    dwrite: IDWriteFactory5,
    loader: IDWriteInMemoryFontFileLoader,
    faces: HashMap<(HashedBlob, u32, i32), Option<IDWriteFontFace>>,
}

thread_local! {
    static FONT_CACHE: RefCell<Option<FontCache>> = const { RefCell::new(None) };
}

/// Runs `f` with the thread's font cache, created on first use from `dwrite`.
pub fn with_font_cache<R>(
    dwrite: &IDWriteFactory5,
    f: impl FnOnce(&mut FontCache) -> R,
) -> Option<R> {
    FONT_CACHE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = FontCache::new(dwrite).ok();
        }
        slot.as_mut().map(f)
    })
}

impl FontCache {
    fn new(dwrite: &IDWriteFactory5) -> windows::core::Result<Self> {
        unsafe {
            let loader = dwrite.CreateInMemoryFontFileLoader()?;
            let base: IDWriteFactory = dwrite.cast()?;
            base.RegisterFontFileLoader(&loader)?;
            Ok(Self {
                dwrite: dwrite.clone(),
                loader,
                faces: HashMap::new(),
            })
        }
    }

    pub fn face(
        &mut self,
        font: &parley::FontData,
        synthesis: &fontique::Synthesis,
    ) -> Option<IDWriteFontFace> {
        let mut sims = DWRITE_FONT_SIMULATIONS_NONE;
        if synthesis.embolden() {
            sims |= DWRITE_FONT_SIMULATIONS_BOLD;
        }
        if synthesis.skew().is_some() {
            sims |= DWRITE_FONT_SIMULATIONS_OBLIQUE;
        }
        let key = (HashedBlob::from(font.data.clone()), font.index, sims.0);
        if let Some(face) = self.faces.get(&key) {
            return face.clone();
        }
        if self.faces.len() >= CAPACITY {
            self.faces.clear();
        }
        let face = self.create(font, sims);
        self.faces.insert(key, face.clone());
        face
    }

    fn create(
        &self,
        font: &parley::FontData,
        sims: DWRITE_FONT_SIMULATIONS,
    ) -> Option<IDWriteFontFace> {
        unsafe {
            let data: &[u8] = font.data.as_ref();
            let base: IDWriteFactory = self.dwrite.cast().ok()?;
            let file = self
                .loader
                .CreateInMemoryFontFileReference(
                    &base,
                    data.as_ptr() as *const _,
                    data.len() as u32,
                    None,
                )
                .ok()?;
            let mut supported = windows::core::BOOL::default();
            let mut file_type = DWRITE_FONT_FILE_TYPE_UNKNOWN;
            let mut face_type = DWRITE_FONT_FACE_TYPE_UNKNOWN;
            let mut faces = 0u32;
            file.Analyze(
                &mut supported,
                &mut file_type,
                Some(&mut face_type),
                &mut faces,
            )
            .ok()?;
            if !supported.as_bool() {
                return None;
            }
            base.CreateFontFace(face_type, &[Some(file)], font.index, sims)
                .ok()
        }
    }
}

impl Drop for FontCache {
    fn drop(&mut self) {
        self.faces.clear();
        unsafe {
            if let Ok(base) = self.dwrite.cast::<IDWriteFactory>() {
                let _ = base.UnregisterFontFileLoader(&self.loader);
            }
        }
    }
}
