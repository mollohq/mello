//! The game clip player in mello-core (plans/GAME-CLIPS.md 6.2): the clip's
//! bytes (`range_reader`), libmello's player (`mello_clip_player_*`), and its
//! state for the UI. The pictures go to the native presenter, as a watched
//! stream's do, so a clip plays only while no stream is watched.

pub mod range_reader;

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use range_reader::{ClipBytes, LocalClip, RangeReader, READ_ERROR};

/// The player's state, as the UI shows it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlaybackState {
    pub position_ms: u64,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub buffering: bool,
    pub paused: bool,
    pub ended: bool,
    pub failed: bool,
}

/// The libmello context, for the thread that opens a clip.
pub(crate) struct SendCtx(pub *mut mello_sys::MelloContext);
unsafe impl Send for SendCtx {}

/// One clip that plays. Dropping it stops the player, which joins threads:
/// drop it on the teardown thread, never on the command loop.
pub(crate) struct ClipPlayback {
    player: *mut mello_sys::MelloClipPlayer,
    /// The read callback's user data, freed after the player stopped.
    bytes: *mut Arc<dyn ClipBytes>,
    pub source: String,
    sent: Option<PlaybackState>,
}

// The client loop owns it, and the teardown thread drops it. libmello's
// player takes its own locks in each call (state, seek, pause), so a shared
// reference across threads is safe too.
unsafe impl Send for ClipPlayback {}
unsafe impl Sync for ClipPlayback {}

/// Where a remote clip's ranges wait while it plays.
fn cache_dir() -> PathBuf {
    std::env::temp_dir().join("mello_clips").join("play")
}

impl ClipPlayback {
    /// Opens `source` (an `http(s)` URL, or the clipper's own file) and
    /// starts libmello's player. Blocking: a URL costs one request.
    pub(crate) fn start(ctx: SendCtx, source: &str) -> Result<Self, String> {
        let bytes: Arc<dyn ClipBytes> =
            if source.starts_with("http://") || source.starts_with("https://") {
                Arc::new(RangeReader::open(source, &cache_dir())?)
            } else {
                Arc::new(LocalClip::open(std::path::Path::new(source))?)
            };
        let length = bytes.length();
        if length == 0 {
            return Err(format!("{source}: an empty clip"));
        }
        let user = Box::into_raw(Box::new(bytes));
        let config = mello_sys::MelloClipPlayerConfig {
            length,
            read: Some(read_clip),
            user_data: user as *mut c_void,
        };
        let player = unsafe { mello_sys::mello_clip_player_start(ctx.0, &config) };
        if player.is_null() {
            drop(unsafe { Box::from_raw(user) });
            return Err("no clip player on this platform".into());
        }
        unsafe {
            mello_sys::mello_clip_player_set_native_frame_callback(
                player,
                Some(on_clip_native_frame),
                std::ptr::null_mut(),
            );
        }
        log::info!("[clip-play] playing {source} ({length} bytes)");
        Ok(Self {
            player,
            bytes: user,
            source: source.to_string(),
            sent: None,
        })
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        unsafe { mello_sys::mello_clip_player_set_paused(self.player, paused) };
    }

    pub(crate) fn seek(&self, position_ms: u64) {
        unsafe { mello_sys::mello_clip_player_seek(self.player, position_ms) };
    }

    pub(crate) fn state(&self) -> PlaybackState {
        let mut s = mello_sys::MelloClipPlayerState {
            position_ms: 0,
            duration_ms: 0,
            width: 0,
            height: 0,
            buffering: false,
            paused: false,
            ended: false,
            failed: false,
        };
        unsafe { mello_sys::mello_clip_player_get_state(self.player, &mut s) };
        PlaybackState {
            position_ms: s.position_ms,
            duration_ms: s.duration_ms,
            width: s.width,
            height: s.height,
            buffering: s.buffering,
            paused: s.paused,
            ended: s.ended,
            failed: s.failed,
        }
    }

    /// The state when the UI must hear it: a flag changed, or the position
    /// moved by 100 ms or more.
    pub(crate) fn poll_changed(&mut self) -> Option<PlaybackState> {
        let now = self.state();
        if !worth_sending(self.sent, now) {
            return None;
        }
        self.sent = Some(now);
        Some(now)
    }
}

pub(crate) fn worth_sending(sent: Option<PlaybackState>, now: PlaybackState) -> bool {
    match sent {
        None => true,
        Some(s) => {
            (
                s.buffering,
                s.paused,
                s.ended,
                s.failed,
                s.width,
                s.height,
                s.duration_ms,
            ) != (
                now.buffering,
                now.paused,
                now.ended,
                now.failed,
                now.width,
                now.height,
                now.duration_ms,
            ) || s.position_ms.abs_diff(now.position_ms) >= 100
        }
    }
}

impl Drop for ClipPlayback {
    fn drop(&mut self) {
        // The player calls `read_clip` until its stop returns.
        unsafe { mello_sys::mello_clip_player_stop(self.player) };
        drop(unsafe { Box::from_raw(self.bytes) });
        log::info!("[clip-play] stopped {}", self.source);
    }
}

/// libmello's read callback: Media Foundation threads.
unsafe extern "C" fn read_clip(user: *mut c_void, offset: u64, buf: *mut u8, len: u32) -> i64 {
    if user.is_null() || buf.is_null() {
        return READ_ERROR;
    }
    let bytes = &*(user as *const Arc<dyn ClipBytes>);
    let slice = std::slice::from_raw_parts_mut(buf, len as usize);
    bytes.read(offset, slice)
}

static CLIP_FRAME_SEQ: AtomicU64 = AtomicU64::new(0);

/// The player's present thread: the picture goes to the native presenter.
unsafe extern "C" fn on_clip_native_frame(
    _user: *mut c_void,
    shared_handle: *mut c_void,
    w: u32,
    h: u32,
    format: mello_sys::MelloNativeFrameFormat,
    uv_y_offset: u32,
    ts: u64,
) {
    if shared_handle.is_null() || w == 0 || h == 0 {
        return;
    }
    #[allow(clippy::unnecessary_cast)]
    let frame = crate::client::NativeSurfaceFrame {
        sequence: CLIP_FRAME_SEQ.fetch_add(1, Ordering::AcqRel) + 1,
        width: w,
        height: h,
        shared_handle: shared_handle as usize,
        format: format as u32,
        uv_y_offset,
        timestamp: ts,
    };
    crate::client::present_native_frame(&frame);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ui_hears_flags_at_once_and_the_position_in_100_ms_steps() {
        let s = PlaybackState {
            position_ms: 1000,
            duration_ms: 30_000,
            ..Default::default()
        };
        assert!(worth_sending(None, s));
        let near = PlaybackState {
            position_ms: 1060,
            ..s
        };
        assert!(!worth_sending(Some(s), near));
        let far = PlaybackState {
            position_ms: 1100,
            ..s
        };
        assert!(worth_sending(Some(s), far));
        let buffering = PlaybackState {
            buffering: true,
            ..near
        };
        assert!(worth_sending(Some(s), buffering));
    }
}
