use std::ffi::{CStr, CString};
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Instant;

use crate::events::Event;
use crate::stream::config::Codec;
use crate::stream::congestion::ViewerCongestionController;
use crate::stream::rtp_peer::{self, PeerMediaRole};
use crate::stream::sink_p2p::P2PFanoutSink;
use crate::transport::sfu_connection::{SfuConnection, SfuEvent, StreamPeerRole};
use crate::voice::{SignalEnvelope, SignalMessage, SignalPurpose};

#[cfg(not(target_os = "windows"))]
use super::stream_ffi::on_viewer_frame;

/// The bundled catalogue, parsed once.
///
/// The picker re-enumerates on every open and the catalogue is static for the
/// life of the process, so re-parsing it per call is pure waste.
fn catalogue() -> Option<&'static crate::catalogue::Head> {
    static HEAD: std::sync::OnceLock<Option<crate::catalogue::Head>> = std::sync::OnceLock::new();
    HEAD.get_or_init(crate::catalogue::Head::bundled).as_ref()
}
use super::stream_ffi::{
    feed_viewer_audio_packet, flush_ice_buffer, log_viewer_native_stats, on_viewer_native_frame,
    poll_p2p_viewer_access_units, poll_sfu_viewer_access_units, stream_audio_track_callback,
    stream_ice_callback, stream_state_callback, tick_viewer_congestion_p2p,
    tick_viewer_congestion_sfu, FrameCallbackData, StreamAudioCallbackData, StreamHostHandle,
    StreamHostPeer, StreamIceCallbackData, StreamPeerDisconnect, ViewerState,
};
use super::FRAME_STATE_PRESENTED;

const STREAM_DEBUG_EVENT_INTERVAL_SECS: f32 = 1.0;
/// Viewer report cadence to the SFU. Above the server's 5s rate limit so a
/// message is never dropped for arriving too soon.
const VIEWER_STATS_INTERVAL_SECS: u64 = 10;
const HOST_PACING_DEBUG_EVENT_INTERVAL_SECS: f32 = 1.0;

impl super::Client {
    pub(super) fn handle_stream_signal(&mut self, from: &str, envelope: SignalEnvelope) {
        // Host side: accept viewer offers, add peers to P2PFanoutSink
        if self.stream_session.is_some() {
            self.handle_stream_signal_as_host(from, envelope.message);
            return;
        }

        // Viewer side: handle answers and ICE from the host
        if self.viewer_state.is_some() {
            self.handle_stream_signal_as_viewer(from, envelope);
            return;
        }

        log::warn!(
            "Stream signal from {} but not hosting or viewing — ignoring",
            from
        );
    }

