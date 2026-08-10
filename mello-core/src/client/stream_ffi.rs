use std::collections::VecDeque;
use std::ffi::CStr;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Instant;

use crate::stream::congestion::{CongestionSample, ViewerCongestionController};
use crate::stream::rtp_peer::{self, ReceivedAccessUnit, RtpPeerError};
use crate::voice::{SignalEnvelope, SignalMessage, SignalPurpose};

use super::FrameSlot;
use super::NativeFrameSlot;
use super::NativeSurfaceFrame;
use super::FRAME_STATE_READY;

/// Maximum native RTP access units polled per stream tick to avoid starvation.
pub(super) const MAX_AU_POLLS_PER_TICK: usize = 32;

/// Initial Annex-B receive buffer capacity; grown in place on small-buffer retry.
pub(super) const VIEWER_AU_RECV_BUF_INITIAL: usize = 256 * 1024;
const MAX_PENDING_STREAM_DISCONNECTS: usize = 64;

/// Decode-queue depth above which the viewer backlog guard (spec 12-STREAMING
/// §7.7) drops incoming delta AUs instead of feeding them. Keyframes always feed.
pub(super) const DECODE_QUEUE_BACKLOG_THRESHOLD: i32 = 4;

/// Bytes libmello prepends to each incoming audio packet: a little-endian RTP
/// sequence number followed by two reserved zero bytes. Added by
/// `PeerConnectionImpl::wire_incoming_audio_track_callbacks` once the RTP
/// header has been stripped.
const AUDIO_SEQ_HEADER_LEN: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StreamPeerDisconnect {
    pub peer_id: String,
    pub callback_data: usize,
}

pub(super) struct FrameCallbackData {
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub frame_slot: FrameSlot,
    pub native_frame_slot: NativeFrameSlot,
    pub frame_consumed: Arc<std::sync::atomic::AtomicBool>,
    pub frame_lifecycle: Arc<std::sync::atomic::AtomicU8>,
    pub surface_frame_seq: Arc<std::sync::atomic::AtomicU64>,
}

/// State for the viewer-side streaming pipeline.
pub(super) struct ViewerState {
    /// The C++ viewer pipeline handle. None until the host's Answer arrives
    /// with the actual encode resolution so we can initialize the decoder correctly.
    pub viewer: Option<*mut mello_sys::MelloStreamView>,
    /// P2P peer to host (only in P2P mode).
    pub peer: *mut mello_sys::MelloPeerConnection,
    /// SFU connection (only in SFU mode).
    pub sfu_connection: Option<Arc<crate::transport::SfuConnection>>,
    /// "sfu" or "p2p"
    pub mode: String,
    pub host_id: String,
    pub _frame_cb_data: *mut FrameCallbackData,
    pub _ice_cb_data: *mut StreamIceCallbackData,
    pub _audio_cb_data: *mut StreamAudioCallbackData,
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
}

/// A present gap longer than this counts as a visible freeze. Two 60fps frame
/// intervals (33ms) is normal jitter; 150ms is unambiguously perceptible and
/// still well under the 600ms AU hard-age cap, so it fires before the receiver
/// gives up on an access unit rather than after.
pub(super) const FREEZE_THRESHOLD_MS: u64 = 150;

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
        unsafe {
            if let Some(v) = self.viewer {
                if !v.is_null() {
                    mello_sys::mello_stream_stop_viewer(v);
                }
            }
            if !self.peer.is_null() {
                mello_sys::mello_peer_destroy(self.peer);
            }
            if !self._frame_cb_data.is_null() {
                drop(Box::from_raw(self._frame_cb_data));
            }
            if !self._ice_cb_data.is_null() {
                drop(Box::from_raw(self._ice_cb_data));
            }
            if !self._audio_cb_data.is_null() {
                drop(Box::from_raw(self._audio_cb_data));
            }
        }
        // SfuConnection is Arc-dropped automatically; leave() is called in handle_stop_watching
    }
}

pub(super) struct StreamIceCallbackData {
    pub peer_id: String,
    pub send_queue: std::sync::Arc<std::sync::Mutex<Vec<(String, SignalEnvelope)>>>,
    pub disconnect_queue: Arc<std::sync::Mutex<VecDeque<StreamPeerDisconnect>>>,
    /// ICE candidates gathered before the offer/answer is queued.
    /// Once `flushed` is true, new candidates go straight to `send_queue`.
    pub pending: std::sync::Mutex<Vec<SignalEnvelope>>,
    pub flushed: std::sync::atomic::AtomicBool,
}

