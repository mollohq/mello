//! The watched stream's format, for the card's label ("720p60";
//! streaming-quality stage 9).
//!
//! The host sends a rung message (`[0x04, 0x05, width, height, fps]`,
//! [`super::controller::rung_message`]) on every rung change and to each
//! viewer that joins. The viewer's control-channel callback stores the last
//! one here; the client's stream timer reads it, as it reads the pointer
//! ([`super::cursor`]). One viewer per client, so one slot.

use std::sync::Mutex;

use super::controller::Rung;

/// The last rung the host announced, and a serial that changes with every
/// store and every reset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ViewerFormat {
    pub rung: Option<Rung>,
    pub serial: u64,
}

static VIEWER_FORMAT: Mutex<ViewerFormat> = Mutex::new(ViewerFormat {
    rung: None,
    serial: 0,
});

/// Store a rung message from the host. Returns `false` for any other
/// control message.
pub fn apply_viewer_rung_message(data: &[u8]) -> bool {
    let Some(rung) = super::controller::parse_rung_message(data) else {
        return false;
    };
    if let Ok(mut format) = VIEWER_FORMAT.lock() {
        if format.rung != Some(rung) {
            log::info!(
                "Stream format from the host: {}x{} at {} fps",
                rung.width,
                rung.height,
                rung.fps
            );
        }
        format.rung = Some(rung);
        format.serial = format.serial.wrapping_add(1);
    }
    true
}

/// The watched stream's last announced format.
pub fn viewer_format() -> ViewerFormat {
    VIEWER_FORMAT.lock().map(|f| *f).unwrap_or_default()
}

/// Forget the format when the viewer stops, so the next stream does not
/// start with this one's label.
pub fn reset_viewer_format() {
    if let Ok(mut format) = VIEWER_FORMAT.lock() {
        format.rung = None;
        format.serial = format.serial.wrapping_add(1);
    }
}

/// Tests that use the global slot hold this, so they do not run at the same
/// time.
#[cfg(test)]
pub(crate) static TEST_SLOT: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::super::controller::rung_message;
    use super::*;

    #[test]
    fn a_rung_message_sets_the_format_and_a_reset_clears_it() {
        let _slot = TEST_SLOT.lock().unwrap_or_else(|e| e.into_inner());
        reset_viewer_format();
        let before = viewer_format();
        assert_eq!(before.rung, None);

        assert!(!apply_viewer_rung_message(&[0x04, 0x03, 1, 0]));
        assert_eq!(viewer_format(), before, "a pause message is not a rung");

        let rung = Rung {
            width: 1280,
            height: 720,
            fps: 60,
        };
        assert!(apply_viewer_rung_message(&rung_message(rung)));
        let after = viewer_format();
        assert_eq!(after.rung, Some(rung));
        assert_ne!(after.serial, before.serial);

        reset_viewer_format();
        let cleared = viewer_format();
        assert_eq!(cleared.rung, None);
        assert_ne!(cleared.serial, after.serial);
    }
}
