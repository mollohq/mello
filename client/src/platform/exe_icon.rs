//! Extract a game's icon from its executable and cache it as PNG under the app
//! data dir. This is the *primary* art source for a game (assets §8.2): it is
//! what the user already recognises from their taskbar, it needs no catalogue
//! entry, and it works offline — so a game nobody has catalogued looks exactly
//! as good as a curated one.
//!
//! Pipeline: `SHDefExtractIconW` (256 px, then 128, falling back to
//! `ExtractIconExW`) → `HICON` → `GetDIBits` → RGBA. On macOS the bundle's
//! `.icns` is read directly and its largest embedded PNG decoded. The GDI plumbing is the
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

/// PNG bytes for the crew-shared upload, downscaled to 128px.
///
/// The disk cache keeps the full 256px rendition for HiDPI cards, but a 256px
/// PNG runs ~84KB and the backend caps a shared icon at 48KB. Crew-shared
/// copies are only ever drawn small, so the wire copy is re-encoded rather than
/// the limit raised — that keeps every existing client working and needs no
/// backend deploy.
pub const SHARED_ICON_PX: u32 = 128;

pub fn cached_icon_png_bytes(game_id: &str) -> Option<Vec<u8>> {
    let path = cached_icon_path(game_id)?;
    let img = image::open(&path).ok()?;
    let small = if img.width() > SHARED_ICON_PX || img.height() > SHARED_ICON_PX {
        img.resize(
            SHARED_ICON_PX,
            SHARED_ICON_PX,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img
    };
    let mut out = std::io::Cursor::new(Vec::new());
    small.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
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
        // Ask the shell for 256 px, which is what modern games actually ship.
        // The icon is now the primary art for a game (assets §8.2), shown on
        // cards and on HiDPI displays, so requesting the largest available
        // rendition matters more than it did when this was a fallback.
        let mut icon = HICON::default();
        let mut got = SHDefExtractIconW(PCWSTR(wide.as_ptr()), 0, 0, Some(&mut icon), None, 256)
            .is_ok()
            && !icon.is_invalid();
        if !got {
            // Older or minimal executables may only carry 128 px.
            got = SHDefExtractIconW(PCWSTR(wide.as_ptr()), 0, 0, Some(&mut icon), None, 128)
                .is_ok()
                && !icon.is_invalid();
        }

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

/// macOS: the app bundle's icon, which is higher quality than the Windows
/// equivalent — `.icns` carries up to 1024 px and every app has one.
/// `path` may point at the binary inside the bundle, so walk up to the `.app`.
#[cfg(target_os = "macos")]
pub fn extract_exe_icon_rgba(path: &str) -> Option<(Vec<u8>, u32, u32)> {
    let bundle = bundle_root(path)?;
    let icns = largest_icns(&bundle)?;
    decode_icns_png(&std::fs::read(icns).ok()?)
}

/// The biggest `.icns` in the bundle's Resources.
///
/// Reading `CFBundleIconFile` from Info.plist would be more precise, but that
/// means parsing binary plists for no practical gain: bundles almost always
/// ship exactly one app icon, and the largest is it.
#[cfg(target_os = "macos")]
fn largest_icns(bundle: &str) -> Option<std::path::PathBuf> {
    let resources = std::path::Path::new(bundle)
        .join("Contents")
        .join("Resources");
    std::fs::read_dir(resources)
        .ok()?
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| x.eq_ignore_ascii_case("icns"))
                == Some(true)
        })
        .max_by_key(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .map(|e| e.path())
}

/// Pull the largest PNG out of an ICNS container.
///
/// ICNS is a flat sequence of `[4-byte type][4-byte big-endian length][data]`
/// chunks after an 8-byte header. Since OS X 10.7 the large renditions
/// (`ic07`–`ic14`) hold PNG data verbatim, so the biggest PNG chunk is the
/// best available icon — no new dependency, and `image` already decodes PNG.
#[cfg(target_os = "macos")]
fn decode_icns_png(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    const PNG_MAGIC: &[u8] = &[0x89, b'P', b'N', b'G'];
    if data.len() < 8 || &data[0..4] != b"icns" {
        return None;
    }
    let mut best: Option<&[u8]> = None;
    let mut off = 8usize;
    while off + 8 <= data.len() {
        let len = u32::from_be_bytes(data[off + 4..off + 8].try_into().ok()?) as usize;
        // A zero or overlong length means a malformed file; stop rather than loop.
        if len < 8 || off + len > data.len() {
            break;
        }
        let payload = &data[off + 8..off + len];
        if payload.starts_with(PNG_MAGIC) && best.is_none_or(|b| payload.len() > b.len()) {
            best = Some(payload);
        }
        off += len;
    }
    let png = best?;
    let img = image::load_from_memory(png).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some((img.into_raw(), w, h))
}

