use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Instant;

use crate::stream::congestion::{CongestionSample, ViewerCongestionController};
use crate::stream::rtp_peer::{ReceivedAccessUnit, RtpPeerError};

use super::FrameSlot;
use super::NativeFrameSlot;
use super::NativeSurfaceFrame;
use super::FRAME_STATE_READY;

/// Maximum native RTP access units polled per stream tick to avoid starvation.
pub(super) const MAX_AU_POLLS_PER_TICK: usize = 32;

/// Initial Annex-B receive buffer capacity; grown in place on small-buffer retry.
pub(super) const VIEWER_AU_RECV_BUF_INITIAL: usize = 256 * 1024;

/// Decode-queue depth above which the viewer backlog guard (spec 12-STREAMING
/// §7.7) drops incoming delta AUs instead of feeding them. Keyframes always feed.
pub(super) const DECODE_QUEUE_BACKLOG_THRESHOLD: i32 = 4;

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
    pub frames_presented: u64,
    pub stream_tick_count: u64,
    pub present_attempts: u64,
    pub present_forced_attempts: u64,
    pub present_skipped_unconsumed: u64,
    /// Complete access units fed to the decoder this session.
    pub transport_packets: u64,
    /// Annex-B bytes fed to the decoder this session.
    pub transport_bytes: u64,
    /// Times the AU receive buffer was grown after a small-buffer retry.
    pub au_buffer_grows: u64,
    pub au_poll_errors: u64,
    pub au_feed_failures: u64,
    /// Delta AUs dropped by the backlog guard (spec §7.7) this session.
    pub backlog_guard_drops: u64,
    pub congestion: ViewerCongestionController,
    pub debug_last_emit: Instant,
    pub debug_last_tick_count: u64,
    pub debug_last_present_attempts: u64,
    pub debug_last_present_forced_attempts: u64,
    pub debug_last_present_skipped_unconsumed: u64,
    pub debug_last_packets: u64,
    pub debug_last_bytes: u64,
    pub debug_last_frames_presented: u64,
    pub debug_last_backlog_guard_drops: u64,
    pub last_present_attempt: Instant,
    pub au_recv_buf: Vec<u8>,
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
    /// Shared with the frame callbacks (`FrameCallbackData::delivered_frames`).
    pub delivered_frames: Arc<std::sync::atomic::AtomicU64>,
    /// `delivered_frames` at the last present that counted as a frame.
    pub last_delivered_frames: u64,
}

/// A present gap longer than this counts as a visible freeze. Two 60fps frame
/// intervals (33ms) is normal jitter; 150ms is unambiguously perceptible and
/// still well under the 600ms AU hard-age cap, so it fires before the receiver
/// gives up on an access unit rather than after.
pub(super) const FREEZE_THRESHOLD_MS: u64 = 150;

/// True when a present delivered a new frame: the present returned true and
/// a frame callback ran since the last counted frame. A present of an
/// unchanged frame is not a frame and does not end a freeze.
pub(super) fn present_is_new_frame(
    presented: bool,
    delivered: u64,
    last_delivered: &mut u64,
) -> bool {
    if !presented || delivered == *last_delivered {
        return false;
    }
    *last_delivered = delivered;
    true
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

impl ViewerState {
    pub fn new_au_recv_buf() -> Vec<u8> {
        vec![0_u8; VIEWER_AU_RECV_BUF_INITIAL]
    }
}

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

/// Reliable-control data callback. Only pause (`0x04/0x03`) and host idle
/// (`0x04/0x04`) messages are handled; everything else (cursor future use, stray binary pongs) is
/// ignored fast so the hot data path never blocks here.
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

/// Outcome of polling and feeding viewer ingress for one stream tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ViewerIngressTick {
    pub access_units_fed: u32,
    pub bytes_fed: u64,
    pub buffer_grows: u32,
}

/// Feed one complete Annex-B access unit to the native decoder.
///
/// # Safety
/// `viewer` must be a valid `MelloStreamView` pointer.
pub(super) unsafe fn feed_access_unit_to_decoder(
    viewer: *mut mello_sys::MelloStreamView,
    annex_b: &[u8],
    is_idr: bool,
) -> bool {
    if viewer.is_null() || annex_b.is_empty() {
        return false;
    }
    mello_sys::mello_stream_feed_packet(
        viewer,
        annex_b.as_ptr(),
        i32::try_from(annex_b.len()).unwrap_or(i32::MAX),
        is_idr,
    )
}

/// Poll up to [`MAX_AU_POLLS_PER_TICK`] complete access units from an SFU viewer
/// connection and feed each to the decoder.
pub(super) fn poll_sfu_viewer_access_units(
    vs: &mut ViewerState,
    viewer: *mut mello_sys::MelloStreamView,
    conn: &crate::transport::SfuConnection,
) -> ViewerIngressTick {
    let mut tick = ViewerIngressTick {
        access_units_fed: 0,
        bytes_fed: 0,
        buffer_grows: 0,
    };

    for _ in 0..MAX_AU_POLLS_PER_TICK {
        let before_len = vs.au_recv_buf.len();
        let au = match conn.poll_received_access_unit(&mut vs.au_recv_buf) {
            Ok(Some(au)) => au,
            Ok(None) => break,
            Err(e) => {
                vs.au_poll_errors = vs.au_poll_errors.saturating_add(1);
                log::warn!("SFU viewer AU poll failed: {}", e);
                break;
            }
        };
        if vs.au_recv_buf.len() > before_len {
            vs.au_buffer_grows = vs.au_buffer_grows.saturating_add(1);
            tick.buffer_grows = tick.buffer_grows.saturating_add(1);
        }
        if feed_one_access_unit(vs, viewer, &au) {
            tick.access_units_fed = tick.access_units_fed.saturating_add(1);
            tick.bytes_fed = tick.bytes_fed.saturating_add(vs.au_recv_buf.len() as u64);
        }
    }

    tick
}

