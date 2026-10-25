//! The watched stream's game-audio volume (streaming-quality stage 9).
//!
//! The stream card's volume popover sends `Command::SetStreamAudioVolume`.
//! libmello keeps the gain on the view (`mello_stream_viewer_set_audio_volume`),
//! across the host's pause mute and across a reconnect, because both keep the
//! view. A new watch makes a new view at unity; the card sends its value again
//! on `StreamWatching`.
//!
//! The card's mute is a gain of 0, not `mello_stream_viewer_set_audio_muted`:
//! that call belongs to the host's pause (the stream tick unmutes on a
//! resume), and a gain keeps the playout running, so an unmute is in sync at
//! once.

use super::Client;

impl Client {
    pub(super) fn handle_set_stream_audio_volume(&mut self, gain: f32) {
        let viewer = stream_view_of(
            self.viewer_state.as_ref().and_then(|vs| vs.viewer),
            self.watch_join.as_ref().and_then(|join| join.decoder),
        );
        let Some(viewer) = viewer else {
            log::debug!("Stream volume {gain:.2}: no stream view");
            return;
        };
        // SAFETY: the view is live while `viewer_state` or the watch's decoder
        // holds it; both are only dropped on this loop.
        let applied = unsafe { super::stream_ffi::set_viewer_audio_volume(viewer, gain) };
        log::info!("Stream volume {gain:.2} (applied: {applied})");
    }
}

/// The view of the watched stream: the running viewer's, or the decoder's of
/// a watch whose SFU join has not finished. `StreamWatching` comes when the
/// decoder is up, before the join, so a value the card sends on it finds the
/// decoder's view.
fn stream_view_of(
    viewer: Option<*mut mello_sys::MelloStreamView>,
    decoder: Option<(usize, usize)>,
) -> Option<*mut mello_sys::MelloStreamView> {
    viewer.filter(|v| !v.is_null()).or_else(|| {
        decoder
            .map(|(view, _)| view as *mut mello_sys::MelloStreamView)
            .filter(|v| !v.is_null())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The running viewer wins; before the join the decoder's view takes
    /// the value; without either nothing is called.
    #[test]
    fn the_volume_goes_to_the_viewer_or_to_the_decoder_of_a_starting_watch() {
        let viewer = 0x1000usize as *mut mello_sys::MelloStreamView;
        let decoder = (0x2000usize, 0x3000usize);
        assert_eq!(stream_view_of(Some(viewer), Some(decoder)), Some(viewer));
        assert_eq!(
            stream_view_of(None, Some(decoder)),
            Some(0x2000usize as *mut mello_sys::MelloStreamView)
        );
        assert_eq!(stream_view_of(None, None), None);
        assert_eq!(stream_view_of(Some(std::ptr::null_mut()), None), None);
    }
}
