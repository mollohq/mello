//! Extract a game's icon from its executable (Windows resource extraction)
//! and cache it as PNG under the app data dir.
//!
//! Pipeline: `SHDefExtractIconW` (requesting 128 px, falls back to
//! `ExtractIconExW`) → `HICON` → `GetDIBits` → RGBA. The GDI plumbing is the
//! inverse of `taskbar_toolbar.rs::create_icon_from_rgba` and mirrors its
//! DC/bitmap lifecycle. Extraction is blocking (file IO + GDI) — call it from
//! a worker thread; `slint::Image` construction happens elsewhere on the UI
//! thread (Images are not Send).

use std::path::PathBuf;

/// Disk cache dir for game icons — durable app data, not temp.
///
/// Windows: `%LOCALAPPDATA%/Mello/game_icons` (same family as the telemetry
/// token). Elsewhere (macOS views crew-shared icons even though extraction is
/// Windows-only): the `directories` app-data dir, matching the log-dir
/// pattern in `main.rs`.
pub fn icon_cache_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    if let Some(base) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        return Some(base.join("Mello").join("game_icons"));
    }
    directories::ProjectDirs::from("app", "mello", "mello")
        .map(|dirs| dirs.data_dir().join("game_icons"))
}

pub fn cached_icon_path(game_id: &str) -> Option<PathBuf> {
    // Ids are slugs ([a-z0-9-]); refuse anything path-ish defensively.
    if game_id.is_empty()
        || !game_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return None;
    }
    Some(icon_cache_dir()?.join(format!("{game_id}.png")))
}

/// Encode extracted RGBA to the PNG cache. Returns the cached path.
pub fn cache_icon_png(game_id: &str, rgba: &[u8], w: u32, h: u32) -> Option<PathBuf> {
    let path = cached_icon_path(game_id)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    let img = image::RgbaImage::from_raw(w, h, rgba.to_vec())?;
    img.save_with_format(&path, image::ImageFormat::Png).ok()?;
    log::info!("[game-icon] cached {} ({}x{})", path.display(), w, h);
    Some(path)
}

/// Load a cached icon back as RGBA (for `slint::Image` on the UI thread).
pub fn load_cached_icon_rgba(game_id: &str) -> Option<(Vec<u8>, u32, u32)> {
    let path = cached_icon_path(game_id)?;
    let img = image::open(path).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Some((img.into_raw(), w, h))
}

/// Raw PNG bytes of a cached icon (for the crew-shared upload).
pub fn cached_icon_png_bytes(game_id: &str) -> Option<Vec<u8>> {
    std::fs::read(cached_icon_path(game_id)?).ok()
}

/// Store PNG bytes fetched from the backend into the local cache. Validates
/// they decode as an image before writing.
pub fn store_fetched_icon_png(game_id: &str, png: &[u8]) -> Option<()> {
    image::load_from_memory(png).ok()?;
    let path = cached_icon_path(game_id)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    std::fs::write(&path, png).ok()?;
    log::info!("[game-icon] stored fetched icon {}", path.display());
    Some(())
}

/// Extract the executable's icon as RGBA. `None` when the exe has no icon or
/// extraction fails. Blocking; run on a worker thread.
#[cfg(target_os = "windows")]
pub fn extract_exe_icon_rgba(path: &str) -> Option<(Vec<u8>, u32, u32)> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::{ExtractIconExW, SHDefExtractIconW};
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        // Preferred: ask the shell for a 128 px rendition (scales down well
        // for cards/badges; most games ship 256 px icons).
        let mut icon = HICON::default();
        let got = SHDefExtractIconW(PCWSTR(wide.as_ptr()), 0, 0, Some(&mut icon), None, 128)
            .is_ok()
            && !icon.is_invalid();

        let icon = if got {
            icon
        } else {
            // Fallback: the classic large icon (usually 32 px).
            let mut large = HICON::default();
            let n = ExtractIconExW(PCWSTR(wide.as_ptr()), 0, Some(&mut large), None, 1);
            if n == 0 || large.is_invalid() {
                return None;
            }
            large
        };

        let rgba = hicon_to_rgba(icon);
        let _ = DestroyIcon(icon);
        rgba
    }
}

#[cfg(target_os = "windows")]
unsafe fn hicon_to_rgba(
    icon: windows::Win32::UI::WindowsAndMessaging::HICON,
) -> Option<(Vec<u8>, u32, u32)> {
    use windows::Win32::Graphics::Gdi::{
        GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetIconInfo;

    let mut info = Default::default();
    GetIconInfo(icon, &mut info).ok()?;

    // RAII-ish cleanup: both bitmaps must be deleted whatever happens below.
    struct Bitmaps(
        windows::Win32::Graphics::Gdi::HBITMAP,
        windows::Win32::Graphics::Gdi::HBITMAP,
    );
    impl Drop for Bitmaps {
        fn drop(&mut self) {
            unsafe {
                use windows::Win32::Graphics::Gdi::DeleteObject;
                if !self.0.is_invalid() {
                    let _ = DeleteObject(self.0.into());
                }
                if !self.1.is_invalid() {
                    let _ = DeleteObject(self.1.into());
                }
            }
        }
    }
    let _bitmaps = Bitmaps(info.hbmColor, info.hbmMask);

    if info.hbmColor.is_invalid() {
        return None; // monochrome icon; not worth rendering
    }

    let mut bm = BITMAP::default();
    if GetObjectW(
        info.hbmColor.into(),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bm as *mut BITMAP as *mut core::ffi::c_void),
    ) == 0
    {
        return None;
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight);
    if w <= 0 || h <= 0 || w > 512 || h > 512 {
        return None;
    }

    let mut bmi = BITMAPINFO {
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

    let hdc = GetDC(None);
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let lines = GetDIBits(
        hdc,
        info.hbmColor,
        0,
        h as u32,
        Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
        &mut bmi,
        DIB_RGB_COLORS,
    );
    ReleaseDC(None, hdc);
    if lines == 0 {
        return None;
    }

    // BGRA → RGBA. Icons without an alpha channel come back all-zero alpha;
    // treat those as opaque (the standard tracker/shell heuristic).
    let all_alpha_zero = buf.chunks_exact(4).all(|p| p[3] == 0);
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
        if all_alpha_zero {
            px[3] = 255;
        }
    }

    Some((buf, w as u32, h as u32))
}

#[cfg(not(target_os = "windows"))]
pub fn extract_exe_icon_rgba(_path: &str) -> Option<(Vec<u8>, u32, u32)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_icon_path_rejects_pathish_ids() {
        assert!(cached_icon_path("custom-night-stones").is_some());
        assert!(cached_icon_path("..\\evil").is_none());
        assert!(cached_icon_path("a/b").is_none());
        assert!(cached_icon_path("").is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn extracts_an_icon_from_a_system_exe() {
        let icon = extract_exe_icon_rgba("C:\\Windows\\System32\\notepad.exe");
        let (rgba, w, h) = icon.expect("notepad has an icon");
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        assert!(w >= 16 && h >= 16);
        // Not fully transparent.
        assert!(rgba.chunks_exact(4).any(|p| p[3] > 0));
    }
}
