use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Instant;

use crate::stream::congestion::{CongestionSample, ViewerCongestionController};
use crate::stream::rtp_peer::RtpPeerError;

use super::FrameSlot;
use super::NativeFrameSlot;
use super::FRAME_STATE_READY;

pub(super) struct FrameCallbackData {
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub frame_slot: FrameSlot,
    pub native_frame_slot: NativeFrameSlot,
    pub frame_consumed: Arc<std::sync::atomic::AtomicBool>,
    pub frame_lifecycle: Arc<std::sync::atomic::AtomicU8>,
    pub surface_frame_seq: Arc<std::sync::atomic::AtomicU64>,
    /// One count per frame a frame callback delivered (native or CPU). A
    /// present that leaves it unchanged delivered no new frame.
    pub delivered_frames: Arc<std::sync::atomic::AtomicU64>,
}

/// State for the viewer-side streaming pipeline.
///
/// Streaming-quality stage 3: libmello runs the viewer by itself
/// (`mello_stream_viewer_attach_peer`). The receive timeline releases each
/// frame, the decode thread decodes it, and the present thread calls the
/// frame callbacks at the frame's render time. The stream tick only reads
/// stats, follows the pause state and the members, and keeps the freeze
/// clock.
pub(super) struct ViewerState {
    /// The C++ viewer pipeline handle. None until the decoder is initialized.
    pub viewer: Option<*mut mello_sys::MelloStreamView>,
    /// The SFU connection of the viewer.
    pub sfu_connection: Option<Arc<crate::transport::SfuConnection>>,
    pub host_id: String,
    /// The other members of the SFU session (host and viewers).
    pub members: crate::stream::StreamMembers,
    pub _frame_cb_data: *mut FrameCallbackData,
    /// Latest pause state from the reliable control channel (host tab-out).
    /// Written by `stream_control_data_callback`, polled by `stream_tick`.
    pub pause_slot: Arc<PauseSlot>,
    /// Last pause state reported to the UI. Compared against the slot so
    /// `Event::StreamPaused` fires on transitions only.
    pub last_reported_paused: bool,
    /// Last pause reason reported to the UI, as the short code. Compared
    /// alongside the flag so the card copy follows a reason change that
    /// arrives while already paused (loading behind an alt-tab).
    pub last_reported_pause_reason: String,
    /// Opaque slot pointer handed to libmello (`Arc::into_raw`). Freed in
    /// `Drop` via `unregister_pause_callback`.
    pub _pause_cb_data: *mut std::ffi::c_void,
    /// Non-owning peer the pause callback is registered on: the shared SFU
    /// connection's peer (kept alive by our `sfu_connection` Arc through
    /// `Drop`, so unregister is safe there).
    pub pause_cb_peer: *mut mello_sys::MelloPeerConnection,
    /// New frames the present thread showed (the frame callbacks ran).
    pub frames_presented: u64,
    pub stream_tick_count: u64,
    pub congestion: ViewerCongestionController,
    pub debug_last_emit: Instant,
    pub debug_last_tick_count: u64,
    pub debug_last_bytes: u64,
    pub debug_last_frames_presented: u64,
    pub debug_last_frames_decoded: u64,
    pub audio_packets_received: u64,
    /// Freeze accounting. Every other viewer metric is a proxy for what the user
    /// perceives; a freeze is the thing itself — a visible stall in the picture.
    /// A gap is counted once when it crosses the threshold, then extended, so a
    /// single 4s stall reports as one freeze of 4000ms rather than many.
    pub last_new_frame_at: Instant,
    pub freeze_count: u64,
    pub freeze_ms_total: u64,
    pub in_freeze: bool,
    /// How much of the current gap is already in `freeze_ms_total`, so a freeze
    /// spanning many ticks accrues once rather than per tick.
    pub freeze_accounted_ms: u64,
    /// Baselines for the 10s SFU report, kept separate from the 1s debug-event
    /// baselines so the two cadences do not consume each other's deltas.
    pub stats_last_emit: Instant,
    pub stats_last_frames_presented: u64,
    pub stats_last_freeze_count: u64,
    pub stats_last_freeze_ms: u64,
    pub stats_last_frames_decoded: u64,
    /// Shared with the frame callbacks (`FrameCallbackData::delivered_frames`).
    pub delivered_frames: Arc<std::sync::atomic::AtomicU64>,
    /// `delivered_frames` at the last tick.
    pub last_delivered_frames: u64,
    /// The game audio playout is muted for the host's pause
    /// (`mello_stream_viewer_set_audio_muted`).
    pub audio_muted: bool,
}

