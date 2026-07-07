//! Bundled color-emoji font setup.
//!
//! Slint 1.17's Skia renderer copies the *entire* fallback emoji font into heap
//! memory the first time any color emoji renders, and caches it for the app's
//! lifetime (`i-slint-renderer-skia/font_cache.rs`, `FontMgr::new_from_data`).
//! On macOS that font is Apple Color Emoji.ttc — a one-emoji message costs a
//! permanent 188 MB of phys_footprint, with transient peaks of 2-3x that from
//! the TTC re-extraction workaround in the same file.
//!
//! Workaround: ship OpenMoji (COLRv0, ~10 MB) and put it on `SLINT_FONT_PATH`
//! before Slint initializes. Slint registers fonts from that variable into the
//! generic-family fallback chain (`i-slint-common/sharedfontique.rs`), which
//! Parley consults before the system emoji fallback — so emoji resolve to
//! OpenMoji and the system emoji font is never touched. Measured: idle with
//! emoji on screen drops from ~400 MB to ~216 MB.
//!
//! Bonus: emoji render identically on Windows/macOS/Linux.
//!
//! OpenMoji is CC BY-SA 4.0 — attribution: "Emoji by OpenMoji
//! (https://openmoji.org), CC BY-SA 4.0". Keep this in the app credits.

use std::path::PathBuf;

static OPENMOJI_TTF: &[u8] = include_bytes!("../ui/fonts/OpenMoji-color-glyf_colr_0.ttf");

/// Make the bundled emoji font available to Slint's font fallback.
/// Must run before the first Slint window is created (the fontique collection
/// reads `SLINT_FONT_PATH` once, lazily, on first use).
pub fn setup() {
    let Some(path) = ensure_font_on_disk() else {
        log::warn!("[emoji] cannot materialize bundled emoji font; system emoji fallback stays");
        return;
    };

    // Respect fonts the user already put on SLINT_FONT_PATH (theirs keep
    // priority; Slint preserves insertion order).
    let joined = match std::env::var_os("SLINT_FONT_PATH") {
        Some(existing) => {
            let mut paths: Vec<PathBuf> = std::env::split_paths(&existing).collect();
            paths.push(path.clone());
            std::env::join_paths(paths).ok()
        }
        None => Some(path.clone().into()),
    };
    match joined {
        Some(value) => {
            std::env::set_var("SLINT_FONT_PATH", value);
            log::info!("[emoji] bundled emoji font active: {}", path.display());
        }
        None => log::warn!("[emoji] failed to extend SLINT_FONT_PATH; system emoji fallback stays"),
    }
}

/// Write the embedded font to the app data dir (once; re-written only when the
/// bundled bytes change size, e.g. after an update).
fn ensure_font_on_disk() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", crate::APP_NAME)?;
    let dir = dirs.data_dir().join("fonts");
    let path = dir.join("OpenMoji-color-glyf_colr_0.ttf");

    let up_to_date = std::fs::metadata(&path)
        .map(|m| m.len() == OPENMOJI_TTF.len() as u64)
        .unwrap_or(false);
    if !up_to_date {
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(&path, OPENMOJI_TTF).ok()?;
        log::info!("[emoji] wrote bundled emoji font to {}", path.display());
    }
    Some(path)
}
