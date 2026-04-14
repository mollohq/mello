use std::ffi::{CStr, CString};
use std::sync::Arc;
use std::time::Instant;

use crate::events::Event;
use crate::stream::sink_p2p::P2PFanoutSink;
use crate::stream::viewer::{ViewerAction, ViewerFeedResult};
use crate::voice::{SignalEnvelope, SignalMessage, SignalPurpose};

use super::stream_ffi::{
    flush_ice_buffer, on_viewer_frame, stream_ice_callback, stream_state_callback, ChunkAssembler,
    FrameCallbackData, StreamHostHandle, StreamHostPeer, StreamIceCallbackData, ViewerState,
};
use super::VIEWER_RECV_BUF_SIZE;

const STREAM_DEBUG_EVENT_INTERVAL_SECS: f32 = 1.0;
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

                // Create peer for this viewer
                let peer_id_c = match CString::new(from) {
                    Ok(c) => c,
                    Err(_) => return,
                };
                let peer = unsafe { mello_sys::mello_peer_create(ctx, peer_id_c.as_ptr()) };
                if peer.is_null() {
                    log::error!("Failed to create peer for stream viewer {}", from);
                    return;
                }

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
                            message: SignalMessage::Answer { sdp: answer },
                        },
                    ));
                }
                unsafe {
                    flush_ice_buffer(&*ice_cb_data);
                }

                // Add peer to P2PFanoutSink
                if let Some(ref sink) = self.stream_sink {
                    if let Err(e) = sink.add_viewer(from.to_string(), peer) {
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
        let vs = match self.viewer_state.as_ref() {
            Some(vs) => vs,
            None => return,
        };

        if from != vs.host_id {
            log::warn!(
                "Stream signal from {} but we're watching {} — ignoring",
                from,
                vs.host_id
            );
            return;
        }

        match envelope.message {
            SignalMessage::Answer { sdp } => {
                let sdp_c = match CString::new(sdp) {
                    Ok(c) => c,
                    Err(_) => return,
                };
                let peer = vs.peer;
                unsafe {
                    mello_sys::mello_peer_set_remote_description(peer, sdp_c.as_ptr(), false);
                }
                log::info!("Set stream remote answer from host {}", from);

                // Initialize the decoder pipeline now that we know the host's resolution
                if vs.viewer.is_none() {
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
                    let viewer = unsafe {
                        mello_sys::mello_stream_start_viewer(
                            ctx,
                            &mello_config,
                            Some(on_viewer_frame),
                            frame_cb_data as *mut std::ffi::c_void,
                        )
                    };

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
                    mello_sys::mello_peer_add_ice_candidate(vs.peer, &ice);
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

        self.emit_host_pacing_debug_stats().await;

        // 2. Poll viewer for incoming stream packets
        if self.viewer_state.is_none() {
            return;
        }

        let vs = self.viewer_state.as_mut().unwrap();
        let viewer = match vs.viewer {
            Some(v) => v,
            None => return, // Decoder not yet initialized (waiting for Answer)
        };
        let mut fed_any = false;

        // Collect raw packets from the transport (SFU or P2P)
        let packets: Vec<Vec<u8>> = if vs.mode == "sfu" {
            // SFU path: chunked DataChannel messages -> reassemble
            if let Some(ref conn) = vs.sfu_connection {
                let mut reassembled = Vec::new();
                for raw in conn.poll_recv() {
                    if let Some(full_msg) = vs.chunk_assembler.feed(&raw) {
                        reassembled.push(full_msg);
                    }
                }
                reassembled
            } else {
                Vec::new()
            }
        } else {
            // P2P path: chunked DataChannel messages → reassemble
            let peer = vs.peer;
            let mut reassembled = Vec::new();
            for _ in 0..512 {
                let size = unsafe {
                    mello_sys::mello_peer_recv(
                        peer,
                        vs.recv_buf.as_mut_ptr(),
                        vs.recv_buf.len() as i32,
                    )
                };
                if size <= 0 {
                    break;
                }
                if size as usize == vs.recv_buf.len() {
                    vs.transport_truncations = vs.transport_truncations.saturating_add(1);
                    if vs.transport_truncations <= 5 || vs.transport_truncations.is_multiple_of(100)
                    {
                        log::warn!(
                            "Stream recv likely truncated: size={} buf={} truncations={}",
                            size,
                            vs.recv_buf.len(),
                            vs.transport_truncations
                        );
                    }
                }
                let raw = &vs.recv_buf[..size as usize];
                if let Some(full_msg) = vs.chunk_assembler.feed(raw) {
                    reassembled.push(full_msg);
                }
            }
            reassembled
        };

        if !packets.is_empty() {
            let mut bytes = 0usize;
            for p in &packets {
                bytes += p.len();
            }
            vs.transport_packets = vs.transport_packets.saturating_add(packets.len() as u64);
            vs.transport_bytes = vs.transport_bytes.saturating_add(bytes as u64);
            if vs.transport_packets <= 10 || vs.transport_packets.is_multiple_of(500) {
                log::info!(
                    "Stream ingress: mode={} packets={} bytes={} total_packets={} total_bytes={} truncations={}",
                    vs.mode,
                    packets.len(),
                    bytes,
                    vs.transport_packets,
                    vs.transport_bytes,
                    vs.transport_truncations
                );
            }
        }

        for data in &packets {
            let results = vs.stream_viewer.feed_packet(data);

            for result in results {
                match result {
                    ViewerFeedResult::VideoPayload {
                        data: payload,
                        is_keyframe,
                    }
                    | ViewerFeedResult::RecoveredVideoPayload {
                        data: payload,
                        is_keyframe,
                    } => {
                        if !vs.got_keyframe {
                            if is_keyframe {
                                vs.got_keyframe = true;
                                log::info!("First keyframe received — stream decode starting");
                            } else {
                                continue;
                            }
                        }
                        let ok = unsafe {
                            mello_sys::mello_stream_feed_packet(
                                viewer,
                                payload.as_ptr(),
                                payload.len() as i32,
                                is_keyframe,
                            )
                        };
                        if !ok && is_keyframe {
                            log::warn!("feed_packet failed for keyframe ({} bytes)", payload.len());
                        }
                        fed_any = true;
                    }
                    ViewerFeedResult::AudioPayload(payload) => unsafe {
                        mello_sys::mello_stream_feed_audio_packet(
                            viewer,
                            payload.as_ptr(),
                            payload.len() as i32,
                        );
                    },
                    ViewerFeedResult::Action(ViewerAction::SendControl(ctrl_data)) => {
                        if vs.mode == "sfu" {
                            if let Some(ref conn) = vs.sfu_connection {
                                let _ = conn.send_control(&ctrl_data);
                            }
                        } else {
                            let peer = vs.peer;
                            let connected = unsafe { mello_sys::mello_peer_is_connected(peer) };
                            if connected {
                                unsafe {
                                    mello_sys::mello_peer_send_reliable(
                                        peer,
                                        ctrl_data.as_ptr(),
                                        ctrl_data.len() as i32,
                                    );
                                }
                            }
                        }
                    }
                    ViewerFeedResult::None => {}
                }
            }
        }

        // Present the latest decoded frame only if the UI has consumed the
        // previous one. This skips the entire GPU readback + memcpy chain when
        // decoding outpaces display (common at >30fps decode vs 60fps UI).
        if fed_any
            && self
                .frame_consumed
                .load(std::sync::atomic::Ordering::Acquire)
        {
            let presented = unsafe { mello_sys::mello_stream_present_frame(viewer) };
            if presented {
                vs.frames_presented += 1;
                if vs.frames_presented <= 3 || vs.frames_presented.is_multiple_of(300) {
                    log::info!("Stream frame presented #{}", vs.frames_presented);
                }
            }
        }

        // Poll SFU events to detect session_ended / host disconnect
        if let Some(ref conn) = vs.sfu_connection {
            for event in conn.poll_events() {
                if let crate::transport::SfuEvent::Disconnected { reason } = event {
                    log::info!("Stream SFU disconnected: {}", reason);
                    let _ = self.event_tx.send(Event::StreamWatchingStopped);
                    self.viewer_state.take();
                    return;
                }
            }
        }

        let elapsed = vs.debug_last_emit.elapsed().as_secs_f32();
        if elapsed >= STREAM_DEBUG_EVENT_INTERVAL_SECS {
            let delta_bytes = vs.transport_bytes.saturating_sub(vs.debug_last_bytes);
            let delta_frames = vs
                .frames_presented
                .saturating_sub(vs.debug_last_frames_presented);
            let ingress_kbps = (delta_bytes as f32 * 8.0 / 1000.0) / elapsed.max(0.001);
            let present_fps = (delta_frames as f32) / elapsed.max(0.001);

            let _ = self.event_tx.send(Event::StreamDebugStats {
                mode: vs.mode.clone(),
                transport_packets: vs.transport_packets,
                transport_bytes: vs.transport_bytes,
                transport_truncations: vs.transport_truncations,
                frames_presented: vs.frames_presented,
                present_fps,
                ingress_kbps,
            });

            vs.debug_last_emit = Instant::now();
            vs.debug_last_packets = vs.transport_packets;
            vs.debug_last_bytes = vs.transport_bytes;
            vs.debug_last_frames_presented = vs.frames_presented;
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

        let mut games_raw = vec![
            mello_sys::MelloGameProcess {
                pid: 0,
                name: [0i8; 128],
                exe: [0i8; 260],
                is_fullscreen: false,
            };
            32
        ];
        let game_count =
            unsafe { mello_sys::mello_enumerate_games(ctx, games_raw.as_mut_ptr(), 32) };
        let mut games = Vec::new();
        for game in games_raw.iter().take(game_count as usize) {
            let name = unsafe { std::ffi::CStr::from_ptr(game.name.as_ptr()) }
                .to_string_lossy()
                .to_string();
            let exe = unsafe { std::ffi::CStr::from_ptr(game.exe.as_ptr()) }
                .to_string_lossy()
                .to_string();
            games.push(crate::events::CaptureSource {
                id: format!("game-{}", game.pid),
                name,
                mode: "process".to_string(),
                monitor_index: None,
                hwnd: None,
                pid: Some(game.pid),
                exe,
                is_fullscreen: game.is_fullscreen,
                resolution: String::new(),
            });
        }

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
            "Enumerated capture sources: {} monitors, {} games, {} windows",
            monitors.len(),
            games.len(),
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
        _title: &str,
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

        // Step 1: async RPC call (no raw pointers held across await)
        let config = crate::stream::StreamConfig::from_preset(
            quality_preset,
            crate::stream::config::Codec::H264,
        );
        let resp = match crate::stream::host::request_start_stream(
            &self.nakama,
            crew_id,
            false, // supports_av1
            config.width,
            config.height,
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

            let source = match capture_mode {
                "window" => mello_sys::MelloCaptureSource {
                    mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_WINDOW,
                    monitor_index: 0,
                    hwnd: hwnd.unwrap_or(0) as *mut std::ffi::c_void,
                    pid: 0,
                },
                "process" => mello_sys::MelloCaptureSource {
                    mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_PROCESS,
                    monitor_index: 0,
                    hwnd: std::ptr::null_mut(),
                    pid: pid.unwrap_or(0),
                },
                _ => mello_sys::MelloCaptureSource {
                    mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_MONITOR,
                    monitor_index: monitor_index.unwrap_or(0),
                    hwnd: std::ptr::null_mut(),
                    pid: 0,
                },
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
            match crate::transport::SfuConnection::connect(endpoint, token).await {
                Ok(mut conn) => {
                    let peer_handle = {
                        let ctx = self.voice.mello_ctx();
                        unsafe { crate::transport::SfuConnection::create_peer(ctx) }
                    };
                    match peer_handle {
                        Ok(ph) => match conn.join_stream(ph, &resp.session_id(), "host").await {
                            Ok(_session) => {
                                if let Err(e) = conn.wait_for_datachannel_open().await {
                                    log::error!(
                                        "SFU DataChannel failed to open: {}, falling back to P2P",
                                        e
                                    );
                                    let p2p = Arc::new(P2PFanoutSink::new());
                                    (Arc::clone(&p2p) as _, Some(p2p))
                                } else {
                                    let conn = Arc::new(conn);
                                    let sfu_sink =
                                        Arc::new(crate::stream::sink_sfu::SfuSink::new(conn));
                                    (sfu_sink as _, None)
                                }
                            }
                            Err(e) => {
                                log::error!("SFU join_stream failed: {}, falling back to P2P", e);
                                let p2p = Arc::new(P2PFanoutSink::new());
                                (Arc::clone(&p2p) as _, Some(p2p))
                            }
                        },
                        Err(e) => {
                            log::error!("SFU peer creation failed: {}, falling back to P2P", e);
                            let p2p = Arc::new(P2PFanoutSink::new());
                            (Arc::clone(&p2p) as _, Some(p2p))
                        }
                    }
                }
                Err(e) => {
                    log::error!("SFU connect failed: {}, falling back to P2P", e);
                    let p2p = Arc::new(P2PFanoutSink::new());
                    (Arc::clone(&p2p) as _, Some(p2p))
                }
            }
        } else {
            let p2p = Arc::new(P2PFanoutSink::new());
            (Arc::clone(&p2p) as _, Some(p2p))
        };

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
        if let Some(mut session) = self.stream_session.take() {
            session.stop();

            // Destroy all host-side stream peers
            for (id, hp) in self.stream_host_peers.drain() {
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
            self.host_pacing_last = None;
            self.host_pacing_last_at = Instant::now();
            self.pending_remote_ice.clear();
            self.stream_encode_width = 0;
            self.stream_encode_height = 0;

            if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
                let payload = serde_json::json!({ "crew_id": crew_id });
                if let Err(e) = self.nakama.rpc("stop_stream", &payload).await {
                    log::warn!("stop_stream RPC failed: {}", e);
                }
                let _ = self.event_tx.send(Event::StreamEnded { crew_id });
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
        let watch_resp = if !session_id.is_empty() {
            match self.nakama.watch_stream(session_id).await {
                Ok(r) => {
                    log::info!("watch_stream RPC: mode={}", r.mode);
                    Some(r)
                }
                Err(e) => {
                    log::warn!("watch_stream RPC failed ({}), falling back to P2P", e);
                    None
                }
            }
        } else {
            log::info!("No session_id provided, using P2P viewer path");
            None
        };

        let use_sfu = watch_resp
            .as_ref()
            .map(|r| r.mode == "sfu")
            .unwrap_or(false);

        if use_sfu {
            self.watch_stream_sfu(
                host_id,
                session_id,
                stream_width,
                stream_height,
                &watch_resp.unwrap(),
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
                log::error!("SFU viewer connect failed: {}, falling back to P2P", e);
                self.watch_stream_p2p(host_id, stream_width, stream_height);
                return;
            }
        };

        let peer_handle = {
            let ctx = self.voice.mello_ctx();
            unsafe { crate::transport::SfuConnection::create_peer(ctx) }
        };
        let ph = match peer_handle {
            Ok(ph) => ph,
            Err(e) => {
                log::error!(
                    "SFU viewer peer creation failed: {}, falling back to P2P",
                    e
                );
                self.watch_stream_p2p(host_id, stream_width, stream_height);
                return;
            }
        };

        if let Err(e) = conn.join_stream(ph, session_id, "viewer").await {
            log::error!("SFU viewer join_stream failed: {}, falling back to P2P", e);
            self.watch_stream_p2p(host_id, stream_width, stream_height);
            return;
        }

        if let Err(e) = conn.wait_for_datachannel_open().await {
            log::error!("SFU viewer DataChannel failed: {}, falling back to P2P", e);
            self.watch_stream_p2p(host_id, stream_width, stream_height);
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
            frame_consumed: self.frame_consumed.clone(),
        }));

        let mello_config = mello_sys::MelloStreamConfig {
            width: w,
            height: h,
            fps: crate::stream::StreamConfig::default().fps,
            bitrate_kbps: 0,
        };

        let ctx = self.voice.mello_ctx();
        let viewer = unsafe {
            mello_sys::mello_stream_start_viewer(
                ctx,
                &mello_config,
                Some(on_viewer_frame),
                frame_cb_data as *mut std::ffi::c_void,
            )
        };

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
            width: stream_width,
            height: stream_height,
        });

        let config = crate::stream::StreamConfig::default();
        self.viewer_state = Some(ViewerState {
            viewer: Some(viewer),
            peer: std::ptr::null_mut(),
            sfu_connection: Some(conn),
            mode: "sfu".to_string(),
            host_id: host_id.to_string(),
            _frame_cb_data: frame_cb_data,
            _ice_cb_data: std::ptr::null_mut(),
            got_keyframe: false,
            frames_presented: 0,
            transport_packets: 0,
            transport_bytes: 0,
            transport_truncations: 0,
            debug_last_emit: Instant::now(),
            debug_last_packets: 0,
            debug_last_bytes: 0,
            debug_last_frames_presented: 0,
            recv_buf: vec![0u8; VIEWER_RECV_BUF_SIZE],
            stream_viewer: crate::stream::viewer::StreamViewer::new(config.fec_n),
            chunk_assembler: ChunkAssembler::new(),
        });
    }

    /// P2P viewer path: create peer, signal offer, wait for answer.
    fn watch_stream_p2p(&mut self, host_id: &str, stream_width: u32, stream_height: u32) {
        let ctx = self.voice.mello_ctx();

        let peer_id_c = match CString::new(host_id) {
            Ok(c) => c,
            Err(_) => return,
        };
        let peer = unsafe { mello_sys::mello_peer_create(ctx, peer_id_c.as_ptr()) };
        if peer.is_null() {
            log::error!("Failed to create peer connection for stream viewer");
            let _ = self.event_tx.send(Event::StreamError {
                message: "Failed to create peer connection".to_string(),
            });
            return;
        }

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

        let sdp_ptr = unsafe { mello_sys::mello_peer_create_offer(peer) };
        if sdp_ptr.is_null() {
            log::error!("Failed to create stream offer");
            unsafe {
                mello_sys::mello_peer_destroy(peer);
                drop(Box::from_raw(ice_cb_data));
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
                    message: SignalMessage::Offer { sdp },
                },
            ));
        }
        unsafe {
            flush_ice_buffer(&*ice_cb_data);
        }

        let config = crate::stream::StreamConfig::default();
        let frame_cb_data = Box::into_raw(Box::new(FrameCallbackData {
            frame_slot: self.frame_slot.clone(),
            frame_consumed: self.frame_consumed.clone(),
        }));

        let _ = self.event_tx.send(Event::StreamWatching {
            host_id: host_id.to_string(),
            width: stream_width,
            height: stream_height,
        });

        self.viewer_state = Some(ViewerState {
            viewer: None,
            peer,
            sfu_connection: None,
            mode: "p2p".to_string(),
            host_id: host_id.to_string(),
            _frame_cb_data: frame_cb_data,
            _ice_cb_data: ice_cb_data,
            got_keyframe: false,
            frames_presented: 0,
            transport_packets: 0,
            transport_bytes: 0,
            transport_truncations: 0,
            debug_last_emit: Instant::now(),
            debug_last_packets: 0,
            debug_last_bytes: 0,
            debug_last_frames_presented: 0,
            recv_buf: vec![0u8; VIEWER_RECV_BUF_SIZE],
            stream_viewer: crate::stream::viewer::StreamViewer::new(config.fec_n),
            chunk_assembler: ChunkAssembler::new(),
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
            let _ = self.event_tx.send(Event::StreamWatchingStopped);
        }
    }
}