/// A present gap longer than this counts as a visible freeze. Two 60fps frame
/// intervals (33ms) is normal jitter; 150ms is unambiguously perceptible and
/// still well under the 600ms AU hard-age cap, so it fires before the receiver
/// gives up on an access unit rather than after.
pub(super) const FREEZE_THRESHOLD_MS: u64 = 150;

/// The mute call the viewer makes for the host's pause state: `Some(true)`
/// when the stream pauses, `Some(false)` when it resumes, `None` when the
/// playout already matches.
pub(super) fn pause_audio_mute_change(paused: bool, audio_muted: &mut bool) -> Option<bool> {
    if paused == *audio_muted {
        return None;
    }
    *audio_muted = paused;
    Some(paused)
}

/// New frames the present thread showed since the last tick: the frame
/// callbacks count each one (`delivered_frames`). A tick with none leaves the
/// freeze clock running; a repeat of an unchanged frame is never counted.
pub(super) fn new_frames_since(delivered: u64, last_delivered: &mut u64) -> u64 {
    let new = delivered.saturating_sub(*last_delivered);
    *last_delivered = delivered;
    new
}

/// One stream tick of the freeze clock. `held` is true while the host holds
/// the picture on purpose: paused, or idle (the capture delivers no new
/// frames and the 2 fps keepalive gives 500 ms gaps). A held picture is not
/// a stall: the clock restarts and nothing accrues. Returns true when the
/// caller must restart its last-frame time.
pub(super) fn freeze_clock_tick(
    held: bool,
    first_frame_seen: bool,
    gap_ms: u64,
    in_freeze: &mut bool,
    freeze_count: &mut u64,
    freeze_ms_total: &mut u64,
    accounted_ms: &mut u64,
) -> bool {
    if held {
        *in_freeze = false;
        *accounted_ms = 0;
        return true;
    }
    if first_frame_seen {
        note_present_gap(
            gap_ms,
            in_freeze,
            freeze_count,
            freeze_ms_total,
            accounted_ms,
        );
    }
    false
}

/// Fold a present-loop observation into the freeze counters.
///
/// Split out from the tick so it is testable without a decoder: the tick has raw
/// pointers and a live pipeline, this has arithmetic.
pub(super) fn note_present_gap(
    gap_ms: u64,
    in_freeze: &mut bool,
    freeze_count: &mut u64,
    freeze_ms_total: &mut u64,
    accounted_ms: &mut u64,
) {
    if gap_ms < FREEZE_THRESHOLD_MS {
        return;
    }
    if !*in_freeze {
        *in_freeze = true;
        *freeze_count = freeze_count.saturating_add(1);
        *accounted_ms = 0;
    }
    // Only the newly-elapsed portion, so a long freeze observed over many ticks
    // is not counted once per tick.
    let unaccounted = gap_ms.saturating_sub(*accounted_ms);
    *freeze_ms_total = freeze_ms_total.saturating_add(unaccounted);
    *accounted_ms = gap_ms;
}

unsafe impl Send for ViewerState {}
unsafe impl Sync for ViewerState {}