/// The enclosing `.app` directory for a path, or the path itself when it is
/// already one.
#[cfg(target_os = "macos")]
fn bundle_root(path: &str) -> Option<String> {
    let mut p = std::path::Path::new(path);
    loop {
        if p.extension().and_then(|e| e.to_str()) == Some("app") {
            return Some(p.to_string_lossy().to_string());
        }
        p = p.parent()?;
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn extract_exe_icon_rgba(_path: &str) -> Option<(Vec<u8>, u32, u32)> {
    None
}

/// Executables whose icon is the *runtime's*, not the game's.
///
/// A shared host renders as Java's coffee cup or a generic Electron shell,
/// which is worse than showing the coloured initials badge: a wrong-but-real
/// icon reads as a bug, a badge reads as "no art yet".
const GENERIC_ICON_HOSTS: &[&str] = &[
    "javaw.exe",
    "java.exe",
    "python.exe",
    "pythonw.exe",
    "node.exe",
    "electron.exe",
    "love.exe",
    "nw.exe",
];

/// Should we use this executable's own icon as the game's art?
pub fn icon_is_representative(exe_path: &str) -> bool {
    let file = exe_path
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(exe_path)
        .to_ascii_lowercase();
    !GENERIC_ICON_HOSTS.contains(&file.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The backend caps a shared icon at 48KB (`gameIconMaxBytes`). Extraction
    /// moved to 256px, which produces ~84KB and silently broke every crew
    /// upload with "icon too large". The wire copy is downscaled to keep it
    /// under the cap while the disk cache stays sharp.
    #[test]
    fn the_shared_copy_fits_the_backend_limit() {
        const BACKEND_LIMIT: usize = 48 * 1024;

        // A 256px icon of the worst kind: full-colour noise, which is the
        // least compressible thing a real icon can be.
        let mut img = image::RgbaImage::new(256, 256);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([
                (x * 7 % 256) as u8,
                (y * 13 % 256) as u8,
                ((x + y) % 256) as u8,
                255,
            ]);
        }
        let dyn_img = image::DynamicImage::ImageRgba8(img);

        let mut full = std::io::Cursor::new(Vec::new());
        dyn_img
            .write_to(&mut full, image::ImageFormat::Png)
            .unwrap();
        assert!(
            full.get_ref().len() > BACKEND_LIMIT,
            "the 256px original should exceed the cap, else this test proves nothing"
        );

        let small = dyn_img.resize(
            SHARED_ICON_PX,
            SHARED_ICON_PX,
            image::imageops::FilterType::Lanczos3,
        );
        let mut wire = std::io::Cursor::new(Vec::new());
        small.write_to(&mut wire, image::ImageFormat::Png).unwrap();
        assert!(
            wire.get_ref().len() <= BACKEND_LIMIT,
            "shared copy is {} bytes, over the {BACKEND_LIMIT} cap",
            wire.get_ref().len()
        );
    }

    #[test]
    fn generic_runtime_hosts_do_not_supply_game_art() {
        // Java's coffee cup on a Minecraft card reads as a bug; the coloured
        // badge reads as "no art yet", which is the better failure.
        assert!(!icon_is_representative(
            r"C:\Program Files\Javain\javaw.exe"
        ));
        assert!(!icon_is_representative("/usr/bin/python.exe"));
        assert!(!icon_is_representative("JAVAW.EXE"));
    }

    #[test]
    fn real_game_executables_supply_their_own_art() {
        assert!(icon_is_representative(
            r"C:\Steam\steamapps\common\CSGO\gamein\cs2.exe"
        ));
        assert!(icon_is_representative("eldenring.exe"));
        assert!(icon_is_representative(""));
    }

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