    fn handle_stream_signal_as_host(&mut self, from: &str, message: SignalMessage) {
        let ctx = self.voice.mello_ctx();

        match message {
            SignalMessage::Offer { sdp } => {
                log::info!("Stream offer from viewer {}", from);

                if self.stream_host_peers.contains_key(from) {
                    log::warn!("Duplicate stream offer from {}, destroying old peer", from);
                    if let Some(old) = self.stream_host_peers.remove(from) {
                        if let Some(ref sink) = self.stream_sink {
                            sink.remove_viewer(from);
                        }
                        unsafe {
                            mello_sys::mello_peer_destroy(old.peer);
                            if !old.ice_cb_data.is_null() {
                                drop(Box::from_raw(old.ice_cb_data));
                            }
                        }
                    }
                }

                // Create stream-host peer for this viewer (native RTP egress).
                let peer = match create_stream_p2p_peer(ctx, from, PeerMediaRole::StreamHost) {
                    Some(peer) => peer.as_ptr(),
                    None => return,
                };

                // Configure ICE servers
                let ice_cstrings: Vec<CString> = self
                    .ice_servers
                    .iter()
                    .filter_map(|u| CString::new(u.as_str()).ok())
                    .collect();
                if !ice_cstrings.is_empty() {
                    let ptrs: Vec<*const std::os::raw::c_char> =
                        ice_cstrings.iter().map(|s| s.as_ptr()).collect();
                    unsafe {
                        mello_sys::mello_peer_set_ice_servers(
                            peer,
                            ptrs.as_ptr() as *mut *const std::os::raw::c_char,
                            ptrs.len() as std::os::raw::c_int,
                        );
                    }
                }

                // ICE callback — candidates are buffered until answer is queued
                let ice_cb_data = Box::into_raw(Box::new(StreamIceCallbackData {
                    peer_id: from.to_string(),
                    send_queue: Arc::clone(&self.stream_signal_queue),
                    disconnect_queue: Arc::clone(&self.stream_disconnect_queue),
                    pending: std::sync::Mutex::new(Vec::new()),
                    flushed: std::sync::atomic::AtomicBool::new(false),
                }));
                unsafe {
                    mello_sys::mello_peer_set_ice_callback(
                        peer,
                        Some(stream_ice_callback),
                        ice_cb_data as *mut std::ffi::c_void,
                    );
                    mello_sys::mello_peer_set_state_callback(
                        peer,
                        Some(stream_state_callback),
                        ice_cb_data as *mut std::ffi::c_void,
                    );
                }

                // Create answer (may synchronously gather ICE candidates into buffer)
                let sdp_c = match CString::new(sdp) {
                    Ok(c) => c,
                    Err(_) => {
                        unsafe {
                            mello_sys::mello_peer_destroy(peer);
                            drop(Box::from_raw(ice_cb_data));
                        }
                        return;
                    }
                };
                let answer_ptr =
                    unsafe { mello_sys::mello_peer_create_answer(peer, sdp_c.as_ptr()) };
                if answer_ptr.is_null() {
                    log::error!("Failed to create stream answer for viewer {}", from);
                    unsafe {
                        mello_sys::mello_peer_destroy(peer);
                        drop(Box::from_raw(ice_cb_data));
                    }
                    return;
                }
                let answer = unsafe { CStr::from_ptr(answer_ptr) }
                    .to_string_lossy()
                    .into_owned();
                log::info!("Created stream answer for viewer {}", from);

                // Queue answer (with encode resolution) first, then flush buffered ICE candidates
                let (enc_w, enc_h) = (self.stream_encode_width, self.stream_encode_height);
                if let Ok(mut queue) = self.stream_signal_queue.lock() {
                    queue.push((
                        from.to_string(),
                        SignalEnvelope {
                            purpose: SignalPurpose::Stream,
                            stream_width: if enc_w > 0 { Some(enc_w) } else { None },
                            stream_height: if enc_h > 0 { Some(enc_h) } else { None },
                            stream_bitrate_kbps: (self.stream_bitrate_kbps > 0)
                                .then_some(self.stream_bitrate_kbps),
                            message: SignalMessage::Answer { sdp: answer },
                        },
                    ));
                }
                unsafe {
                    flush_ice_buffer(&*ice_cb_data);
                }

                // Add peer to P2PFanoutSink
                if let Some(ref sink) = self.stream_sink {
                    // SAFETY: `peer` was just created above and is destroyed
                    // only after remove_viewer or session stop.
                    if let Err(e) = unsafe { sink.add_viewer(from.to_string(), peer) } {
                        log::error!("Failed to add viewer {} to sink: {}", from, e);
                        unsafe {
                            mello_sys::mello_peer_destroy(peer);
                            drop(Box::from_raw(ice_cb_data));
                        }
                        return;
                    }
                }

                self.stream_host_peers
                    .insert(from.to_string(), StreamHostPeer { peer, ice_cb_data });

                // Apply any ICE candidates that arrived before this Offer
                if let Some(early_ice) = self.pending_remote_ice.remove(from) {
                    log::debug!(
                        "Applying {} buffered ICE candidates for viewer {}",
                        early_ice.len(),
                        from
                    );
                    for msg in early_ice {
                        if let SignalMessage::IceCandidate {
                            candidate,
                            sdp_mid,
                            sdp_mline_index,
                        } = msg
                        {
                            let cand_c = match CString::new(candidate) {
                                Ok(c) => c,
                                Err(_) => continue,
                            };
                            let mid_c = match CString::new(sdp_mid) {
                                Ok(c) => c,
                                Err(_) => continue,
                            };
                            let ice = mello_sys::MelloIceCandidate {
                                candidate: cand_c.as_ptr(),
                                sdp_mid: mid_c.as_ptr(),
                                sdp_mline_index,
                            };
                            unsafe {
                                mello_sys::mello_peer_add_ice_candidate(peer, &ice);
                            }
                        }
                    }
                }

                let _ = self.event_tx.send(Event::StreamViewerJoined {
                    viewer_id: from.to_string(),
                });
            }
            SignalMessage::IceCandidate {
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => {
                if let Some(hp) = self.stream_host_peers.get(from) {
                    let cand_c = match CString::new(candidate.clone()) {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let mid_c = match CString::new(sdp_mid.clone()) {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let ice = mello_sys::MelloIceCandidate {
                        candidate: cand_c.as_ptr(),
                        sdp_mid: mid_c.as_ptr(),
                        sdp_mline_index,
                    };
                    unsafe {
                        mello_sys::mello_peer_add_ice_candidate(hp.peer, &ice);
                    }
                    log::debug!("Added stream ICE candidate from viewer {}", from);
                } else {
                    log::debug!(
                        "Buffering early ICE candidate from viewer {} (offer not yet received)",
                        from
                    );
                    self.pending_remote_ice
                        .entry(from.to_string())
                        .or_default()
                        .push(SignalMessage::IceCandidate {
                            candidate,
                            sdp_mid,
                            sdp_mline_index,
                        });
                }
            }
            SignalMessage::Answer { .. } => {
                log::warn!(
                    "Unexpected stream Answer from {} while hosting — ignoring",
                    from
                );
            }
        }
    }

    fn handle_stream_signal_as_viewer(&mut self, from: &str, envelope: SignalEnvelope) {
        let (host_id, peer, viewer_is_none) = match self.viewer_state.as_ref() {
            Some(vs) => (vs.host_id.clone(), vs.peer, vs.viewer.is_none()),
            None => return,
        };

        if from != host_id {
            log::warn!(
                "Stream signal from {} but we're watching {} — ignoring",
                from,
                host_id
            );
            return;
        }

        match envelope.message {
            SignalMessage::Answer { sdp } => {
                let sdp_c = match CString::new(sdp) {
                    Ok(c) => c,
                    Err(_) => return,
                };
                unsafe {
                    mello_sys::mello_peer_set_remote_description(peer, sdp_c.as_ptr(), false);
                }
                log::info!("Set stream remote answer from host {}", from);

                let configured_bitrate_kbps = envelope
                    .stream_bitrate_kbps
                    .filter(|bitrate| *bitrate > 0)
                    .unwrap_or_else(|| {
                        let fallback = crate::stream::StreamConfig::default().bitrate_kbps;
                        log::warn!(
                            "Legacy stream Answer omitted bitrate; using default receive target {} kbps",
                            fallback
                        );
                        fallback
                    });
                if let Some(vs) = self.viewer_state.as_mut() {
                    vs.congestion.set_ceiling_kbps(configured_bitrate_kbps);
                }

                // Initialize the decoder pipeline now that we know the host's resolution
                if viewer_is_none {
                    let config = crate::stream::StreamConfig::default();
                    let (w, h) = match (envelope.stream_width, envelope.stream_height) {
                        (Some(sw), Some(sh)) if sw > 0 && sh > 0 => {
                            log::info!("Host encode resolution from signaling: {}x{}", sw, sh);
                            (sw, sh)
                        }
                        _ => {
                            log::warn!(
                                "No resolution in Answer, falling back to {}x{}",
                                config.width,
                                config.height
                            );
                            (config.width, config.height)
                        }
                    };

                    let mello_config = mello_sys::MelloStreamConfig {
                        width: w,
                        height: h,
                        fps: config.fps,
                        bitrate_kbps: 0,
                    };

                    let ctx = self.voice.mello_ctx();
                    let frame_cb_data = self
                        .viewer_state
                        .as_ref()
                        .map(|v| v._frame_cb_data)
                        .unwrap();
                    #[cfg(target_os = "windows")]
                    let frame_callback = None;
                    #[cfg(not(target_os = "windows"))]
                    let frame_callback = Some(on_viewer_frame as _);
                    let viewer = unsafe {
                        mello_sys::mello_stream_start_viewer(
                            ctx,
                            &mello_config,
                            frame_callback,
                            frame_cb_data as *mut std::ffi::c_void,
                        )
                    };
                    if !viewer.is_null() {
                        unsafe {
                            mello_sys::mello_stream_set_native_frame_callback(
                                viewer,
                                Some(on_viewer_native_frame as _),
                                frame_cb_data as *mut std::ffi::c_void,
                            );
                        }
                    }

                    if viewer.is_null() {
                        log::error!("Failed to start stream viewer pipeline at {}x{}", w, h);
                        let _ = self.event_tx.send(Event::StreamError {
                            message: "Failed to start video decoder".to_string(),
                        });
                        self.viewer_state = None;
                        return;
                    }

                    log::info!("Viewer pipeline initialized at {}x{}", w, h);
                    if let Some(vs) = self.viewer_state.as_mut() {
                        vs.viewer = Some(viewer);
                        if !vs._audio_cb_data.is_null() {
                            let cb = unsafe { &*vs._audio_cb_data };
                            if let Ok(mut slot) = cb.viewer_slot.lock() {
                                *slot = Some(viewer);
                            }
                        }
                    }
                }
            }
            SignalMessage::IceCandidate {
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => {
                let cand_c = match CString::new(candidate) {
                    Ok(c) => c,
                    Err(_) => return,
                };
                let mid_c = match CString::new(sdp_mid) {
                    Ok(c) => c,
                    Err(_) => return,
                };
                let ice = mello_sys::MelloIceCandidate {
                    candidate: cand_c.as_ptr(),
                    sdp_mid: mid_c.as_ptr(),
                    sdp_mline_index,
                };
                unsafe {
                    mello_sys::mello_peer_add_ice_candidate(peer, &ice);
                }
                log::debug!("Added stream ICE candidate from host {}", from);
            }
            SignalMessage::Offer { .. } => {
                log::warn!(
                    "Unexpected stream Offer from {} while viewing — ignoring",
                    from
                );
            }
        }
    }

    pub(super) async fn stream_tick(&mut self) {
        // 1. Drain stream signal queue and send via Nakama
        let signals: Vec<(String, SignalEnvelope)> = {
            match self.stream_signal_queue.lock() {
                Ok(mut q) => std::mem::take(&mut *q),
                Err(_) => Vec::new(),
            }
        };
        for (to, envelope) in signals {
            let payload = match serde_json::to_string(&envelope) {
                Ok(p) => p,
                Err(e) => {
                    log::error!("Failed to serialize stream signal: {}", e);
                    continue;
                }
            };
            if let Err(e) = self.nakama.send_signal(&to, &payload).await {
                log::error!("Failed to send stream signal to {}: {}", to, e);
            }
        }

        self.drain_stream_peer_disconnects();
        self.poll_sfu_host_membership_events().await;
        self.emit_host_pacing_debug_stats().await;

        // 2. Poll viewer for incoming stream packets
        if self.viewer_state.is_none() {
            return;
        }

        let vs = self.viewer_state.as_mut().unwrap();
        vs.stream_tick_count = vs.stream_tick_count.saturating_add(1);
        let viewer = match vs.viewer {
            Some(v) => v,
            None => return, // Decoder not yet initialized (waiting for Answer)
        };

        if vs.mode == "sfu" {
            if let Some(conn) = vs.sfu_connection.clone() {
                // ~2s control-channel ping: the stream path had no RTT signal
                // at all (send_ping was only wired for voice), leaving rtt_ms
                // dark in viewer telemetry.
                if vs.stream_tick_count.is_multiple_of(125) {
                    conn.send_ping();
                }
                tick_viewer_congestion_sfu(vs, &conn);
                // Deliberately not logged per access unit: at 60fps that is 60
                // lines a second, and every field is already accumulated into
                // transport_packets / transport_bytes / au_buffer_grows and
                // reported once a second by `Stream cadence`.
                let _ = poll_sfu_viewer_access_units(vs, viewer, &conn);
            }
        } else if !vs.peer.is_null() {
            if let Some(peer) = NonNull::new(vs.peer) {
                tick_viewer_congestion_p2p(vs, peer);
                // Same reasoning as the SFU path above.
                let _ = poll_p2p_viewer_access_units(vs, viewer, peer);
            }
        }

        // Present at most one frame per stream tick so visual cadence tracks
        // UI cadence instead of burst-draining the decoder queue.
        const MAX_PRESENTS_PER_TICK: u32 = 1;
        let mut presented_this_tick = 0u32;

        while presented_this_tick < MAX_PRESENTS_PER_TICK {
            vs.present_attempts = vs.present_attempts.saturating_add(1);
            let presented = unsafe { mello_sys::mello_stream_present_frame(viewer) };
            vs.last_present_attempt = Instant::now();
            if presented {
                vs.frames_presented += 1;
                presented_this_tick += 1;
                vs.last_new_frame_at = Instant::now();
                vs.in_freeze = false;
                vs.freeze_accounted_ms = 0;
                if vs.frames_presented <= 3 || vs.frames_presented.is_multiple_of(300) {
                    log::info!("Stream frame presented #{}", vs.frames_presented);
                }
            } else {
                break; // ring buffer empty
            }
        }

        // Sampled every tick rather than only on recovery, so a freeze in
        // progress is already visible in telemetry instead of appearing only
        // once the picture comes back.
        //
        // Not counted before the first frame: pipeline init and the wait for the
        // first IDR would otherwise register as a freeze on every session, and
        // "startup took a while" is a different measurement from "the picture
        // stalled" (the certification gate already times first-keyframe).
        if vs.frames_presented > 0 {
            let present_gap_ms = vs.last_new_frame_at.elapsed().as_millis() as u64;
            let (mut in_freeze, mut count, mut total, mut accounted) = (
                vs.in_freeze,
                vs.freeze_count,
                vs.freeze_ms_total,
                vs.freeze_accounted_ms,
            );
            super::stream_ffi::note_present_gap(
                present_gap_ms,
                &mut in_freeze,
                &mut count,
                &mut total,
                &mut accounted,
            );
            vs.in_freeze = in_freeze;
            vs.freeze_count = count;
            vs.freeze_ms_total = total;
            vs.freeze_accounted_ms = accounted;
        }

        // Poll SFU events for session lifecycle only — stream video uses native RTP.
        if let Some(ref conn) = vs.sfu_connection {
            for event in conn.poll_events() {
                match event {
                    crate::transport::SfuEvent::Disconnected { reason } => {
                        log::info!("Stream SFU disconnected: {}", reason);
                        let _ = self.event_tx.send(Event::StreamWatchingStopped);
                        self.viewer_state.take();
                        return;
                    }
                    crate::transport::SfuEvent::MediaPacket { .. } => {
                        log::debug!("Ignoring SFU MediaPacket — stream video uses native RTP");
                    }
                    crate::transport::SfuEvent::ControlPacket { .. }
                    | crate::transport::SfuEvent::MemberJoined { .. }
                    | crate::transport::SfuEvent::MemberLeft { .. } => {}
                    crate::transport::SfuEvent::AudioTrackData { data, .. } => {
                        if let Some(viewer) = vs.viewer {
                            // SAFETY: `vs.viewer` is set only from a successful
                            // viewer start and cleared on stop, so it is valid
                            // for the lifetime of this ViewerState.
                            if unsafe { feed_viewer_audio_packet(viewer, &data) } {
                                vs.audio_packets_received =
                                    vs.audio_packets_received.saturating_add(1);
                            }
                        }
                    }
                }
            }
        }

        let elapsed = vs.debug_last_emit.elapsed().as_secs_f32();
        if elapsed >= STREAM_DEBUG_EVENT_INTERVAL_SECS {
            let delta_ticks = vs
                .stream_tick_count
                .saturating_sub(vs.debug_last_tick_count);
            let delta_bytes = vs.transport_bytes.saturating_sub(vs.debug_last_bytes);
            let delta_frames = vs
                .frames_presented
                .saturating_sub(vs.debug_last_frames_presented);
            let delta_present_attempts = vs
                .present_attempts
                .saturating_sub(vs.debug_last_present_attempts);
            let delta_present_forced = vs
                .present_forced_attempts
                .saturating_sub(vs.debug_last_present_forced_attempts);
            let delta_present_skipped = vs
                .present_skipped_unconsumed
                .saturating_sub(vs.debug_last_present_skipped_unconsumed);
            let delta_backlog_guard_drops = vs
                .backlog_guard_drops
                .saturating_sub(vs.debug_last_backlog_guard_drops);
            let ingress_kbps = (delta_bytes as f32 * 8.0 / 1000.0) / elapsed.max(0.001);
            let present_fps = (delta_frames as f32) / elapsed.max(0.001);
            let stream_tick_hz = (delta_ticks as f32) / elapsed.max(0.001);
            let present_attempt_hz = (delta_present_attempts as f32) / elapsed.max(0.001);

            let _ = self.event_tx.send(Event::StreamDebugStats {
                mode: vs.mode.clone(),
                transport_packets: vs.transport_packets,
                transport_bytes: vs.transport_bytes,
                transport_truncations: vs.au_buffer_grows,
                frames_presented: vs.frames_presented,
                present_fps,
                ingress_kbps,
            });

            if vs.mode == "sfu" {
                if let Some(conn) = vs.sfu_connection.clone() {
                    if let Ok(stats) = conn.video_stats() {
                        log_viewer_native_stats(&vs.mode, &stats);
                    }
                }
            } else if let Some(peer) = NonNull::new(vs.peer) {
                if let Ok(stats) = video_stats_from_peer(peer) {
                    log_viewer_native_stats("p2p", &stats);
                }
            }

            let rtt_ms = if vs.mode == "sfu" {
                vs.sfu_connection.as_ref().map(|conn| conn.rtt_ms())
            } else {
                NonNull::new(vs.peer)
                    .map(|peer| unsafe { mello_sys::mello_peer_rtt_ms(peer.as_ptr()) })
            };
            log::info!(
                "Stream cadence: mode={} tick_hz={:.1} present_attempt_hz={:.1} present_fps={:.1} forced={} skipped_unconsumed={} backlog_guard_drops={} rtt_ms={:.0}",
                vs.mode,
                stream_tick_hz,
                present_attempt_hz,
                present_fps,
                delta_present_forced,
                delta_present_skipped,
                delta_backlog_guard_drops,
                rtt_ms.unwrap_or(-1.0)
            );

            vs.debug_last_emit = Instant::now();
            vs.debug_last_tick_count = vs.stream_tick_count;
            vs.debug_last_present_attempts = vs.present_attempts;
            vs.debug_last_present_forced_attempts = vs.present_forced_attempts;
            vs.debug_last_present_skipped_unconsumed = vs.present_skipped_unconsumed;
            vs.debug_last_packets = vs.transport_packets;
            vs.debug_last_bytes = vs.transport_bytes;
            vs.debug_last_frames_presented = vs.frames_presented;
            vs.debug_last_backlog_guard_drops = vs.backlog_guard_drops;
        }

        self.report_viewer_stream_stats().await;
    }

    /// Push viewer diagnostics to the SFU every 10s.
    ///
    /// SFU mode only: P2P has no relay to report to, and the host is the peer we
    /// would be reporting about. Freeze time leads the payload because it is the
    /// only field that corresponds to what the user actually sees — the rest
    /// explain *why* a freeze happened.
    async fn report_viewer_stream_stats(&mut self) {
        let Some(vs) = self.viewer_state.as_mut() else {
            return;
        };
        if vs.mode != "sfu" {
            return;
        }
        if vs.stats_last_emit.elapsed().as_secs() < VIEWER_STATS_INTERVAL_SECS {
            return;
        }
        let elapsed = vs.stats_last_emit.elapsed().as_secs_f32().max(0.001);
        vs.stats_last_emit = Instant::now();

        let Some(conn) = vs.sfu_connection.clone() else {
            return;
        };

        let present_fps = vs
            .frames_presented
            .saturating_sub(vs.stats_last_frames_presented) as f32
            / elapsed;
        vs.stats_last_frames_presented = vs.frames_presented;

        let freeze_count = vs.freeze_count.saturating_sub(vs.stats_last_freeze_count);
        vs.stats_last_freeze_count = vs.freeze_count;
        let freeze_ms = vs.freeze_ms_total.saturating_sub(vs.stats_last_freeze_ms);
        vs.stats_last_freeze_ms = vs.freeze_ms_total;

        let native = conn.video_stats().ok();
        let payload = serde_json::json!({
            "role": "viewer",
            "freeze_n": freeze_count,
            "freeze_ms": freeze_ms,
            "present_fps": (present_fps * 10.0).round() / 10.0,
            "rtt_ms": conn.rtt_ms().round() as i32,
            "rx_complete": native.as_ref().map(|s| s.rx_core_complete_access_units),
            "rx_incomplete": native.as_ref().map(|s| s.rx_core_incomplete_access_units),
            "rx_missing": native.as_ref().map(|s| s.rx_core_missing_sequences_detected),
            "rx_repaired": native.as_ref().map(|s| s.rx_core_repaired_packets),
            "rx_gated": native.as_ref().map(|s| s.rx_core_gated),
            "rx_nacks": native.as_ref().map(|s| s.rx_nack_sequences_sent),
            "rx_pli": native.as_ref().map(|s| s.rx_pli_requests),
            "rx_jitter": native.as_ref().map(|s| s.rx_core_interarrival_jitter),
            "rx_fec_ok": native.as_ref().map(|s| s.rx_fec_recovered),
            "rx_fec_fail": native.as_ref().map(|s| s.rx_fec_unrecoverable),
            "bg_drops": vs.backlog_guard_drops,
        });
        conn.send_stream_stats(&payload).await;
    }

    async fn poll_sfu_host_membership_events(&mut self) {
        let Some(conn) = self.stream_sfu_connection.clone() else {
            return;
        };
        let Some(sink) = self.stream_host_sink.clone() else {
            return;
        };

        // ~2s control-channel ping so the host measures its RTT to the SFU
        // and the control round-trip stays observable (125 x 16ms ticks).
        self.host_sfu_ping_ticks = self.host_sfu_ping_ticks.wrapping_add(1);
        if self.host_sfu_ping_ticks.is_multiple_of(125) {
            conn.send_ping();
        }

        for event in conn.poll_events() {
            match event {
                SfuEvent::MemberJoined { user_id, role } => {
                    if role == "viewer" {
                        sink.on_viewer_joined(&user_id).await;
                    }
                }
                SfuEvent::MemberLeft { user_id, .. } => {
                    sink.on_viewer_left(&user_id).await;
                }
                SfuEvent::Disconnected { reason } => {
                    log::warn!("Stream SFU host disconnected: {}", reason);
                }
                SfuEvent::MediaPacket { .. } | SfuEvent::ControlPacket { .. } => {}
                SfuEvent::AudioTrackData { .. } => {}
            }
        }
    }

    async fn emit_host_pacing_debug_stats(&mut self) {
        if self.stream_session.is_none() {
            return;
        }
        let Some(sink) = self.stream_host_sink.clone() else {
            return;
        };

        let elapsed = self.host_pacing_last_at.elapsed().as_secs_f32();
        if elapsed < HOST_PACING_DEBUG_EVENT_INTERVAL_SECS {
            return;
        }

        let Some(now_stats) = sink.pacing_telemetry().await else {
            return;
        };

        let (delta_bytes, delta_sleep_count, delta_sleep_ms) =
            if let Some(prev) = self.host_pacing_last {
                (
                    now_stats.paced_bytes.saturating_sub(prev.paced_bytes),
                    now_stats.sleep_count.saturating_sub(prev.sleep_count),
                    now_stats.sleep_ms_total.saturating_sub(prev.sleep_ms_total),
                )
            } else {
                (0, 0, 0)
            };

        let out_kbps = if elapsed > 0.0 {
            (delta_bytes as f32 * 8.0 / 1000.0) / elapsed
        } else {
            0.0
        };

        let mode = self
            .stream_session
            .as_ref()
            .map(|s| s.mode.clone())
            .unwrap_or_else(|| "unknown".to_string());

        let _ = self.event_tx.send(Event::StreamHostPacingStats {
            mode,
            target_kbps: now_stats.target_kbps,
            out_kbps,
            paced_bytes: now_stats.paced_bytes,
            sleep_count: now_stats.sleep_count,
            sleep_ms_total: now_stats.sleep_ms_total,
            sleep_count_delta: delta_sleep_count,
            sleep_ms_delta: delta_sleep_ms,
        });

        self.host_pacing_last = Some(now_stats);
        self.host_pacing_last_at = Instant::now();
    }

    pub(super) fn handle_list_capture_sources(&mut self) {
        let ctx = self.voice.mello_ctx();
        if ctx.is_null() {
            log::error!("Cannot enumerate capture sources: libmello not initialized");
            return;
        }

        let mut mons_raw = vec![
            mello_sys::MelloMonitorInfo {
                index: 0,
                name: [0i8; 128],
                width: 0,
                height: 0,
                primary: false,
            };
            16
        ];
        let mon_count =
            unsafe { mello_sys::mello_enumerate_monitors(ctx, mons_raw.as_mut_ptr(), 16) };
        let mut monitors = Vec::new();
        for mon in mons_raw.iter().take(mon_count as usize) {
            let display_name = if mon.primary {
                format!("Display {} (Primary)", mon.index + 1)
            } else {
                format!("Display {}", mon.index + 1)
            };
            monitors.push(crate::events::CaptureSource {
                id: format!("monitor-{}", mon.index),
                name: display_name,
                mode: "monitor".to_string(),
                monitor_index: Some(mon.index),
                hwnd: None,
                pid: None,
                exe: String::new(),
                is_fullscreen: false,
                resolution: format!("{}x{}", mon.width, mon.height),
            });
        }

        // libmello's enumerator returns the whole process table by design — the
        // game database decides what is a game (spec 17 §2.1), not libmello. The
        // buffer therefore has to fit a real process table: at 32 entries it
        // truncated in *boot order*, so the list was System, smss, csrss, wininit,
        // services, lsass, svchost... and never reached a game.
        const MAX_PROCESSES: usize = 512;
        let mut games_raw = vec![
            mello_sys::MelloGameProcess {
                pid: 0,
                name: [0i8; 128],
                exe: [0i8; 260],
                is_fullscreen: false,
                path: [0i8; 520],
                title: [0i8; 256],
                is_foreground: false,
                started_at_ms: 0,
            };
            MAX_PROCESSES
        ];
        let game_count = unsafe {
            mello_sys::mello_enumerate_games(ctx, games_raw.as_mut_ptr(), MAX_PROCESSES as i32)
        };

        let head = catalogue();
        let own_pid = std::process::id();

        // Two tiers. Database matches are what the user is almost always after,
        // so they come first and carry the catalogue's display name. Everything
        // else that owns a visible window follows, so a game the database has
        // never heard of — an indie title, a beta, a private build — is still
        // streamable instead of disappearing from the picker entirely.
        let mut known = Vec::new();
        let mut other = Vec::new();
        for game in games_raw.iter().take(game_count as usize) {
            if game.pid == own_pid {
                continue;
            }
            let exe = unsafe { std::ffi::CStr::from_ptr(game.exe.as_ptr()) }
                .to_string_lossy()
                .to_string();
            let title = unsafe { std::ffi::CStr::from_ptr(game.title.as_ptr()) }
                .to_string_lossy()
                .to_string();
            let path = unsafe { std::ffi::CStr::from_ptr(game.path.as_ptr()) }
                .to_string_lossy()
                .to_string();
            let entry = head.and_then(|h| h.lookup_exe(&exe, &path));
            // A process with no window cannot be captured, so it has no business
            // in a capture picker regardless of which tier it would land in.
            if entry.is_none() && title.is_empty() {
                continue;
            }
            let name = match &entry {
                Some(e) => e.name.to_string(),
                None if !title.is_empty() => title,
                None => unsafe { std::ffi::CStr::from_ptr(game.name.as_ptr()) }
                    .to_string_lossy()
                    .to_string(),
            };
            let source = crate::events::CaptureSource {
                id: format!("game-{}", game.pid),
                name,
                mode: "process".to_string(),
                monitor_index: None,
                hwnd: None,
                pid: Some(game.pid),
                exe,
                is_fullscreen: game.is_fullscreen,
                resolution: String::new(),
            };
            if entry.is_some() {
                known.push((game.is_foreground, source));
            } else {
                other.push((game.is_foreground, source));
            }
        }
        // Whatever the user is currently in is the likeliest pick, so it leads
        // its tier; the rest sort by name so the list does not reshuffle between
        // openings the way process-table order would.
        for tier in [&mut known, &mut other] {
            tier.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        }
        let known_count = known.len();
        let games: Vec<crate::events::CaptureSource> = known
            .into_iter()
            .chain(other)
            .map(|(_, source)| source)
            .collect();

        let mut windows_raw = vec![
            mello_sys::MelloWindow {
                hwnd: std::ptr::null_mut(),
                title: [0i8; 256],
                exe: [0i8; 256],
                pid: 0,
            };
            64
        ];
        let win_count =
            unsafe { mello_sys::mello_enumerate_windows(ctx, windows_raw.as_mut_ptr(), 64) };
        let mut windows = Vec::new();
        for win in windows_raw.iter().take(win_count as usize) {
            let title = unsafe { std::ffi::CStr::from_ptr(win.title.as_ptr()) }
                .to_string_lossy()
                .to_string();
            let exe = unsafe { std::ffi::CStr::from_ptr(win.exe.as_ptr()) }
                .to_string_lossy()
                .to_string();
            let hwnd = win.hwnd as u64;
            windows.push(crate::events::CaptureSource {
                id: format!("window-{}", hwnd),
                name: title,
                mode: "window".to_string(),
                monitor_index: None,
                hwnd: Some(hwnd),
                pid: Some(win.pid),
                exe,
                is_fullscreen: false,
                resolution: String::new(),
            });
        }

        // Cache windows for thumbnail refresh
        self.cached_windows = windows
            .iter()
            .filter_map(|w| w.hwnd.map(|h| (w.id.clone(), h)))
            .collect();

        log::info!(
            "Enumerated capture sources: {} monitors, {} games ({} from game db), {} windows",
            monitors.len(),
            games.len(),
            known_count,
            windows.len()
        );
        let _ = self.event_tx.send(Event::CaptureSourcesListed {
            monitors,
            games,
            windows,
        });
    }

    pub(super) fn start_thumbnail_refresh(&mut self) {
        self.stop_thumbnail_refresh();

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.thumbnail_stop = Some(stop.clone());

        let event_tx = self.event_tx.clone();
        let windows = self.cached_windows.clone();

        const THUMB_W: u32 = 192;
        const THUMB_H: u32 = 128;
        let buf_size = (THUMB_W * THUMB_H * 4) as usize;

        std::thread::spawn(move || {
            log::debug!(
                "Thumbnail refresh thread started for {} windows",
                windows.len()
            );
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let mut thumbnails = Vec::new();
                for (id, hwnd) in &windows {
                    let mut rgba = vec![0u8; buf_size];
                    let mut out_w: u32 = 0;
                    let mut out_h: u32 = 0;
                    let ret = unsafe {
                        mello_sys::mello_capture_window_thumbnail(
                            *hwnd as *mut std::ffi::c_void,
                            THUMB_W,
                            THUMB_H,
                            rgba.as_mut_ptr(),
                            &mut out_w,
                            &mut out_h,
                        )
                    };
                    if ret == 0 && out_w > 0 && out_h > 0 {
                        rgba.truncate((out_w * out_h * 4) as usize);
                        thumbnails.push((id.clone(), rgba, out_w, out_h));
                    }
                }

                if !thumbnails.is_empty() {
                    let _ = event_tx.send(Event::WindowThumbnailsUpdated { thumbnails });
                }

                // Sleep 3 seconds, checking stop flag every 100ms
                for _ in 0..30 {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            log::debug!("Thumbnail refresh thread stopped");
        });
    }

    pub(super) fn stop_thumbnail_refresh(&mut self) {
        if let Some(stop) = self.thumbnail_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_start_stream(
        &mut self,
        crew_id: &str,
        title: &str,
        capture_mode: &str,
        monitor_index: Option<u32>,
        hwnd: Option<u64>,
        pid: Option<u32>,
        preset_idx: u32,
    ) {
        if self.stream_session.is_some() {
            let _ = self.event_tx.send(Event::StreamError {
                message: "Already streaming".to_string(),
            });
            return;
        }

        let quality_preset = match preset_idx {
            0 => crate::stream::config::QualityPreset::Ultra,
            1 => crate::stream::config::QualityPreset::High,
            3 => crate::stream::config::QualityPreset::Low,
            4 => crate::stream::config::QualityPreset::Potato,
            _ => crate::stream::config::QualityPreset::Medium,
        };
        log::info!("Starting stream with preset: {:?}", quality_preset);

        // Resolve the capture target before anything else, and refuse to guess.
        // A stream that captures the wrong thing is indistinguishable from one
        // that captures nothing, so this must fail here rather than fall back.
        let target = match crate::stream::config::CaptureTarget::resolve(
            capture_mode,
            monitor_index,
            hwnd,
            pid,
        ) {
            Ok(t) => t,
            Err(e) => {
                log::error!("Stream start rejected: {}", e);
                let _ = self.event_tx.send(Event::StreamError { message: e });
                return;
            }
        };
        log::info!(
            "Capture target resolved: {} (requested mode={:?} monitor={:?} hwnd={:?} pid={:?}, title={:?})",
            target.describe(),
            capture_mode,
            monitor_index,
            hwnd,
            pid,
            title
        );

        // Step 1: async RPC call (no raw pointers held across await)
        let mut config = crate::stream::StreamConfig::from_preset(
            quality_preset,
            crate::stream::config::Codec::H264,
        );
        config.capture_desc = target.describe();
        let configured_bitrate_kbps = config.bitrate_kbps;
        let resp = match crate::stream::host::request_start_stream(
            &self.nakama,
            crew_id,
            title,
            false, // supports_av1
            config.width,
            config.height,
            config.bitrate_kbps,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                log::error!("start_stream RPC failed: {}", e);
                let _ = self.event_tx.send(Event::StreamError {
                    message: e.to_string(),
                });
                return;
            }
        };

        // Step 2: sync FFI calls (raw pointer ctx must NOT live across await)
        // Scope ctx so it's dropped before any SFU .await calls.
        let (host, video_rx, audio_rx, resources) = {
            let ctx = self.voice.mello_ctx();

            if !unsafe { crate::stream::encoder_available(ctx) } {
                let msg = "Streaming requires a hardware encoder \
                           (NVIDIA, AMD, or Intel). None was found on this machine.";
                log::error!("{}", msg);
                let _ = self.event_tx.send(Event::StreamError {
                    message: msg.to_string(),
                });
                return;
            }

            let mello_config = mello_sys::MelloStreamConfig {
                width: config.width,
                height: config.height,
                fps: config.fps,
                bitrate_kbps: config.bitrate_kbps,
            };

            let source = match target {
                crate::stream::config::CaptureTarget::Window { hwnd } => {
                    mello_sys::MelloCaptureSource {
                        mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_WINDOW,
                        monitor_index: 0,
                        hwnd: hwnd as *mut std::ffi::c_void,
                        pid: 0,
                    }
                }
                crate::stream::config::CaptureTarget::Process { pid } => {
                    mello_sys::MelloCaptureSource {
                        mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_PROCESS,
                        monitor_index: 0,
                        hwnd: std::ptr::null_mut(),
                        pid,
                    }
                }
                crate::stream::config::CaptureTarget::Monitor { index } => {
                    mello_sys::MelloCaptureSource {
                        mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_MONITOR,
                        monitor_index: index,
                        hwnd: std::ptr::null_mut(),
                        pid: 0,
                    }
                }
            };

            let (host, video_rx, audio_rx, resources) =
                match unsafe { crate::stream::host::start_host(ctx, &source, &mello_config) } {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = self.event_tx.send(Event::StreamError {
                            message: e.to_string(),
                        });
                        return;
                    }
                };

            let (mut actual_w, mut actual_h) = (config.width, config.height);
            unsafe {
                mello_sys::mello_stream_get_host_resolution(host, &mut actual_w, &mut actual_h);
            }
            log::info!("Host encode resolution: {}x{}", actual_w, actual_h);
            self.stream_encode_width = actual_w;
            self.stream_encode_height = actual_h;

            unsafe {
                mello_sys::mello_stream_start_audio(host);
            }

            (StreamHostHandle(host), video_rx, audio_rx, resources)
        }; // ctx and raw pointers drop here — safe to .await below

        // Update backend with actual encode resolution (may differ from preset)
        if let Err(e) = self
            .nakama
            .update_stream_resolution(crew_id, self.stream_encode_width, self.stream_encode_height)
            .await
        {
            log::warn!("update_stream_resolution RPC failed: {}", e);
        }

        // Select sink based on mode: SFU for premium crews, P2P for free
        let (sink, p2p_sink): (
            Arc<dyn crate::stream::sink::PacketSink>,
            Option<Arc<P2PFanoutSink>>,
        ) = if resp.mode == "sfu" {
            let endpoint = resp.sfu_endpoint.as_deref().unwrap_or_default();
            let token = resp.sfu_token.as_deref().unwrap_or_default();
            let mut sfu_sink: Option<Arc<dyn crate::stream::sink::PacketSink>> = None;
            match crate::transport::SfuConnection::connect(endpoint, token).await {
                Ok(mut conn) => {
                    let peer_handle = {
                        let ctx = self.voice.mello_ctx();
                        unsafe { SfuConnection::create_stream_peer(ctx, StreamPeerRole::Host) }
                    };
                    match peer_handle {
                        Ok(ph) => match conn.join_stream(ph, &resp.session_id(), "host").await {
                            Ok(_session) => {
                                if let Err(e) = conn.wait_for_datachannel_open().await {
                                    log::error!("SFU stream transport failed: {}", e);
                                } else {
                                    let conn = Arc::new(conn);
                                    self.stream_sfu_connection = Some(Arc::clone(&conn));
                                    sfu_sink =
                                        Some(Arc::new(crate::stream::sink_sfu::SfuSink::new(conn)));
                                }
                            }
                            Err(e) => log::error!("SFU join_stream failed: {}", e),
                        },
                        Err(e) => log::error!("SFU stream peer creation failed: {}", e),
                    }
                }
                Err(e) => log::error!("SFU connect failed: {}", e),
            }
            if let Some(sink) = sfu_sink {
                (sink, None)
            } else {
                let p2p = Arc::new(P2PFanoutSink::new());
                (Arc::clone(&p2p) as _, Some(p2p))
            }
        } else {
            let p2p = Arc::new(P2PFanoutSink::new());
            (Arc::clone(&p2p) as _, Some(p2p))
        };

        if resp.mode == "sfu" && p2p_sink.is_some() {
            let message =
                "SFU stream setup failed on host; aborting stream start (no silent P2P fallback)";
            log::error!("{}", message);
            unsafe {
                mello_sys::mello_stream_stop_audio(host.0);
                mello_sys::mello_stream_stop_host(host.0);
            }
            let _ = self.event_tx.send(Event::StreamError {
                message: message.to_string(),
            });
            self.stream_host_sink = None;
            self.stream_sfu_connection = None;
            self.stream_sink = None;
            return;
        }

        // Re-obtain ctx for session creation (sync, no more awaits)
        let ctx = self.voice.mello_ctx();
        let host = host.0;
        match crate::stream::host::create_stream_session(
            ctx,
            host,
            &resp,
            config,
            video_rx,
            audio_rx,
            resources,
            Arc::clone(&sink),
        ) {
            Ok(session) => {
                let _ = self.event_tx.send(Event::StreamStarted {
                    crew_id: crew_id.to_string(),
                    session_id: session.session_id.clone(),
                    mode: session.mode.clone(),
                });
                self.stream_host_sink = Some(Arc::clone(&sink));
                self.stream_sink = p2p_sink;
                self.stream_session = Some(session);
                self.stream_bitrate_kbps = configured_bitrate_kbps;
                self.host_pacing_last = None;
                self.host_pacing_last_at = Instant::now();
            }
            Err(e) => {
                log::error!("Failed to create stream session: {}", e);
                unsafe {
                    mello_sys::mello_stream_stop_audio(host);
                    mello_sys::mello_stream_stop_host(host);
                }
                let _ = self.event_tx.send(Event::StreamError {
                    message: e.to_string(),
                });
                self.stream_host_sink = None;
            }
        }
    }

    pub(super) async fn handle_stop_stream(&mut self) {
        if let Some(session) = self.stream_session.take() {
            session.stop_and_wait().await;

            // The manager has exited. Remove every sink membership before
            // destroying its native peer and callback state.
            for (id, hp) in self.stream_host_peers.drain() {
                if let Some(ref sink) = self.stream_sink {
                    sink.remove_viewer(&id);
                }
                unsafe {
                    mello_sys::mello_peer_destroy(hp.peer);
                    if !hp.ice_cb_data.is_null() {
                        drop(Box::from_raw(hp.ice_cb_data));
                    }
                }
                log::info!("Destroyed stream host peer {}", id);
            }
            self.stream_sink = None;
            self.stream_host_sink = None;
            self.stream_sfu_connection = None;
            self.host_pacing_last = None;
            self.host_pacing_last_at = Instant::now();
            self.pending_remote_ice.clear();
            if let Ok(mut queue) = self.stream_disconnect_queue.lock() {
                queue.clear();
            }
            self.stream_encode_width = 0;
            self.stream_encode_height = 0;
            self.stream_bitrate_kbps = 0;

            if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
                let payload = serde_json::json!({ "crew_id": crew_id });
                if let Err(e) = self.nakama.rpc("stop_stream", &payload).await {
                    log::warn!("stop_stream RPC failed: {}", e);
                }
                let _ = self.event_tx.send(Event::StreamEnded { crew_id });
            }
        }
    }

    fn drain_stream_peer_disconnects(&mut self) {
        let notices = self
            .stream_disconnect_queue
            .lock()
            .map(|mut queue| queue.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();

        for notice in notices {
            let detached = detach_disconnected_host_peer(
                &notice,
                self.stream_sink.as_deref(),
                &mut self.stream_host_peers,
                |_| {},
            );
            if let Some(peer) = detached {
                unsafe {
                    mello_sys::mello_peer_destroy(peer.peer);
                    if !peer.ice_cb_data.is_null() {
                        drop(Box::from_raw(peer.ice_cb_data));
                    }
                }
                log::info!("Cleaned up disconnected stream viewer {}", notice.peer_id);
                let _ = self.event_tx.send(Event::StreamViewerLeft {
                    viewer_id: notice.peer_id,
                });
                continue;
            }

            let viewer_matches = self.viewer_state.as_ref().is_some_and(|viewer| {
                viewer.mode == "p2p"
                    && viewer.host_id == notice.peer_id
                    && viewer._ice_cb_data as usize == notice.callback_data
            });
            if viewer_matches {
                if let Some(viewer) = self.viewer_state.take() {
                    drop(viewer);
                }
                log::info!("P2P stream host disconnected while viewing");
                let _ = self.event_tx.send(Event::StreamWatchingStopped);
            } else {
                log::debug!(
                    "Ignoring stale stream disconnect notice for {}",
                    notice.peer_id
                );
            }
        }
    }

    pub(super) async fn handle_watch_stream(
        &mut self,
        host_id: &str,
        session_id: &str,
        stream_width: u32,
        stream_height: u32,
    ) {
        if self.viewer_state.is_some() {
            log::warn!("Already watching a stream, ignoring WatchStream");
            return;
        }

        log::info!("Starting stream viewer for host {}", host_id);
        let ctx = self.voice.mello_ctx();
        if ctx.is_null() {
            let _ = self.event_tx.send(Event::StreamError {
                message: "libmello context not initialized".to_string(),
            });
            return;
        }

        // Ask the backend which mode to use for viewing
        if session_id.is_empty() {
            log::info!("No session_id provided, using P2P viewer path");
            self.watch_stream_p2p(host_id, stream_width, stream_height);
            return;
        }

        let watch_resp = match self.nakama.watch_stream(session_id).await {
            Ok(r) => {
                log::info!("watch_stream RPC: mode={}", r.mode);
                r
            }
            Err(e) => {
                log::error!("watch_stream RPC failed: {}", e);
                let _ = self.event_tx.send(Event::StreamError {
                    message: e.to_string(),
                });
                return;
            }
        };

        if watch_resp.mode == "sfu" {
            self.watch_stream_sfu(
                host_id,
                session_id,
                stream_width,
                stream_height,
                &watch_resp,
            )
            .await;
        } else {
            self.watch_stream_p2p(host_id, stream_width, stream_height);
        }
    }

    /// SFU viewer path: connect to SFU, join session as viewer, initialize decoder.
    async fn watch_stream_sfu(
        &mut self,
        host_id: &str,
        session_id: &str,
        stream_width: u32,
        stream_height: u32,
        resp: &crate::nakama::WatchStreamResponse,
    ) {
        let endpoint = resp.sfu_endpoint.as_deref().unwrap_or_default();
        let token = resp.sfu_token.as_deref().unwrap_or_default();

        let mut conn = match crate::transport::SfuConnection::connect(endpoint, token).await {
            Ok(c) => c,
            Err(e) => {
                log::error!("SFU viewer connect failed: {}", e);
                let _ = self.event_tx.send(Event::StreamError {
                    message: format!("SFU viewer connect failed: {e}"),
                });
                return;
            }
        };

        let peer_handle = {
            let ctx = self.voice.mello_ctx();
            unsafe { SfuConnection::create_stream_peer(ctx, StreamPeerRole::Viewer) }
        };
        let ph = match peer_handle {
            Ok(ph) => ph,
            Err(e) => {
                log::error!("SFU viewer peer creation failed: {}", e);
                let _ = self.event_tx.send(Event::StreamError {
                    message: format!("SFU viewer peer creation failed: {e}"),
                });
                return;
            }
        };

        if let Err(e) = conn.join_stream(ph, session_id, "viewer").await {
            log::error!("SFU viewer join_stream failed: {}", e);
            let _ = self.event_tx.send(Event::StreamError {
                message: format!("SFU viewer join failed: {e}"),
            });
            return;
        }

        if let Err(e) = conn.wait_for_datachannel_open().await {
            log::error!("SFU viewer stream transport failed: {}", e);
            let _ = self.event_tx.send(Event::StreamError {
                message: format!("SFU viewer transport failed: {e}"),
            });
            return;
        }

        log::info!("SFU viewer connected to session {}", session_id);
        let conn = Arc::new(conn);

        // Prefer actual encode resolution from watch_stream response (set by host
        // via update_stream_resolution RPC), fall back to crew-state UI values.
        let (w, h) = if resp.width > 0 && resp.height > 0 {
            log::info!(
                "SFU viewer using encode resolution from backend: {}x{}",
                resp.width,
                resp.height
            );
            (resp.width, resp.height)
        } else if stream_width > 0 && stream_height > 0 {
            log::warn!(
                "SFU viewer: no resolution from backend, using UI values: {}x{}",
                stream_width,
                stream_height
            );
            (stream_width, stream_height)
        } else {
            let config = crate::stream::StreamConfig::default();
            log::warn!(
                "SFU viewer: no resolution info, using default: {}x{}",
                config.width,
                config.height
            );
            (config.width, config.height)
        };

        let frame_cb_data = Box::into_raw(Box::new(FrameCallbackData {
            frame_slot: self.frame_slot.clone(),
            native_frame_slot: self.native_frame_slot.clone(),
            frame_consumed: self.frame_consumed.clone(),
            frame_lifecycle: self.frame_lifecycle.clone(),
            surface_frame_seq: self.surface_frame_seq.clone(),
        }));
        self.frame_lifecycle
            .store(FRAME_STATE_PRESENTED, std::sync::atomic::Ordering::Release);

        let mello_config = mello_sys::MelloStreamConfig {
            width: w,
            height: h,
            fps: crate::stream::StreamConfig::default().fps,
            bitrate_kbps: 0,
        };

        let ctx = self.voice.mello_ctx();
        #[cfg(target_os = "windows")]
        let frame_callback = None;
        #[cfg(not(target_os = "windows"))]
        let frame_callback = Some(on_viewer_frame as _);
        let viewer = unsafe {
            mello_sys::mello_stream_start_viewer(
                ctx,
                &mello_config,
                frame_callback,
                frame_cb_data as *mut std::ffi::c_void,
            )
        };
        if !viewer.is_null() {
            unsafe {
                mello_sys::mello_stream_set_native_frame_callback(
                    viewer,
                    Some(on_viewer_native_frame as _),
                    frame_cb_data as *mut std::ffi::c_void,
                );
            }
        }

        if viewer.is_null() {
            log::error!("Failed to start SFU stream viewer pipeline at {}x{}", w, h);
            let _ = self.event_tx.send(Event::StreamError {
                message: "Failed to start video decoder".to_string(),
            });
            unsafe {
                drop(Box::from_raw(frame_cb_data));
            }
            return;
        }

        log::info!("SFU viewer pipeline initialized at {}x{}", w, h);

        let _ = self.event_tx.send(Event::StreamWatching {
            host_id: host_id.to_string(),
            width: w,
            height: h,
        });

        let config = crate::stream::StreamConfig::default();
        let receive_bitrate_kbps = if resp.bitrate_kbps > 0 {
            resp.bitrate_kbps
        } else {
            log::warn!(
                "Legacy watch_stream response omitted bitrate; using default receive target {} kbps",
                config.bitrate_kbps
            );
            config.bitrate_kbps
        };
        self.viewer_state = Some(ViewerState {
            viewer: Some(viewer),
            peer: std::ptr::null_mut(),
            sfu_connection: Some(conn),
            mode: "sfu".to_string(),
            host_id: host_id.to_string(),
            _frame_cb_data: frame_cb_data,
            _ice_cb_data: std::ptr::null_mut(),
            _audio_cb_data: std::ptr::null_mut(),
            frames_presented: 0,
            stream_tick_count: 0,
            present_attempts: 0,
            present_forced_attempts: 0,
            present_skipped_unconsumed: 0,
            transport_packets: 0,
            transport_bytes: 0,
            au_buffer_grows: 0,
            au_poll_errors: 0,
            au_feed_failures: 0,
            backlog_guard_drops: 0,
            congestion: ViewerCongestionController::new(receive_bitrate_kbps, Codec::H264),
            debug_last_emit: Instant::now(),
            debug_last_tick_count: 0,
            debug_last_present_attempts: 0,
            debug_last_present_forced_attempts: 0,
            debug_last_present_skipped_unconsumed: 0,
            debug_last_packets: 0,
            debug_last_bytes: 0,
            debug_last_frames_presented: 0,
            debug_last_backlog_guard_drops: 0,
            last_present_attempt: Instant::now(),
            au_recv_buf: ViewerState::new_au_recv_buf(),
            audio_packets_received: 0,
            last_new_frame_at: Instant::now(),
            freeze_count: 0,
            freeze_ms_total: 0,
            in_freeze: false,
            freeze_accounted_ms: 0,
            stats_last_emit: Instant::now(),
            stats_last_frames_presented: 0,
            stats_last_freeze_count: 0,
            stats_last_freeze_ms: 0,
        });
    }

    /// P2P viewer path: create peer, signal offer, wait for answer.
    fn watch_stream_p2p(&mut self, host_id: &str, stream_width: u32, stream_height: u32) {
        let ctx = self.voice.mello_ctx();

        let peer = match create_stream_p2p_peer(ctx, host_id, PeerMediaRole::StreamViewer) {
            Some(peer) => peer.as_ptr(),
            None => {
                let _ = self.event_tx.send(Event::StreamError {
                    message: "Failed to create peer connection".to_string(),
                });
                return;
            }
        };

        let ice_cstrings: Vec<CString> = self
            .ice_servers
            .iter()
            .filter_map(|u| CString::new(u.as_str()).ok())
            .collect();
        if !ice_cstrings.is_empty() {
            let ptrs: Vec<*const std::os::raw::c_char> =
                ice_cstrings.iter().map(|s| s.as_ptr()).collect();
            unsafe {
                mello_sys::mello_peer_set_ice_servers(
                    peer,
                    ptrs.as_ptr() as *mut *const std::os::raw::c_char,
                    ptrs.len() as std::os::raw::c_int,
                );
            }
        }

        let ice_cb_data = Box::into_raw(Box::new(StreamIceCallbackData {
            peer_id: host_id.to_string(),
            send_queue: Arc::clone(&self.stream_signal_queue),
            disconnect_queue: Arc::clone(&self.stream_disconnect_queue),
            pending: std::sync::Mutex::new(Vec::new()),
            flushed: std::sync::atomic::AtomicBool::new(false),
        }));
        let audio_cb_data = Box::into_raw(Box::new(StreamAudioCallbackData {
            viewer_slot: std::sync::Mutex::new(None),
            packets_received: std::sync::atomic::AtomicU64::new(0),
        }));
        unsafe {
            mello_sys::mello_peer_set_ice_callback(
                peer,
                Some(stream_ice_callback),
                ice_cb_data as *mut std::ffi::c_void,
            );
            mello_sys::mello_peer_set_state_callback(
                peer,
                Some(stream_state_callback),
                ice_cb_data as *mut std::ffi::c_void,
            );
            mello_sys::mello_peer_set_audio_track_callback(
                peer,
                Some(stream_audio_track_callback),
                audio_cb_data as *mut std::ffi::c_void,
            );
        }

        let sdp_ptr = unsafe { mello_sys::mello_peer_create_offer(peer) };
        if sdp_ptr.is_null() {
            log::error!("Failed to create stream offer");
            unsafe {
                mello_sys::mello_peer_destroy(peer);
                drop(Box::from_raw(ice_cb_data));
                drop(Box::from_raw(audio_cb_data));
            }
            let _ = self.event_tx.send(Event::StreamError {
                message: "Failed to create stream offer".to_string(),
            });
            return;
        }
        let sdp = unsafe { CStr::from_ptr(sdp_ptr) }
            .to_string_lossy()
            .into_owned();
        log::info!("Created stream offer for host {}", host_id);

        if let Ok(mut queue) = self.stream_signal_queue.lock() {
            queue.push((
                host_id.to_string(),
                SignalEnvelope {
                    purpose: SignalPurpose::Stream,
                    stream_width: None,
                    stream_height: None,
                    stream_bitrate_kbps: None,
                    message: SignalMessage::Offer { sdp },
                },
            ));
        }
        unsafe {
            flush_ice_buffer(&*ice_cb_data);
        }

        let frame_cb_data = Box::into_raw(Box::new(FrameCallbackData {
            frame_slot: self.frame_slot.clone(),
            native_frame_slot: self.native_frame_slot.clone(),
            frame_consumed: self.frame_consumed.clone(),
            frame_lifecycle: self.frame_lifecycle.clone(),
            surface_frame_seq: self.surface_frame_seq.clone(),
        }));
        self.frame_lifecycle
            .store(FRAME_STATE_PRESENTED, std::sync::atomic::Ordering::Release);

        let _ = self.event_tx.send(Event::StreamWatching {
            host_id: host_id.to_string(),
            width: stream_width,
            height: stream_height,
        });

        let config = crate::stream::StreamConfig::default();
        self.viewer_state = Some(ViewerState {
            viewer: None,
            peer,
            sfu_connection: None,
            mode: "p2p".to_string(),
            host_id: host_id.to_string(),
            _frame_cb_data: frame_cb_data,
            _ice_cb_data: ice_cb_data,
            _audio_cb_data: audio_cb_data,
            frames_presented: 0,
            stream_tick_count: 0,
            present_attempts: 0,
            present_forced_attempts: 0,
            present_skipped_unconsumed: 0,
            transport_packets: 0,
            transport_bytes: 0,
            au_buffer_grows: 0,
            au_poll_errors: 0,
            au_feed_failures: 0,
            backlog_guard_drops: 0,
            congestion: ViewerCongestionController::new(config.bitrate_kbps, config.codec),
            debug_last_emit: Instant::now(),
            debug_last_tick_count: 0,
            debug_last_present_attempts: 0,
            debug_last_present_forced_attempts: 0,
            debug_last_present_skipped_unconsumed: 0,
            debug_last_packets: 0,
            debug_last_bytes: 0,
            debug_last_frames_presented: 0,
            debug_last_backlog_guard_drops: 0,
            last_present_attempt: Instant::now(),
            au_recv_buf: ViewerState::new_au_recv_buf(),
            audio_packets_received: 0,
            last_new_frame_at: Instant::now(),
            freeze_count: 0,
            freeze_ms_total: 0,
            in_freeze: false,
            freeze_accounted_ms: 0,
            stats_last_emit: Instant::now(),
            stats_last_frames_presented: 0,
            stats_last_freeze_count: 0,
            stats_last_freeze_ms: 0,
        });

        log::info!(
            "Stream viewer peer created, waiting for Answer from host {}",
            host_id
        );
    }

    pub(super) async fn handle_stop_watching(&mut self) {
        if let Some(vs) = self.viewer_state.take() {
            log::info!("Stopping stream viewer for host {}", vs.host_id);
            if let Some(ref conn) = vs.sfu_connection {
                conn.leave().await;
            }
            drop(vs);
            self.frame_lifecycle
                .store(FRAME_STATE_PRESENTED, std::sync::atomic::Ordering::Release);
            let _ = self.event_tx.send(Event::StreamWatchingStopped);
        }
    }
}

fn detach_disconnected_host_peer<F>(
    notice: &StreamPeerDisconnect,
    sink: Option<&P2PFanoutSink>,
    peers: &mut std::collections::HashMap<String, StreamHostPeer>,
    after_sink_remove: F,
) -> Option<StreamHostPeer>
where
    F: FnOnce(bool),
{
    let owns_notice = peers
        .get(&notice.peer_id)
        .is_some_and(|peer| peer.ice_cb_data as usize == notice.callback_data);
    if !owns_notice {
        return None;
    }

    if let Some(sink) = sink {
        sink.remove_viewer(&notice.peer_id);
    }
    after_sink_remove(peers.contains_key(&notice.peer_id));
    peers.remove(&notice.peer_id)
}

fn create_stream_p2p_peer(
    ctx: *mut mello_sys::MelloContext,
    peer_id: &str,
    role: PeerMediaRole,
) -> Option<NonNull<mello_sys::MelloPeerConnection>> {
    let ctx = NonNull::new(ctx)?;
    match rtp_peer::create_peer_for_role(ctx, peer_id, role) {
        Ok(peer) => Some(peer),
        Err(e) => {
            log::error!(
                "Failed to create stream {:?} peer for {}: {}",
                role,
                peer_id,
                e
            );
            None
        }
    }
}

fn video_stats_from_peer(
    peer: NonNull<mello_sys::MelloPeerConnection>,
) -> Result<mello_sys::MelloRtpVideoStats, ()> {
    let mut stats = unsafe { std::mem::zeroed::<mello_sys::MelloRtpVideoStats>() };
    unsafe { mello_sys::mello_peer_video_get_stats(peer.as_ptr(), &mut stats) };
    Ok(stats)
}

#[cfg(test)]
mod streaming_rtp_tests {
    use std::collections::HashMap;

    use super::{detach_disconnected_host_peer, StreamHostPeer, StreamPeerDisconnect};
    use crate::stream::rtp_peer::PeerMediaRole;
    use crate::stream::sink_p2p::P2PFanoutSink;
    use crate::transport::sfu_connection::StreamPeerRole;

    #[test]
    fn sfu_and_p2p_topology_roles_map_to_native_media_roles() {
        assert_eq!(
            StreamPeerRole::Host.to_media_role(),
            PeerMediaRole::StreamHost
        );
        assert_eq!(
            StreamPeerRole::Viewer.to_media_role(),
            PeerMediaRole::StreamViewer
        );
    }

    #[test]
    fn disconnected_peer_leaves_sink_before_client_drops_ownership() {
        let sink = P2PFanoutSink::new();
        let peer = std::ptr::dangling_mut::<mello_sys::MelloPeerConnection>();
        sink.add_viewer_for_test("viewer-1", peer);
        let callback_data = std::ptr::dangling_mut::<super::StreamIceCallbackData>();
        let mut peers = HashMap::from([(
            "viewer-1".to_string(),
            StreamHostPeer {
                peer,
                ice_cb_data: callback_data,
            },
        )]);
        let notice = StreamPeerDisconnect {
            peer_id: "viewer-1".to_string(),
            callback_data: callback_data as usize,
        };
        let mut observed_barrier = false;

        let detached =
            detach_disconnected_host_peer(&notice, Some(&sink), &mut peers, |still_owned| {
                assert_eq!(sink.viewer_count(), 0);
                assert!(still_owned);
                observed_barrier = true;
            });

        assert!(observed_barrier);
        assert!(detached.is_some());
        assert!(!peers.contains_key("viewer-1"));
    }

    #[test]
    fn sfu_host_membership_events_route_to_sink() {
        let src = include_str!("streaming.rs");
        assert!(src.contains("poll_sfu_host_membership_events"));
        assert!(src.contains("SfuEvent::MemberJoined"));
        assert!(src.contains("sink.on_viewer_joined"));
        assert!(src.contains("sink.on_viewer_left"));
        assert!(src.contains("stream_sfu_connection"));
    }
}