impl Drop for ViewerState {
    fn drop(&mut self) {
        // The viewer's pause callback goes first, on this thread. It touches the
        // peer: the shared SFU connection outlives this viewer through its own
        // Arc, so the peer is still valid right here.
        unsafe {
            if !self._pause_cb_data.is_null() {
                if let Some(peer) = NonNull::new(self.pause_cb_peer) {
                    unregister_pause_callback(peer, self._pause_cb_data);
                } else {
                    drop(Arc::from_raw(self._pause_cb_data as *const PauseSlot));
                }
                self._pause_cb_data = std::ptr::null_mut();
            }
            // After the callback is gone, so no packet in flight comes back:
            // the next stream must not start with this one's pointer.
            crate::stream::cursor::reset_viewer_cursor();
        }

        // Native viewer stop joins decode threads. It runs on the teardown
        // thread, bounded by its watchdog, never on the command loop. The
        // callback context is freed there only after the native viewer that
        // calls into it is gone.
        use crate::stream::teardown::TeardownPtr;
        let viewer = TeardownPtr(self.viewer.take().unwrap_or(std::ptr::null_mut()));
        let frame_cb = TeardownPtr(std::mem::replace(
            &mut self._frame_cb_data,
            std::ptr::null_mut(),
        ));
        if viewer.get().is_null() && frame_cb.get().is_null() {
            return;
        }
        crate::stream::teardown::spawn("stream_viewer", move |steps| unsafe {
            if !viewer.get().is_null() {
                steps.step("mello_stream_stop_viewer");
                mello_sys::mello_stream_stop_viewer(viewer.get());
            }
            steps.step("free callback contexts");
            if !frame_cb.get().is_null() {
                drop(Box::from_raw(frame_cb.get()));
            }
        });
        // SfuConnection is Arc-dropped automatically; leave() is called in handle_stop_watching
    }
}

/// Viewer pause slot: written by the reliable-control data callback on a
/// libdatachannel thread, polled by `stream_tick` on the client thread.
/// Latest-wins, like the native frame slot — pause state is idempotent.
pub(super) struct PauseSlot {
    paused: std::sync::atomic::AtomicU8,
    reason: std::sync::atomic::AtomicU8,
    /// The host's idle keepalive state (`[0x04, 0x04, idle]`).
    host_idle: std::sync::atomic::AtomicU8,
}

impl PauseSlot {
    pub fn new() -> Self {
        Self {
            paused: std::sync::atomic::AtomicU8::new(0),
            reason: std::sync::atomic::AtomicU8::new(0),
            host_idle: std::sync::atomic::AtomicU8::new(0),
        }
    }

    /// True while the host re-encodes the last picture (idle keepalive).
    pub fn is_host_idle(&self) -> bool {
        self.host_idle.load(std::sync::atomic::Ordering::Acquire) != 0
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(std::sync::atomic::Ordering::Acquire) != 0
    }

    pub fn reason(&self) -> crate::stream::pause::PauseReason {
        match self.reason.load(std::sync::atomic::Ordering::Acquire) {
            1 => crate::stream::pause::PauseReason::Minimized,
            2 => crate::stream::pause::PauseReason::WaitingForGame,
            3 => crate::stream::pause::PauseReason::Failed,
            _ => crate::stream::pause::PauseReason::Unknown,
        }
    }
}

/// Reliable-control data callback. Pause (`0x04/0x03`) and host idle
/// (`0x04/0x04`) messages go to the slot, cursor messages (`0x04/0x02`, the
/// hook's pointer) to the viewer pointer the presenter draws
/// (`stream::cursor`); everything else (stray binary pongs) is ignored fast so
/// the hot data path never blocks here.
pub(super) unsafe extern "C" fn stream_control_data_callback(
    user_data: *mut std::ffi::c_void,
    data: *const u8,
    size: i32,
    reliable: bool,
) {
    if !reliable || user_data.is_null() || data.is_null() || size <= 0 {
        return;
    }
    let bytes = std::slice::from_raw_parts(data, size as usize);
    if let Some(idle) = crate::stream::pause::parse_idle_message(bytes) {
        let slot = &*(user_data as *const PauseSlot);
        slot.host_idle
            .store(u8::from(idle), std::sync::atomic::Ordering::Release);
        return;
    }
    if let Some((paused, reason)) = crate::stream::pause::parse_pause_message_full(bytes) {
        let slot = &*(user_data as *const PauseSlot);
        slot.paused
            .store(u8::from(paused), std::sync::atomic::Ordering::Release);
        slot.reason
            .store(reason as u8, std::sync::atomic::Ordering::Release);
    } else if let Some(cursor) = crate::stream::cursor::parse_cursor_message(bytes) {
        crate::stream::cursor::apply_viewer_cursor(cursor);
    }
}

