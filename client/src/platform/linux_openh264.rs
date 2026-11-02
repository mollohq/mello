//! Cisco's OpenH264 on Linux: the client's side of mello-core's
//! `stream::openh264` (spec 12-STREAMING section 7.4). The setting
//! "Software Video Decoder" (`Settings::openh264_enabled`) turns the codec on
//! and off; a change applies at the next start, as Hardware Acceleration does.

use std::path::{Path, PathBuf};

use mello_core::stream::openh264::{self, Startup};

/// Where the downloaded binary lives: `~/.local/share/m3llo/codecs`, next to
/// the emoji font (`emoji_font.rs`).
fn codec_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", crate::APP_NAME).map(|d| d.data_dir().join("codecs"))
}

/// Startup, before the async runtime and libmello start: sets the variables
/// libmello reads when it loads OpenH264, and starts the download when the
/// codec is on and not on disk.
pub fn setup() {
    let enabled = crate::settings::Settings::load().openh264_enabled;
    match codec_dir() {
        Some(dir) => {
            if openh264::prepare(enabled, &dir) == Startup::Download {
                openh264::spawn_download(dir);
            }
        }
        // Off needs no directory. On without one: libmello still finds a
        // system library or one next to the executable.
        None if !enabled => {
            openh264::prepare(false, Path::new("."));
        }
        None => log::warn!("[openh264] no data directory, no download"),
    }
}