pub(super) struct StreamAudioCallbackData {
    pub viewer_slot: std::sync::Mutex<Option<*mut mello_sys::MelloStreamView>>,
    pub packets_received: std::sync::atomic::AtomicU64,
}

/// Feed one Opus packet to the native viewer for playout.
///
/// # Safety
/// `viewer` must be a valid `MelloStreamView` pointer, or null.
///
/// Marked `unsafe` for the same reason as [`feed_access_unit_to_decoder`]: it
/// dereferences a caller-supplied raw pointer, so a safe signature would let
/// safe code cause undefined behaviour by passing a dangling pointer.
pub unsafe fn feed_viewer_audio_packet(
    viewer: *mut mello_sys::MelloStreamView,
    data: &[u8],
) -> bool {
    if viewer.is_null() || data.is_empty() {
        return false;
    }
    // Strip the 4-byte LE sequence header that
    // PeerConnectionImpl::wire_incoming_audio_track_callbacks prepends after
    // removing the RTP header: [seq_lo, seq_hi, 0, 0, opus...]. The sequence
    // number is unused here — RTP already handles ordering and loss. Same
    // convention as the voice path (voice/mod.rs, "strip the 4-byte LE
    // sequence header").
    //
    // A packet of 4 bytes or fewer carries a header and no payload, so there
    // is nothing to decode.
    if data.len() <= AUDIO_SEQ_HEADER_LEN {
        return false;
    }
    let opus = &data[AUDIO_SEQ_HEADER_LEN..];
    unsafe {
        mello_sys::mello_stream_feed_audio_packet(
            viewer,
            opus.as_ptr(),
            i32::try_from(opus.len()).unwrap_or(0),
        ) == mello_sys::MelloResult_MELLO_OK
    }
}