/// Register the pause callback on a viewer peer. Returns the opaque slot
/// pointer the caller must hand to [`unregister_pause_callback`] on teardown.
///
/// # Safety
/// `peer` must be a valid `MelloPeerConnection` that outlives the registration.
pub(super) unsafe fn register_pause_callback(
    peer: NonNull<mello_sys::MelloPeerConnection>,
    slot: &Arc<PauseSlot>,
) -> *mut std::ffi::c_void {
    let raw = Arc::into_raw(Arc::clone(slot)) as *mut std::ffi::c_void;
    mello_sys::mello_peer_set_data_callback(peer.as_ptr(), Some(stream_control_data_callback), raw);
    raw
}

/// Clear the callback and free the slot. Safe to call with a null `raw`
/// (no-op). The peer must still be valid: call it while the shared SFU
/// connection is alive.
pub(super) unsafe fn unregister_pause_callback(
    peer: NonNull<mello_sys::MelloPeerConnection>,
    raw: *mut std::ffi::c_void,
) {
    mello_sys::mello_peer_set_data_callback(peer.as_ptr(), None, std::ptr::null_mut());
    if !raw.is_null() {
        drop(Arc::from_raw(raw as *const PauseSlot));
    }
}

/// The payload of a complete RTP packet (RFC 3550): after the fixed header,
/// the CSRC list and a header extension, before padding. None when the packet
/// is not RTP version 2 or carries no payload.
pub(super) fn rtp_payload(packet: &[u8]) -> Option<&[u8]> {
    const FIXED: usize = 12;
    let first = *packet.first()?;
    if packet.len() < FIXED || first >> 6 != 2 {
        return None;
    }
    let mut header = FIXED + usize::from(first & 0x0F) * 4;
    if first & 0x10 != 0 {
        let ext = packet.get(header..header + 4)?;
        let words = usize::from(u16::from_be_bytes([ext[2], ext[3]]));
        header += 4 + words * 4;
    }
    let mut end = packet.len();
    if first & 0x20 != 0 {
        let pad = usize::from(*packet.last()?);
        if pad == 0 || header + pad > end {
            return None;
        }
        end -= pad;
    }
    if header >= end {
        return None;
    }
    packet.get(header..end)
}

/// Feed one audio packet of an SFU stream (a complete RTP packet,
/// `SfuEvent::AudioRtp`) to the viewer's audio decoder. Returns `true` when
/// libmello accepted it.
///
/// # Safety
/// `viewer` must be a valid `MelloStreamView` pointer, or null.
pub unsafe fn feed_viewer_audio_rtp(viewer: *mut mello_sys::MelloStreamView, rtp: &[u8]) -> bool {
    if viewer.is_null() {
        return false;
    }
    let Some(opus) = rtp_payload(rtp) else {
        return false;
    };
    unsafe {
        mello_sys::mello_stream_feed_audio_packet(
            viewer,
            opus.as_ptr(),
            i32::try_from(opus.len()).unwrap_or(0),
        ) == mello_sys::MelloResult_MELLO_OK
    }
}

/// Send-safe wrapper for MelloStreamHost pointer, used to pass across async boundaries.
pub(super) struct StreamHostHandle(pub *mut mello_sys::MelloStreamHost);
unsafe impl Send for StreamHostHandle {}

/// Give the viewer's video to libmello: the viewer pipeline takes released
/// frames from the receive timeline of `peer` and presents them on its own
/// threads. Returns `false` when libmello refused.
///
/// # Safety
/// `viewer` must be a live `MelloStreamView` and `peer` a live peer.
pub(super) unsafe fn attach_viewer_peer(
    viewer: *mut mello_sys::MelloStreamView,
    peer: NonNull<mello_sys::MelloPeerConnection>,
) -> bool {
    if viewer.is_null() {
        return false;
    }
    mello_sys::mello_stream_viewer_attach_peer(viewer, peer.as_ptr())
        == mello_sys::MelloResult_MELLO_OK
}