/// Backlog-guard drop decision (spec §7.7): shed delta AUs while the decode
/// queue is backlogged so a sustained network burst can't bury the decoder;
/// keyframes always feed so the reference chain can recover.
fn should_drop_for_backlog(decode_queue_depth: i32, is_keyframe: bool) -> bool {
    !is_keyframe && decode_queue_depth > DECODE_QUEUE_BACKLOG_THRESHOLD
}

fn feed_one_access_unit(
    vs: &mut ViewerState,
    viewer: *mut mello_sys::MelloStreamView,
    au: &ReceivedAccessUnit,
) -> bool {
    // No viewer→host keyframe-request channel exists yet, so the guard only
    // drops; the native receiver's automatic PLI remains the IDR trigger.
    let decode_queue_depth = unsafe { mello_sys::mello_stream_viewer_decode_queue_depth(viewer) };
    if should_drop_for_backlog(decode_queue_depth, au.is_idr) {
        vs.backlog_guard_drops = vs.backlog_guard_drops.saturating_add(1);
        return false;
    }
    let ok = unsafe { feed_access_unit_to_decoder(viewer, &vs.au_recv_buf, au.is_idr) };
    if ok {
        vs.transport_packets = vs.transport_packets.saturating_add(1);
        vs.transport_bytes = vs
            .transport_bytes
            .saturating_add(vs.au_recv_buf.len() as u64);
    } else {
        vs.au_feed_failures = vs.au_feed_failures.saturating_add(1);
        if au.is_idr {
            log::warn!(
                "feed_packet failed for IDR access unit ({} bytes)",
                vs.au_recv_buf.len()
            );
        }
    }
    ok
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
    if let Ok(mut slot) = data.native_frame_slot.lock() {
        // MelloNativeFrameFormat is u32 on macOS (clang) but i32 on Windows (MSVC);
        // cast is necessary for Windows, no-op on macOS.
        #[allow(clippy::unnecessary_cast)]
        let fmt = format as u32;
        let sequence = data
            .surface_frame_seq
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            .saturating_add(1);
        *slot = Some(NativeSurfaceFrame {
            sequence,
            width: w,
            height: h,
            shared_handle: shared_handle as usize,
            format: fmt,
            uv_y_offset,
            timestamp: ts,
        });
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

    #[test]
    fn max_au_polls_per_tick_is_bounded() {
        assert_eq!(MAX_AU_POLLS_PER_TICK, 32);
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

    /// A present that delivered no new frame (no frame callback ran) is not
    /// a frame: it must not reset the freeze clock or count in present_fps.
    #[test]
    fn a_present_of_an_unchanged_frame_is_not_a_frame() {
        let mut last = 0u64;
        assert!(present_is_new_frame(true, 1, &mut last));
        assert!(
            !present_is_new_frame(true, 1, &mut last),
            "same frame again"
        );
        assert!(!present_is_new_frame(false, 2, &mut last), "no present");
        assert!(present_is_new_frame(true, 2, &mut last));
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
    fn viewer_au_recv_buf_has_sane_initial_capacity() {
        let buf = ViewerState::new_au_recv_buf();
        assert_eq!(buf.len(), VIEWER_AU_RECV_BUF_INITIAL);
        assert!(buf.len() >= 64 * 1024);
    }

    #[test]
    fn backlog_guard_drops_delta_above_threshold() {
        assert!(should_drop_for_backlog(
            DECODE_QUEUE_BACKLOG_THRESHOLD + 1,
            false
        ));
        assert!(should_drop_for_backlog(i32::MAX, false));
    }

    #[test]
    fn backlog_guard_feeds_delta_at_or_below_threshold() {
        assert!(!should_drop_for_backlog(
            DECODE_QUEUE_BACKLOG_THRESHOLD,
            false
        ));
        assert!(!should_drop_for_backlog(0, false));
        assert!(!should_drop_for_backlog(-1, false));
    }

    #[test]
    fn backlog_guard_never_drops_keyframes() {
        assert!(!should_drop_for_backlog(
            DECODE_QUEUE_BACKLOG_THRESHOLD + 1,
            true
        ));
        assert!(!should_drop_for_backlog(i32::MAX, true));
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

    #[test]
    fn feed_access_unit_to_decoder_rejects_null_viewer() {
        let ok = unsafe { feed_access_unit_to_decoder(std::ptr::null_mut(), b"annexb", true) };
        assert!(!ok);
    }

    #[test]
    fn feed_access_unit_to_decoder_rejects_empty_payload() {
        let viewer = std::ptr::dangling_mut::<mello_sys::MelloStreamView>();
        let ok = unsafe { feed_access_unit_to_decoder(viewer, &[], true) };
        assert!(!ok);
    }
}