pub(super) unsafe extern "C" fn stream_audio_track_callback(
    user_data: *mut std::ffi::c_void,
    _sender_id: *const std::ffi::c_char,
    data: *const u8,
    size: i32,
) {
    if user_data.is_null() || data.is_null() || size <= 0 {
        return;
    }
    let cb_data = &*(user_data as *const StreamAudioCallbackData);
    let pkt = std::slice::from_raw_parts(data, size as usize);
    let viewer = cb_data.viewer_slot.lock().ok().and_then(|slot| *slot);
    if let Some(viewer) = viewer {
        if feed_viewer_audio_packet(viewer, pkt) {
            cb_data
                .packets_received
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

pub(super) struct StreamHostPeer {
    pub peer: *mut mello_sys::MelloPeerConnection,
    pub ice_cb_data: *mut StreamIceCallbackData,
}

unsafe impl Send for StreamHostPeer {}
unsafe impl Sync for StreamHostPeer {}

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

/// Poll up to [`MAX_AU_POLLS_PER_TICK`] complete access units from `peer` and feed
/// each to the decoder. Never uses unreliable DataChannel media APIs.
pub(super) fn poll_p2p_viewer_access_units(
    vs: &mut ViewerState,
    viewer: *mut mello_sys::MelloStreamView,
    peer: NonNull<mello_sys::MelloPeerConnection>,
) -> ViewerIngressTick {
    let mut tick = ViewerIngressTick {
        access_units_fed: 0,
        bytes_fed: 0,
        buffer_grows: 0,
    };

    for _ in 0..MAX_AU_POLLS_PER_TICK {
        let before_len = vs.au_recv_buf.len();
        let au = match rtp_peer::poll_received_access_unit(peer, &mut vs.au_recv_buf) {
            Ok(Some(au)) => au,
            Ok(None) => break,
            Err(RtpPeerError::RecvFailed) => {
                vs.au_poll_errors = vs.au_poll_errors.saturating_add(1);
                break;
            }
            Err(e) => {
                vs.au_poll_errors = vs.au_poll_errors.saturating_add(1);
                log::warn!("Viewer AU poll failed: {}", e);
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

/// Apply the viewer congestion controller and emit REMB when the target changes.
pub(super) fn tick_viewer_congestion_p2p(
    vs: &mut ViewerState,
    peer: NonNull<mello_sys::MelloPeerConnection>,
) {
    if !rtp_peer::video_is_open(peer) {
        return;
    }
    let mut stats = unsafe { std::mem::zeroed::<mello_sys::MelloRtpVideoStats>() };
    unsafe { mello_sys::mello_peer_video_get_stats(peer.as_ptr(), &mut stats) };
    apply_congestion_output(vs, CongestionSample::from_native(&stats), |bps| {
        rtp_peer::set_receive_target(peer, bps)
    });
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
            log::debug!(
                "Viewer RTP receive target set to {} bps (mode={})",
                target_bps,
                vs.mode
            );
        }
        Err(e) => log::warn!("Failed to set viewer receive target: {}", e),
    }
}

/// Log a structured snapshot of native RTP video stats for viewer diagnostics.
pub(super) fn log_viewer_native_stats(mode: &str, stats: &mello_sys::MelloRtpVideoStats) {
    log::info!(
        "Stream native stats mode={} rx_complete={} rx_emitted={} rx_incomplete={} gate_dropped={} nacks={} pli={} jitter={} buffered_aus={} ingress_packets={} ingress_bytes={} fec_recovered={} fec_unrecoverable={}",
        mode,
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
    }
}

pub(super) unsafe extern "C" fn stream_ice_callback(
    user_data: *mut std::ffi::c_void,
    candidate: *const mello_sys::MelloIceCandidate,
) {
    if user_data.is_null() || candidate.is_null() {
        return;
    }
    let data = &*(user_data as *const StreamIceCallbackData);
    let c = &*candidate;
    let cand = CStr::from_ptr(c.candidate).to_string_lossy().into_owned();
    let mid = CStr::from_ptr(c.sdp_mid).to_string_lossy().into_owned();
    let idx = c.sdp_mline_index;
    log::debug!(
        "Stream ICE candidate gathered for peer {}: {}",
        data.peer_id,
        cand
    );

    let envelope = SignalEnvelope {
        purpose: SignalPurpose::Stream,
        stream_width: None,
        stream_height: None,
        stream_bitrate_kbps: None,
        message: SignalMessage::IceCandidate {
            candidate: cand,
            sdp_mid: mid,
            sdp_mline_index: idx,
        },
    };

    if data.flushed.load(std::sync::atomic::Ordering::Acquire) {
        // Offer/answer already queued — send directly
        if let Ok(mut q) = data.send_queue.lock() {
            q.push((data.peer_id.clone(), envelope));
        }
    } else {
        // Buffer until offer/answer is queued
        if let Ok(mut buf) = data.pending.lock() {
            buf.push(envelope);
        }
    }
}

pub(super) unsafe extern "C" fn stream_state_callback(
    user_data: *mut std::ffi::c_void,
    state: i32,
) {
    if user_data.is_null() {
        return;
    }
    let data = &*(user_data as *const StreamIceCallbackData);
    let queued_disconnect = enqueue_terminal_disconnect(data, state);
    let label = match state {
        0 => "New",
        1 => "Connecting",
        2 => "Connected",
        3 => "Disconnected",
        4 => "Failed",
        5 => "Closed",
        _ => "Unknown",
    };
    if state == 4 {
        log::error!(
            "Stream peer {} ICE state: {} — NAT traversal failed",
            data.peer_id,
            label
        );
    } else if state == 2 {
        log::info!("Stream peer {} ICE state: {}", data.peer_id, label);
    } else {
        log::debug!("Stream peer {} ICE state: {}", data.peer_id, label);
    }
    if queued_disconnect {
        log::info!(
            "Queued terminal stream peer cleanup: peer={} state={}",
            data.peer_id,
            label
        );
    }
}

fn enqueue_terminal_disconnect(data: &StreamIceCallbackData, state: i32) -> bool {
    if !matches!(state, 3..=5) {
        return false;
    }
    let Ok(mut queue) = data.disconnect_queue.lock() else {
        return false;
    };
    let notice = StreamPeerDisconnect {
        peer_id: data.peer_id.clone(),
        callback_data: std::ptr::from_ref(data) as usize,
    };
    if queue.iter().any(|queued| queued == &notice) {
        return false;
    }
    if queue.len() >= MAX_PENDING_STREAM_DISCONNECTS {
        log::warn!(
            "Stream disconnect queue full; dropping terminal callback for {}",
            data.peer_id
        );
        return false;
    }
    queue.push_back(notice);
    true
}

/// Flush buffered ICE candidates from a `StreamIceCallbackData` into the main
/// send queue. Must be called *after* the offer/answer has been pushed to `send_queue`.
/// Sets `flushed = true` so subsequent candidates go directly to the send queue.
pub(super) fn flush_ice_buffer(cb_data: &StreamIceCallbackData) {
    let buffered: Vec<SignalEnvelope> = cb_data
        .pending
        .lock()
        .map(|mut buf| std::mem::take(&mut *buf))
        .unwrap_or_default();
    if !buffered.is_empty() {
        if let Ok(mut q) = cb_data.send_queue.lock() {
            for envelope in buffered {
                q.push((cb_data.peer_id.clone(), envelope));
            }
        }
    }
    cb_data
        .flushed
        .store(true, std::sync::atomic::Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn max_au_polls_per_tick_is_bounded() {
        assert_eq!(MAX_AU_POLLS_PER_TICK, 32);
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

    /// A packet carrying only the sequence header has no Opus payload, so it
    /// must be rejected rather than passed on as if the header were audio.
    #[test]
    fn audio_packets_without_a_payload_are_rejected() {
        let viewer = std::ptr::dangling_mut::<mello_sys::MelloStreamView>();
        // Exactly the header, no payload.
        let header_only = [0x01, 0x00, 0x00, 0x00];
        assert!(!unsafe { feed_viewer_audio_packet(viewer, &header_only) });
        // Shorter than the header.
        assert!(!unsafe { feed_viewer_audio_packet(viewer, &[0x01, 0x00]) });
        // Empty.
        assert!(!unsafe { feed_viewer_audio_packet(viewer, &[]) });
    }

    #[test]
    fn audio_packets_with_a_null_viewer_are_rejected() {
        let pkt = [0x01, 0x00, 0x00, 0x00, 0xAA, 0xBB];
        assert!(!unsafe { feed_viewer_audio_packet(std::ptr::null_mut(), &pkt) });
    }

    /// Documents the wire framing so it does not have to be re-derived from
    /// libmello: [seq_lo, seq_hi, 0, 0, opus...].
    #[test]
    fn audio_seq_header_is_four_bytes() {
        assert_eq!(AUDIO_SEQ_HEADER_LEN, 4);
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

    #[test]
    fn sfu_designated_mode_never_falls_back_to_p2p() {
        assert!(!should_fallback_to_p2p("sfu", true));
        assert!(!should_fallback_to_p2p("sfu", false));
        assert!(should_fallback_to_p2p("p2p", true));
        assert!(!should_fallback_to_p2p("p2p", false));
    }

    fn should_fallback_to_p2p(backend_mode: &str, setup_failed: bool) -> bool {
        setup_failed && backend_mode != "sfu"
    }

    #[test]
    fn duplicate_terminal_callbacks_queue_one_disconnect() {
        let disconnect_queue = Arc::new(std::sync::Mutex::new(VecDeque::new()));
        let data = StreamIceCallbackData {
            peer_id: "viewer-1".to_string(),
            send_queue: Arc::new(std::sync::Mutex::new(Vec::new())),
            disconnect_queue: Arc::clone(&disconnect_queue),
            pending: std::sync::Mutex::new(Vec::new()),
            flushed: AtomicBool::new(false),
        };

        assert!(enqueue_terminal_disconnect(&data, 3));
        assert!(!enqueue_terminal_disconnect(&data, 4));
        assert!(!enqueue_terminal_disconnect(&data, 5));
        assert_eq!(
            disconnect_queue
                .lock()
                .expect("disconnect queue lock")
                .iter()
                .map(|notice| notice.peer_id.as_str())
                .collect::<Vec<_>>(),
            vec!["viewer-1"]
        );
    }

    #[test]
    fn non_terminal_callback_does_not_queue_disconnect() {
        let disconnect_queue = Arc::new(std::sync::Mutex::new(VecDeque::new()));
        let data = StreamIceCallbackData {
            peer_id: "viewer-1".to_string(),
            send_queue: Arc::new(std::sync::Mutex::new(Vec::new())),
            disconnect_queue: Arc::clone(&disconnect_queue),
            pending: std::sync::Mutex::new(Vec::new()),
            flushed: AtomicBool::new(false),
        };

        assert!(!enqueue_terminal_disconnect(&data, 2));
        assert!(disconnect_queue
            .lock()
            .expect("disconnect queue lock")
            .is_empty());
    }
}