/// The viewer pipeline's statistics (stage 3 fields of `MelloStreamStats`).
///
/// # Safety
/// `viewer` must be a live `MelloStreamView`.
pub(super) unsafe fn viewer_stats(viewer: *mut mello_sys::MelloStreamView) -> ViewerTimelineStats {
    let mut raw: mello_sys::MelloStreamStats = std::mem::zeroed();
    mello_sys::mello_stream_viewer_get_stats(viewer, &mut raw);
    ViewerTimelineStats::from_native(&raw)
}

/// What the stream tick reads from the viewer pipeline once a second.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct ViewerTimelineStats {
    pub decoder: String,
    pub target_delay_ms: f32,
    pub jitter_ms: f32,
    pub decode_ms_p95: f32,
    pub frames_released: u64,
    pub frames_late: u64,
    pub frames_dropped_late: u64,
    pub frames_decoded: u64,
    pub frames_presented: u64,
    pub keyframe_requests: u64,
    pub loss_events: u64,
    pub waiting_for_keyframe: bool,
}

impl ViewerTimelineStats {
    fn from_native(raw: &mello_sys::MelloStreamStats) -> Self {
        let name: Vec<u8> = raw
            .decoder_name
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8)
            .collect();
        Self {
            decoder: String::from_utf8_lossy(&name).into_owned(),
            target_delay_ms: raw.target_delay_ms,
            jitter_ms: raw.jitter_ms,
            decode_ms_p95: raw.decode_ms_p95,
            frames_released: raw.frames_released,
            frames_late: raw.frames_late,
            frames_dropped_late: raw.frames_dropped_late,
            frames_decoded: raw.frames_decoded,
            frames_presented: raw.frames_presented,
            keyframe_requests: raw.keyframe_requests,
            loss_events: raw.loss_events,
            waiting_for_keyframe: raw.waiting_for_keyframe != 0,
        }
    }
}

/// One decimal, in f64 so the JSON shows 31.3 and not 31.299999237060547.
fn round1(v: f32) -> f64 {
    (f64::from(v) * 10.0).round() / 10.0
}

/// The abbreviated viewer fields of `stream_client_stats` (spec 12
/// section 11): the timeline and the decoder over the report interval.
pub(super) fn timeline_report_fields(
    stats: &ViewerTimelineStats,
    decoded_in_interval: u64,
    interval_secs: f32,
) -> serde_json::Value {
    serde_json::json!({
        "tgt_ms": round1(stats.target_delay_ms),
        "jit_ms": round1(stats.jitter_ms),
        "dec_p95": round1(stats.decode_ms_p95),
        "rel": stats.frames_released,
        "late": stats.frames_late,
        "drop_late": stats.frames_dropped_late,
        "kf_req": stats.keyframe_requests,
        "dec": stats.decoder,
        "dec_fps": round1(decoded_in_interval as f32 / interval_secs.max(0.001)),
    })
}

/// Apply the viewer congestion controller on an SFU connection.
pub(super) fn tick_viewer_congestion_sfu(
    vs: &mut ViewerState,
    conn: &crate::transport::SfuConnection,
) {
    if !conn.is_video_track_open() {
        return;
    }
    let stats = match conn.video_stats() {
        Ok(stats) => stats,
        Err(e) => {
            log::warn!("Failed to read SFU viewer RTP stats: {}", e);
            return;
        }
    };
    apply_congestion_output(vs, CongestionSample::from_native(&stats), |bps| {
        conn.set_video_receive_target(bps)
            .map_err(|_| RtpPeerError::TransportFailed)
    });
}

fn apply_congestion_output<F>(vs: &mut ViewerState, sample: CongestionSample, mut set_target: F)
where
    F: FnMut(u32) -> Result<(), RtpPeerError>,
{
    let Some(target_bps) = vs.congestion.tick(sample, Instant::now()) else {
        return;
    };
    match set_target(target_bps) {
        Ok(()) => {
            log::debug!("Viewer RTP receive target set to {} bps", target_bps);
        }
        Err(e) => log::warn!("Failed to set viewer receive target: {}", e),
    }
}

/// Log a structured snapshot of native RTP video stats for viewer diagnostics.
pub(super) fn log_viewer_native_stats(stats: &mello_sys::MelloRtpVideoStats) {
    log::info!(
        "Stream native stats rx_complete={} rx_emitted={} rx_incomplete={} gate_dropped={} nacks={} pli={} jitter={} buffered_aus={} ingress_packets={} ingress_bytes={} fec_recovered={} fec_unrecoverable={}",
        stats.rx_core_complete_access_units,
        stats.rx_core_emitted_access_units,
        stats.rx_core_incomplete_access_units,
        stats.rx_core_gate_dropped_access_units,
        stats.rx_core_nacks,
        stats.rx_core_pli_requests,
        stats.rx_core_interarrival_jitter,
        stats.rx_core_buffered_access_units,
        stats.rx_ingress_packets,
        stats.rx_ingress_bytes,
        stats.rx_fec_recovered,
        stats.rx_fec_unrecoverable,
    );
}

#[cfg_attr(target_os = "windows", allow(dead_code))]
pub(super) unsafe extern "C" fn on_viewer_frame(
    user_data: *mut std::ffi::c_void,
    rgba: *const u8,
    w: u32,
    h: u32,
    _ts: u64,
) {
    if user_data.is_null() || rgba.is_null() || w == 0 || h == 0 {
        return;
    }
    let data = &*(user_data as *const FrameCallbackData);
    let expected_len = (w * h) as usize * 4;
    let src = std::slice::from_raw_parts(rgba, expected_len);
    if let Ok(mut slot) = data.frame_slot.lock() {
        match slot.as_mut() {
            Some((ow, oh, buf)) if buf.len() == expected_len => {
                buf.copy_from_slice(src);
                *ow = w;
                *oh = h;
            }
            _ => {
                *slot = Some((w, h, src.to_vec()));
            }
        }
        data.frame_consumed
            .store(false, std::sync::atomic::Ordering::Release);
        data.frame_lifecycle
            .store(FRAME_STATE_READY, std::sync::atomic::Ordering::Release);
        data.delivered_frames
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Presents one native frame. Runs on libmello's present thread, at the
/// frame's render time; it must finish within a frame interval.
pub type NativePresentFn = Box<dyn FnMut(&super::NativeSurfaceFrame) -> bool + Send>;

static NATIVE_PRESENTER: std::sync::Mutex<Option<NativePresentFn>> = std::sync::Mutex::new(None);

/// Install the presenter of native frames (the client's DComp swap chain), or
/// remove it with `None`. Removal waits for a present in progress, so after it
/// returns nothing calls the old presenter.
pub fn set_native_presenter(presenter: Option<NativePresentFn>) {
    match NATIVE_PRESENTER.lock() {
        Ok(mut slot) => *slot = presenter,
        Err(poisoned) => *poisoned.into_inner() = presenter,
    }
}

pub(super) unsafe extern "C" fn on_viewer_native_frame(
    user_data: *mut std::ffi::c_void,
    shared_handle: *mut std::ffi::c_void,
    w: u32,
    h: u32,
    format: mello_sys::MelloNativeFrameFormat,
    uv_y_offset: u32,
    ts: u64,
) {
    if user_data.is_null() || shared_handle.is_null() || w == 0 || h == 0 {
        return;
    }
    let data = &*(user_data as *const FrameCallbackData);
    // libmello's present thread calls this at the frame's render time:
    // present now. The slot keeps the latest descriptor for diagnostics.
    #[allow(clippy::unnecessary_cast)]
    let frame = super::NativeSurfaceFrame {
        sequence: data
            .surface_frame_seq
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            .saturating_add(1),
        width: w,
        height: h,
        shared_handle: shared_handle as usize,
        format: format as u32,
        uv_y_offset,
        timestamp: ts,
    };
    if let Ok(mut presenter) = NATIVE_PRESENTER.lock() {
        if let Some(present) = presenter.as_mut() {
            present(&frame);
        }
    }
    if let Ok(mut slot) = data.native_frame_slot.lock() {
        *slot = Some(frame);
        data.frame_consumed
            .store(false, std::sync::atomic::Ordering::Release);
        data.frame_lifecycle
            .store(FRAME_STATE_READY, std::sync::atomic::Ordering::Release);
        data.delivered_frames
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pause message on the control channel mutes the game audio once; the
    /// resume unmutes it once. This is the state change that drives
    /// `mello_stream_viewer_set_audio_muted` in the viewer tick; libmello has
    /// no getter, so the C call itself is not observed here.
    #[test]
    fn the_pause_message_mutes_and_the_resume_unmutes_the_game_audio() {
        let slot = Arc::new(PauseSlot::new());
        let raw = Arc::as_ptr(&slot) as *mut std::ffi::c_void;
        let mut audio_muted = false;
        let mut deliver = |msg: &[u8]| {
            // SAFETY: the slot outlives the call.
            unsafe { stream_control_data_callback(raw, msg.as_ptr(), msg.len() as i32, true) };
            pause_audio_mute_change(slot.is_paused(), &mut audio_muted)
        };

        // Paused, as the host sends it to a viewer that joins late: v2 with
        // the reason, then v1.
        assert_eq!(deliver(&[0x04, 0x03, 0x01, 0x01]), Some(true));
        assert_eq!(deliver(&[0x04, 0x03, 0x01]), None, "already muted");
        assert_eq!(deliver(&[0x04, 0x04, 0x01]), None, "idle is not a pause");
        assert_eq!(deliver(&[0x04, 0x03, 0x00, 0x00]), Some(false));
        assert_eq!(deliver(&[0x04, 0x03, 0x00]), None, "already unmuted");
    }

    /// The host's idle message sets the slot's idle flag and leaves the
    /// pause state alone.
    #[test]
    fn the_idle_control_message_sets_the_host_idle_flag() {
        let slot = Arc::new(PauseSlot::new());
        let raw = Arc::as_ptr(&slot) as *mut std::ffi::c_void;
        for (msg, idle) in [([0x04u8, 0x04, 0x01], true), ([0x04, 0x04, 0x00], false)] {
            // SAFETY: the slot outlives the call.
            unsafe { stream_control_data_callback(raw, msg.as_ptr(), msg.len() as i32, true) };
            assert_eq!(slot.is_host_idle(), idle);
            assert!(!slot.is_paused());
        }
    }

    /// The 2 fps idle keepalive gives 500 ms present gaps. While the host is
    /// idle they are not freezes; the same gaps from a live host are.
    #[test]
    fn idle_keepalive_gaps_are_not_freezes() {
        for (held, want) in [(true, 0u64), (false, 1)] {
            let (mut in_freeze, mut count, mut total, mut accounted) = (false, 0u64, 0u64, 0u64);
            let restart = freeze_clock_tick(
                held,
                true,
                500,
                &mut in_freeze,
                &mut count,
                &mut total,
                &mut accounted,
            );
            assert_eq!(restart, held);
            assert_eq!(count, want, "held={held}");
        }
    }

    /// Only frames the present thread showed count: a tick without a frame
    /// callback has no new frame and does not end a freeze.
    #[test]
    fn only_frames_the_present_thread_showed_count() {
        let mut last = 0u64;
        assert_eq!(new_frames_since(1, &mut last), 1);
        assert_eq!(new_frames_since(1, &mut last), 0, "same frame again");
        assert_eq!(
            new_frames_since(4, &mut last),
            3,
            "three frames between ticks"
        );
    }

    /// The abbreviated stage 3 fields of `stream_client_stats`.
    #[test]
    fn the_report_carries_the_timeline_fields() {
        let stats = ViewerTimelineStats {
            decoder: "NVDEC".into(),
            target_delay_ms: 31.27,
            jitter_ms: 4.04,
            decode_ms_p95: 2.0,
            frames_released: 600,
            frames_late: 3,
            frames_dropped_late: 1,
            keyframe_requests: 0,
            ..Default::default()
        };
        let v = timeline_report_fields(&stats, 600, 10.0);
        assert_eq!(v["tgt_ms"], 31.3);
        assert_eq!(v["jit_ms"], 4.0);
        assert_eq!(v["dec"], "NVDEC");
        assert_eq!(v["dec_fps"], 60.0);
        assert_eq!(v["rel"], 600);
        assert_eq!(v["drop_late"], 1);
    }

    /// Normal inter-frame jitter must not register as a freeze, or the metric
    /// reports a stall on every healthy stream and becomes unusable.
    #[test]
    fn gaps_below_the_threshold_are_not_freezes() {
        let (mut in_freeze, mut count, mut total, mut accounted) = (false, 0u64, 0u64, 0u64);
        for gap in [0, 16, 33, 90, FREEZE_THRESHOLD_MS - 1] {
            note_present_gap(gap, &mut in_freeze, &mut count, &mut total, &mut accounted);
        }
        assert_eq!(count, 0);
        assert_eq!(total, 0);
        assert!(!in_freeze);
    }

    /// One stall observed across several ticks is one freeze, and its duration is
    /// the elapsed gap — not the sum of every observation of it.
    #[test]
    fn a_freeze_spanning_ticks_counts_once_and_accrues_once() {
        let (mut in_freeze, mut count, mut total, mut accounted) = (false, 0u64, 0u64, 0u64);
        note_present_gap(200, &mut in_freeze, &mut count, &mut total, &mut accounted);
        note_present_gap(400, &mut in_freeze, &mut count, &mut total, &mut accounted);
        note_present_gap(900, &mut in_freeze, &mut count, &mut total, &mut accounted);
        assert_eq!(count, 1, "one stall must be one freeze");
        assert_eq!(
            total, 900,
            "duration is the gap, not the sum of observations"
        );
        assert!(in_freeze);
    }

    /// Recovering and stalling again is two freezes; the second must not inherit
    /// the first one's accounted duration.
    #[test]
    fn separate_stalls_count_separately() {
        let (mut in_freeze, mut count, mut total, mut accounted) = (false, 0u64, 0u64, 0u64);
        note_present_gap(300, &mut in_freeze, &mut count, &mut total, &mut accounted);
        // Frame arrives: the present loop clears the gap state.
        in_freeze = false;
        accounted = 0;
        note_present_gap(500, &mut in_freeze, &mut count, &mut total, &mut accounted);
        assert_eq!(count, 2);
        assert_eq!(total, 800);
    }

    #[test]
    fn rtp_payload_skips_csrcs_extension_and_padding() {
        // Plain header.
        let plain = [0x80, 111, 0, 1, 0, 0, 0, 0, 0, 0, 0, 7, 0xAA, 0xBB];
        assert_eq!(rtp_payload(&plain), Some(&[0xAA, 0xBB][..]));
        // One CSRC, a one-word extension, two bytes of padding.
        let mut p = vec![0x80 | 0x20 | 0x10 | 0x01, 111, 0, 1, 0, 0, 0, 0, 0, 0, 0, 7];
        p.extend_from_slice(&[9, 9, 9, 9]); // CSRC
        p.extend_from_slice(&[0xBE, 0xDE, 0x00, 0x01, 1, 2, 3, 4]); // extension
        p.extend_from_slice(&[0xCC]); // payload
        p.extend_from_slice(&[0x00, 0x02]); // padding
        assert_eq!(rtp_payload(&p), Some(&[0xCC][..]));
        // Not RTP v2, too short, no payload.
        assert_eq!(rtp_payload(&[0x40; 14]), None);
        assert_eq!(rtp_payload(&plain[..11]), None);
        assert_eq!(rtp_payload(&plain[..12]), None);
    }

    #[test]
    fn rtp_audio_with_a_null_viewer_is_rejected() {
        let plain = [0x80, 111, 0, 1, 0, 0, 0, 0, 0, 0, 0, 7, 0xAA];
        assert!(!unsafe { feed_viewer_audio_rtp(std::ptr::null_mut(), &plain) });
    }
}
