use tokio::sync::mpsc;

use crate::command::Command;
use crate::config::Config;
use crate::events::Event;
use crate::giphy::GiphyClient;
use crate::nakama::NakamaClient;
use crate::nakama::{InternalPresence, InternalSignal};
use crate::presence::PresenceStatus;
use crate::session;
use crate::stream::manager::StreamSession;
use crate::stream::sink_p2p::P2PFanoutSink;
use crate::stream::viewer::{StreamViewer, ViewerAction, ViewerFeedResult};
use crate::voice::{SignalEnvelope, SignalMessage, SignalPurpose, VoiceManager};

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::sync::Arc;

/// Individual chunks are at most ~60KB + 6 byte header.
const VIEWER_RECV_BUF_SIZE: usize = 64 * 1024;

/// Shared single-slot buffer for decoded stream frames. The C++ callback
/// overwrites the latest frame; the UI timer reads and takes it. This avoids
/// unbounded queue buildup that occurs when sending ~11 MB frames through a
/// channel at 30+ fps.
pub type FrameSlot = Arc<std::sync::Mutex<Option<(u32, u32, Vec<u8>)>>>;

struct FrameCallbackData {
    frame_slot: FrameSlot,
    /// Cleared by the callback after writing a frame, set by the UI after
    /// consuming it. When false, `present_frame` + the expensive GPU readback
    /// are skipped entirely.
    frame_consumed: Arc<std::sync::atomic::AtomicBool>,
}

/// Reassembles chunked DataChannel messages back into full StreamPackets.
struct ChunkAssembler {
    pending: HashMap<u16, ChunkAssembly>,
}

struct ChunkAssembly {
    chunk_count: u16,
    chunks_received: u16,
    chunks: Vec<Option<Vec<u8>>>,
}

impl ChunkAssembler {
    fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }

    /// Feed a raw DataChannel message. Returns the reassembled payload if complete.
    fn feed(&mut self, raw: &[u8]) -> Option<Vec<u8>> {
        use crate::stream::sink_p2p::CHUNK_HEADER_SIZE;
        if raw.len() < CHUNK_HEADER_SIZE {
            return None;
        }

        let msg_id = u16::from_le_bytes([raw[0], raw[1]]);
        let chunk_idx = u16::from_le_bytes([raw[2], raw[3]]);
        let chunk_count = u16::from_le_bytes([raw[4], raw[5]]);
        let payload = &raw[CHUNK_HEADER_SIZE..];

        if chunk_count == 0 || chunk_idx >= chunk_count {
            return None;
        }

        // Evict stale assemblies (keep only messages within a recent window)
        self.pending.retain(|&id, _| msg_id.wrapping_sub(id) < 64);

        let entry = self.pending.entry(msg_id).or_insert_with(|| ChunkAssembly {
            chunk_count,
            chunks_received: 0,
            chunks: (0..chunk_count).map(|_| None).collect(),
        });

        let idx = chunk_idx as usize;
        if idx < entry.chunks.len() && entry.chunks[idx].is_none() {
            entry.chunks[idx] = Some(payload.to_vec());
            entry.chunks_received += 1;
        }

        if entry.chunks_received == entry.chunk_count {
            let assembly = self.pending.remove(&msg_id).unwrap();
            let total: usize = assembly
                .chunks
                .iter()
                .map(|c| c.as_ref().map_or(0, |v| v.len()))
                .sum();
            let mut result = Vec::with_capacity(total);
            for data in assembly.chunks.into_iter().flatten() {
                result.extend_from_slice(&data);
            }
            Some(result)
        } else {
            None
        }
    }
}

/// State for the viewer-side streaming pipeline.
struct ViewerState {
    /// The C++ viewer pipeline handle. None until the host's Answer arrives
    /// with the actual encode resolution so we can initialize the decoder correctly.
    viewer: Option<*mut mello_sys::MelloStreamView>,
    /// P2P peer to host (only in P2P mode).
    peer: *mut mello_sys::MelloPeerConnection,
    /// SFU connection (only in SFU mode).
    sfu_connection: Option<Arc<crate::transport::SfuConnection>>,
    /// "sfu" or "p2p"
    mode: String,
    host_id: String,
    _frame_cb_data: *mut FrameCallbackData,
    _ice_cb_data: *mut StreamIceCallbackData,
    got_keyframe: bool,
    frames_presented: u64,
    recv_buf: Vec<u8>,
    stream_viewer: StreamViewer,
    chunk_assembler: ChunkAssembler,
}

unsafe impl Send for ViewerState {}
unsafe impl Sync for ViewerState {}

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
        }
        // SfuConnection is Arc-dropped automatically; leave() is called in handle_stop_watching
    }
}

struct StreamIceCallbackData {
    peer_id: String,
    send_queue: std::sync::Arc<std::sync::Mutex<Vec<(String, SignalEnvelope)>>>,
    /// ICE candidates gathered before the offer/answer is queued.
    /// Once `flushed` is true, new candidates go straight to `send_queue`.
    pending: std::sync::Mutex<Vec<SignalEnvelope>>,
    flushed: std::sync::atomic::AtomicBool,
}

struct StreamHostPeer {
    peer: *mut mello_sys::MelloPeerConnection,
    ice_cb_data: *mut StreamIceCallbackData,
}

unsafe impl Send for StreamHostPeer {}
unsafe impl Sync for StreamHostPeer {}

/// Send-safe wrapper for MelloStreamHost pointer, used to pass across async boundaries.
struct StreamHostHandle(*mut mello_sys::MelloStreamHost);
unsafe impl Send for StreamHostHandle {}

unsafe extern "C" fn on_viewer_frame(
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
    }
}

unsafe extern "C" fn stream_ice_callback(
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

unsafe extern "C" fn stream_state_callback(user_data: *mut std::ffi::c_void, state: i32) {
    if user_data.is_null() {
        return;
    }
    let data = &*(user_data as *const StreamIceCallbackData);
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
}

/// Flush buffered ICE candidates from a `StreamIceCallbackData` into the main
/// send queue. Must be called *after* the offer/answer has been pushed to `send_queue`.
/// Sets `flushed = true` so subsequent candidates go directly to the send queue.
fn flush_ice_buffer(cb_data: &StreamIceCallbackData) {
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

pub struct Client {
    nakama: NakamaClient,
    voice: VoiceManager,
    event_tx: std::sync::mpsc::Sender<Event>,
    frame_slot: FrameSlot,
    frame_consumed: Arc<std::sync::atomic::AtomicBool>,
    stream_session: Option<StreamSession>,
    stream_sink: Option<Arc<P2PFanoutSink>>,
    stream_host_peers: HashMap<String, StreamHostPeer>,
    viewer_state: Option<ViewerState>,
    stream_signal_queue: Arc<std::sync::Mutex<Vec<(String, SignalEnvelope)>>>,
    /// ICE candidates received before the peer was created (host side).
    pending_remote_ice: HashMap<String, Vec<SignalMessage>>,
    ice_servers: Vec<String>,
    /// Actual encode resolution (set after host pipeline starts).
    stream_encode_width: u32,
    stream_encode_height: u32,
    /// Stop signal for the thumbnail refresh thread.
    thumbnail_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Cached list of windows for thumbnail refresh.
    cached_windows: Vec<(String, u64)>,
    history_cursor: Option<String>,
    giphy: GiphyClient,
}

impl Client {
    pub fn new(
        config: Config,
        event_tx: std::sync::mpsc::Sender<Event>,
        loopback: bool,
        frame_slot: FrameSlot,
        frame_consumed: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            nakama: NakamaClient::new(config),
            voice: VoiceManager::new(event_tx.clone(), loopback),
            event_tx,
            frame_slot,
            frame_consumed,
            stream_session: None,
            stream_sink: None,
            stream_host_peers: HashMap::new(),
            viewer_state: None,
            stream_signal_queue: Arc::new(std::sync::Mutex::new(Vec::new())),
            stream_encode_width: 0,
            stream_encode_height: 0,
            pending_remote_ice: HashMap::new(),
            ice_servers: Vec::new(),
            thumbnail_stop: None,
            cached_windows: Vec::new(),
            history_cursor: None,
            giphy: GiphyClient::new(),
        }
    }

    pub async fn run(&mut self, mut cmd_rx: mpsc::Receiver<Command>) {
        log::info!("Mello client started, waiting for commands...");

        let mut signal_rx = self.nakama.take_signal_rx().unwrap();
        let mut presence_rx = self.nakama.take_presence_rx().unwrap();
        let mut voice_tick = tokio::time::interval(tokio::time::Duration::from_millis(20));
        // Refresh access token every 45 minutes (token lives 1 hour)
        let mut refresh_tick = tokio::time::interval(tokio::time::Duration::from_secs(45 * 60));
        refresh_tick.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(cmd) => self.handle_command(cmd).await,
                        None => break,
                    }
                }
                signal = signal_rx.recv() => {
                    if let Some(sig) = signal {
                        self.handle_signal(sig);
                    }
                }
                presence = presence_rx.recv() => {
                    if let Some(p) = presence {
                        self.handle_presence(p);
                    }
                }
                _ = voice_tick.tick() => {
                    self.voice_tick().await;
                    self.stream_tick().await;
                }
                _ = refresh_tick.tick() => {
                    self.refresh_token().await;
                }
            }
        }
        log::info!("Mello client shutting down");
    }

    fn handle_presence(&mut self, presence: InternalPresence) {
        if !self.voice.is_active() {
            return;
        }

        let local_id = match self.nakama.current_user_id() {
            Some(id) => id.to_string(),
            None => return,
        };

        match presence {
            InternalPresence::Joined { user_id } => {
                if user_id != local_id {
                    log::info!(
                        "Presence: member {} joined channel, adding to voice mesh",
                        user_id
                    );
                    self.voice.on_member_joined(&local_id, &user_id);
                }
            }
            InternalPresence::Left { user_id } => {
                if user_id != local_id {
                    log::info!(
                        "Presence: member {} left channel, removing from voice mesh",
                        user_id
                    );
                    self.voice.on_member_left(&user_id);
                }
            }
        }
    }

    fn handle_signal(&mut self, signal: InternalSignal) {
        match serde_json::from_str::<SignalEnvelope>(&signal.payload) {
            Ok(env) => match env.purpose {
                SignalPurpose::Voice => {
                    log::info!("Voice signal from {}: {:?}", signal.from, env.message);
                    self.voice.handle_signal(&signal.from, env.message);
                }
                SignalPurpose::Stream => {
                    log::info!("Stream signal from {}: {:?}", signal.from, env.message);
                    self.handle_stream_signal(&signal.from, env);
                }
            },
            Err(_) => {
                // Backward compat: try parsing as bare SignalMessage (no envelope)
                match serde_json::from_str::<SignalMessage>(&signal.payload) {
                    Ok(msg) => {
                        log::info!("Voice signal (legacy) from {}: {:?}", signal.from, msg);
                        self.voice.handle_signal(&signal.from, msg);
                    }
                    Err(e) => {
                        log::warn!("Failed to parse signal from {}: {}", signal.from, e);
                    }
                }
            }
        }
    }

    fn handle_stream_signal(&mut self, from: &str, envelope: SignalEnvelope) {
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

    async fn refresh_token(&mut self) {
        if let Some(rt) = self.nakama.refresh_token().map(String::from) {
            match self.nakama.refresh_session(&rt).await {
                Ok(user) => {
                    log::info!("Access token refreshed for {}", user.display_name);
                    if let Some(new_rt) = self.nakama.refresh_token() {
                        if let Err(e) = session::save(new_rt) {
                            log::warn!("Failed to save refreshed token: {}", e);
                        }
                    }
                }
                Err(e) => {
                    log::error!("Token refresh failed: {}", e);
                }
            }
        }
    }

    async fn voice_tick(&mut self) {
        self.voice.tick();

        // Send any pending signaling messages through Nakama
        let signals = self.voice.drain_signals();
        for (to, signal) in signals {
            let envelope = SignalEnvelope {
                purpose: SignalPurpose::Voice,
                stream_width: None,
                stream_height: None,
                message: signal,
            };
            let payload = match serde_json::to_string(&envelope) {
                Ok(p) => p,
                Err(e) => {
                    log::error!("Failed to serialize signal: {}", e);
                    continue;
                }
            };
            if let Err(e) = self.nakama.send_signal(&to, &payload).await {
                log::error!("Failed to send signal to {}: {}", to, e);
            }
        }
    }

    async fn stream_tick(&mut self) {
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
            // SFU path: raw StreamPacket bytes, no chunking
            if let Some(ref conn) = vs.sfu_connection {
                conn.poll_recv()
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
                let raw = &vs.recv_buf[..size as usize];
                if let Some(full_msg) = vs.chunk_assembler.feed(raw) {
                    reassembled.push(full_msg);
                }
            }
            reassembled
        };

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
                                    mello_sys::mello_peer_send_unreliable(
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
    }

    async fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::TryRestore => {
                self.handle_restore().await;
            }
            Command::DeviceAuth { device_id } => {
                self.handle_device_auth(&device_id).await;
            }
            Command::Login { email, password } => {
                self.handle_login(&email, &password).await;
            }
            Command::LinkEmail { email, password } => {
                self.handle_link_email(&email, &password).await;
            }
            Command::Logout => {
                self.handle_logout().await;
            }

            // Social auth
            Command::AuthSteam => {
                log::info!("[auth] Steam auth requested");
                // TODO: implemented by client/src/auth/steam.rs -> sends ticket to Nakama
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: "Steam auth not yet implemented".into(),
                });
            }
            Command::AuthGoogle => {
                log::info!("[auth] Google auth requested");
                self.handle_auth_google().await;
            }
            Command::AuthTwitch => {
                log::info!("[auth] Twitch auth requested");
                // TODO: OAuth2 PKCE flow -> access_token -> Nakama /authenticate/custom
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: "Twitch auth not yet implemented".into(),
                });
            }
            Command::AuthDiscord => {
                log::info!("[auth] Discord auth requested");
                self.handle_auth_discord().await;
            }
            Command::AuthApple => {
                log::info!("[auth] Apple auth requested");
                // TODO: Apple Sign In -> id_token -> Nakama /authenticate/apple
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: "Apple auth not yet implemented".into(),
                });
            }

            // Social link (onboarding — attaches identity to current device account)
            Command::LinkGoogle => {
                log::info!("[auth] Google link requested");
                self.handle_link_google().await;
            }
            Command::LinkDiscord => {
                log::info!("[auth] Discord link requested");
                self.handle_link_discord().await;
            }
            Command::DiscoverCrews { cursor } => {
                self.handle_discover_crews(cursor.as_deref()).await;
            }
            Command::FinalizeOnboarding {
                crew_id,
                crew_name,
                crew_description,
                crew_open,
                crew_avatar,
                display_name,
                avatar,
            } => {
                self.handle_finalize_onboarding(
                    crew_id,
                    crew_name,
                    crew_description,
                    crew_open,
                    crew_avatar,
                    &display_name,
                    avatar,
                )
                .await;
            }
            Command::LoadMyCrews => {
                self.load_crews().await;
            }
            Command::JoinCrew { crew_id } => {
                self.handle_join_crew(&crew_id).await;
            }
            Command::CreateCrew {
                name,
                description,
                open,
                avatar,
                invite_user_ids,
            } => {
                self.handle_create_crew(
                    &name,
                    &description,
                    open,
                    avatar.as_deref(),
                    &invite_user_ids,
                )
                .await;
            }
            Command::FetchCrewAvatars { crew_ids } => {
                self.handle_fetch_crew_avatars(&crew_ids).await;
            }
            Command::SearchUsers { query } => {
                self.handle_search_users(&query).await;
            }
            Command::JoinByInviteCode { code } => {
                self.handle_join_by_invite_code(&code).await;
            }
            Command::SelectCrew { crew_id } => {
                self.handle_select_crew(&crew_id).await;
            }
            Command::LeaveCrew => {
                self.handle_leave_crew().await;
            }
            Command::SendMessage { content, reply_to } => {
                self.handle_send_message(&content, reply_to.as_deref())
                    .await;
            }
            Command::SendGif { gif, body } => {
                self.handle_send_gif(gif, &body).await;
            }
            Command::EditMessage {
                message_id,
                new_body,
            } => {
                self.handle_edit_message(&message_id, &new_body).await;
            }
            Command::DeleteMessage { message_id } => {
                self.handle_delete_message(&message_id).await;
            }
            Command::LoadHistory { cursor } => {
                self.handle_load_history(cursor.as_deref()).await;
            }
            Command::SearchGifs { query } => {
                self.handle_search_gifs(&query).await;
            }
            Command::LoadTrendingGifs => {
                self.handle_trending_gifs().await;
            }
            Command::JoinVoice { channel_id } => {
                self.handle_join_voice(&channel_id).await;
            }
            Command::LeaveVoice => {
                self.handle_leave_voice().await;
            }
            Command::VoiceSpeaking { speaking } => {
                if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
                    log::debug!("voice_speaking RPC: crew={} speaking={}", crew_id, speaking);
                    if let Err(e) = self.nakama.voice_speaking(&crew_id, speaking).await {
                        log::warn!("voice_speaking RPC failed: {}", e);
                    }
                } else {
                    log::debug!("voice_speaking: no active crew");
                }
            }
            Command::SetMute { muted } => {
                self.voice.set_mute(muted);
            }
            Command::SetDeafen { deafened } => {
                self.voice.set_deafen(deafened);
            }
            Command::CheckMicPermission => {
                let status = unsafe { mello_sys::mello_mic_permission_status() };
                let granted = status == mello_sys::MelloMicPermission_MELLO_MIC_GRANTED;
                let denied = status == mello_sys::MelloMicPermission_MELLO_MIC_DENIED;
                let _ = self
                    .event_tx
                    .send(Event::MicPermissionChanged { granted, denied });
            }
            Command::RequestMicPermission => {
                let tx = self.event_tx.clone();
                unsafe extern "C" fn on_result(user_data: *mut std::ffi::c_void, granted: bool) {
                    let tx = Box::from_raw(user_data as *mut std::sync::mpsc::Sender<Event>);
                    let _ = tx.send(Event::MicPermissionChanged {
                        granted,
                        denied: !granted,
                    });
                }
                let tx_box = Box::new(tx);
                unsafe {
                    mello_sys::mello_mic_request_permission(
                        Some(on_result),
                        Box::into_raw(tx_box) as *mut std::ffi::c_void,
                    );
                }
            }
            Command::ListAudioDevices => {
                let capture = self.voice.list_capture_devices();
                let playback = self.voice.list_playback_devices();
                let _ = self
                    .event_tx
                    .send(Event::AudioDevicesListed { capture, playback });
            }
            Command::SetCaptureDevice { id } => {
                self.voice.set_capture_device(&id);
            }
            Command::SetPlaybackDevice { id } => {
                self.voice.set_playback_device(&id);
            }
            Command::SetLoopback { enabled } => {
                self.voice.set_loopback(enabled);
            }
            Command::SetDebugMode { enabled } => {
                self.voice.set_debug_mode(enabled);
            }
            Command::UpdateProfile { display_name } => {
                self.handle_update_profile(&display_name).await;
            }
            // --- Streaming ---
            Command::ListCaptureSources => {
                self.handle_list_capture_sources();
            }
            Command::StartThumbnailRefresh => {
                self.start_thumbnail_refresh();
            }
            Command::StopThumbnailRefresh => {
                self.stop_thumbnail_refresh();
            }
            Command::StartStream {
                crew_id,
                title,
                capture_mode,
                monitor_index,
                hwnd,
                pid,
                preset,
            } => {
                self.handle_start_stream(
                    &crew_id,
                    &title,
                    &capture_mode,
                    monitor_index,
                    hwnd,
                    pid,
                    preset,
                )
                .await;
            }
            Command::StopStream => {
                self.handle_stop_stream().await;
            }
            Command::WatchStream {
                host_id,
                session_id,
                width,
                height,
            } => {
                self.handle_watch_stream(&host_id, &session_id, width, height)
                    .await;
            }
            Command::StopWatching => {
                self.handle_stop_watching().await;
            }

            // --- Voice channels CRUD ---
            Command::CreateVoiceChannel { crew_id, name } => {
                self.handle_create_voice_channel(&crew_id, &name).await;
            }
            Command::RenameVoiceChannel {
                crew_id,
                channel_id,
                name,
            } => {
                self.handle_rename_voice_channel(&crew_id, &channel_id, &name)
                    .await;
            }
            Command::DeleteVoiceChannel {
                crew_id,
                channel_id,
            } => {
                self.handle_delete_voice_channel(&crew_id, &channel_id)
                    .await;
            }

            // --- Presence & crew state ---
            Command::UpdatePresence { status, activity } => {
                if let Err(e) = self
                    .nakama
                    .presence_update(&status, activity.as_ref())
                    .await
                {
                    log::error!("Failed to update presence: {}", e);
                }
            }
            Command::SetActiveCrew { crew_id } => {
                self.handle_set_active_crew(&crew_id).await;
            }
            Command::SubscribeSidebar { crew_ids } => {
                self.handle_subscribe_sidebar(&crew_ids).await;
            }
        }
    }

    async fn handle_device_auth(&mut self, device_id: &str) {
        match self.nakama.authenticate_device(device_id).await {
            Ok((user, created)) => {
                log::info!(
                    "Device auth succeeded for {} (created={})",
                    user.id,
                    created
                );
                if let Some(rt) = self.nakama.refresh_token() {
                    let _ = session::save(rt);
                }
                if let Err(e) = self.nakama.connect_ws(self.event_tx.clone()).await {
                    log::error!("WebSocket connect failed after device auth: {}", e);
                }
                self.on_connected().await;
                let _ = self.event_tx.send(Event::DeviceAuthed { user, created });
            }
            Err(e) => {
                log::error!("Device auth failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn handle_discover_crews(&self, cursor: Option<&str>) {
        match self.nakama.discover_crews_public(50, cursor).await {
            Ok((crews, next_cursor)) => {
                log::info!(
                    "[discover] loaded {} crews, has_more={}",
                    crews.len(),
                    next_cursor.is_some()
                );
                let _ = self.event_tx.send(Event::DiscoverCrewsLoaded {
                    crews,
                    cursor: next_cursor,
                });
            }
            Err(e) => {
                log::error!("Failed to discover crews: {}", e);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_finalize_onboarding(
        &mut self,
        crew_id: Option<String>,
        crew_name: Option<String>,
        crew_description: Option<String>,
        crew_open: Option<bool>,
        crew_avatar: Option<String>,
        display_name: &str,
        _avatar: u8,
    ) {
        let device_id = {
            use rand::Rng;
            let bytes: [u8; 16] = rand::thread_rng().gen();
            bytes
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        };
        log::info!(
            "[onboarding] finalizing — device auth with id={}",
            device_id
        );

        let (user, _created) = match self.nakama.authenticate_device(&device_id).await {
            Ok(pair) => pair,
            Err(e) => {
                log::error!("[onboarding] device auth failed: {}", e);
                let _ = self.event_tx.send(Event::OnboardingFailed {
                    reason: format!("Account creation failed: {}", e),
                });
                return;
            }
        };

        if let Some(rt) = self.nakama.refresh_token() {
            let _ = session::save(rt);
        }

        if let Err(e) = self.nakama.connect_ws(self.event_tx.clone()).await {
            log::error!("[onboarding] WebSocket connect failed: {}", e);
            let _ = self.event_tx.send(Event::OnboardingFailed {
                reason: format!("Connection failed: {}", e),
            });
            return;
        }

        self.on_connected().await;

        if !display_name.is_empty() {
            if let Err(e) = self.nakama.update_account(display_name).await {
                log::warn!("[onboarding] failed to set display name: {}", e);
            }
        }

        // TODO: persist avatar in user metadata once supported

        let final_crew_id = if let Some(id) = crew_id {
            if let Err(e) = self.nakama.join_group(&id).await {
                log::error!("[onboarding] failed to join crew {}: {}", id, e);
                let _ = self.event_tx.send(Event::OnboardingFailed {
                    reason: format!("Failed to join crew: {}", e),
                });
                return;
            }
            Some(id)
        } else if let Some(name) = crew_name {
            match self
                .nakama
                .create_crew(
                    &name,
                    crew_description.as_deref().unwrap_or(""),
                    crew_open.unwrap_or(true),
                    crew_avatar.as_deref(),
                    &[],
                )
                .await
            {
                Ok((crew, _invite_code)) => {
                    let id = crew.id.clone();
                    let _ = self.event_tx.send(Event::CrewCreated {
                        crew,
                        invite_code: None,
                    });
                    Some(id)
                }
                Err(e) => {
                    log::error!("[onboarding] failed to create crew: {}", e);
                    let _ = self.event_tx.send(Event::OnboardingFailed {
                        reason: format!("Failed to create crew: {}", e),
                    });
                    return;
                }
            }
        } else {
            None
        };

        if let Some(ref cid) = final_crew_id {
            self.handle_select_crew(cid).await;
        }

        let mut updated_user = user;
        updated_user.display_name = display_name.to_string();
        let _ = self
            .event_tx
            .send(Event::OnboardingReady { user: updated_user });
    }

    async fn handle_join_crew(&mut self, crew_id: &str) {
        if let Err(e) = self.nakama.join_group(crew_id).await {
            log::error!("Failed to join crew {}: {}", crew_id, e);
            let _ = self.event_tx.send(Event::Error {
                message: format!("Failed to join crew: {}", e),
            });
            return;
        }
        self.handle_select_crew(crew_id).await;
        self.load_crews().await;
    }

    async fn handle_link_email(&mut self, email: &str, password: &str) {
        match self.nakama.link_email(email, password).await {
            Ok(()) => {
                log::info!("Email linked successfully");
                let _ = self.event_tx.send(Event::EmailLinked);
            }
            Err(e) => {
                log::error!("Email link failed: {}", e);
                let _ = self.event_tx.send(Event::EmailLinkFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn handle_restore(&mut self) {
        let token = match session::load() {
            Some(t) => {
                log::info!("Found stored refresh token, attempting restore...");
                t
            }
            None => {
                log::info!("No stored session found");
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: String::new(),
                });
                return;
            }
        };

        let _ = self.event_tx.send(Event::Restoring);

        match self.nakama.refresh_session(&token).await {
            Ok(user) => {
                log::info!("Session restored for {}", user.display_name);

                if let Some(new_rt) = self.nakama.refresh_token() {
                    let _ = session::save(new_rt);
                }

                if let Err(e) = self.nakama.connect_ws(self.event_tx.clone()).await {
                    log::error!("WebSocket connect failed on restore: {}", e);
                    session::clear();
                    let _ = self.event_tx.send(Event::LoginFailed {
                        reason: format!("WebSocket failed: {}", e),
                    });
                    return;
                }

                self.on_connected().await;
                let _ = self.event_tx.send(Event::LoggedIn { user });
                self.load_crews().await;
            }
            Err(e) => {
                log::warn!("Session restore failed ({}), clearing", e);
                session::clear();
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: String::new(),
                });
            }
        }
    }

    async fn handle_login(&mut self, email: &str, password: &str) {
        match self.nakama.login_email(email, password).await {
            Ok(user) => {
                log::info!("Logged in as {} ({})", user.display_name, user.tag);

                match self.nakama.refresh_token() {
                    Some(rt) => {
                        log::info!("Saving refresh token to keyring");
                        if let Err(e) = session::save(rt) {
                            log::warn!("Failed to save session: {}", e);
                        }
                    }
                    None => {
                        log::warn!("No refresh token returned by server");
                    }
                }

                if let Err(e) = self.nakama.connect_ws(self.event_tx.clone()).await {
                    log::error!("WebSocket connect failed: {}", e);
                    let _ = self.event_tx.send(Event::LoginFailed {
                        reason: format!("WebSocket failed: {}", e),
                    });
                    return;
                }

                self.on_connected().await;
                let _ = self.event_tx.send(Event::LoggedIn { user });
                self.load_crews().await;
            }
            Err(e) => {
                log::error!("Login failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn handle_auth_google(&mut self) {
        let client_id = match self.nakama.config().google_client_id.clone() {
            Some(id) => id,
            None => {
                log::warn!("[auth] GOOGLE_CLIENT_ID not configured");
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: "Google login not configured".into(),
                });
                return;
            }
        };

        let oauth_result = tokio::task::spawn_blocking(move || {
            crate::auth_google::GoogleAuth::authenticate(&client_id)
        })
        .await;

        let (code, verifier) = match oauth_result {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                log::error!("[auth] Google OAuth flow failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: format!("Google sign-in failed: {}", e),
                });
                return;
            }
            Err(e) => {
                log::error!("[auth] Google OAuth task panicked: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: "Google sign-in failed unexpectedly".into(),
                });
                return;
            }
        };

        let id_token = match self.nakama.google_exchange_code(&code, &verifier).await {
            Ok(t) => t,
            Err(e) => {
                log::error!("[auth] Google token exchange failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: e.to_string(),
                });
                return;
            }
        };

        match self.nakama.authenticate_google(&id_token).await {
            Ok(user) => self.on_social_login(user).await,
            Err(e) => {
                log::error!("[auth] Google Nakama auth failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn handle_auth_discord(&mut self) {
        let client_id = match self.nakama.config().discord_client_id.clone() {
            Some(id) => id,
            None => {
                log::warn!("[auth] DISCORD_CLIENT_ID not configured");
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: "Discord login not configured".into(),
                });
                return;
            }
        };

        let oauth_result = tokio::task::spawn_blocking(move || {
            crate::auth_discord::DiscordAuth::authenticate(&client_id)
        })
        .await;

        let token = match oauth_result {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                log::error!("[auth] Discord OAuth flow failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: format!("Discord sign-in failed: {}", e),
                });
                return;
            }
            Err(e) => {
                log::error!("[auth] Discord OAuth task panicked: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: "Discord sign-in failed unexpectedly".into(),
                });
                return;
            }
        };

        match self.nakama.authenticate_custom(&token, "discord").await {
            Ok(user) => self.on_social_login(user).await,
            Err(e) => {
                log::error!("[auth] Discord Nakama auth failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    /// Shared post-auth flow for social logins (same as handle_login success path).
    async fn on_social_login(&mut self, user: crate::events::User) {
        log::info!(
            "[auth] Social login success: {} ({})",
            user.display_name,
            user.tag
        );

        match self.nakama.refresh_token() {
            Some(rt) => {
                if let Err(e) = session::save(rt) {
                    log::warn!("Failed to save session: {}", e);
                }
            }
            None => {
                log::warn!("No refresh token returned by server");
            }
        }

        if let Err(e) = self.nakama.connect_ws(self.event_tx.clone()).await {
            log::error!("WebSocket connect failed: {}", e);
            let _ = self.event_tx.send(Event::LoginFailed {
                reason: format!("WebSocket failed: {}", e),
            });
            return;
        }

        self.on_connected().await;
        let _ = self.event_tx.send(Event::LoggedIn { user });
        self.load_crews().await;
    }

    async fn handle_link_google(&mut self) {
        let client_id = match self.nakama.config().google_client_id.clone() {
            Some(id) => id,
            None => {
                log::warn!("[auth] GOOGLE_CLIENT_ID not configured");
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: "Google login not configured".into(),
                });
                return;
            }
        };

        let oauth_result = tokio::task::spawn_blocking(move || {
            crate::auth_google::GoogleAuth::authenticate(&client_id)
        })
        .await;

        let (code, verifier) = match oauth_result {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                log::error!("[auth] Google OAuth flow failed: {}", e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: format!("Google sign-in failed: {}", e),
                });
                return;
            }
            Err(e) => {
                log::error!("[auth] Google OAuth task panicked: {}", e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: "Google sign-in failed unexpectedly".into(),
                });
                return;
            }
        };

        let id_token = match self.nakama.google_exchange_code(&code, &verifier).await {
            Ok(t) => t,
            Err(e) => {
                log::error!("[auth] Google token exchange failed: {}", e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: e.to_string(),
                });
                return;
            }
        };

        match self.nakama.link_google(&id_token).await {
            Ok(()) => {
                log::info!("[auth] Google identity linked to device account");
                let _ = self.event_tx.send(Event::SocialLinked);
            }
            Err(e) if e.to_string().contains("already in use") => {
                log::info!("[auth] Google already linked elsewhere, falling back to authenticate");
                match self.nakama.authenticate_google(&id_token).await {
                    Ok(user) => self.on_social_login(user).await,
                    Err(e2) => {
                        log::error!("[auth] Google authenticate fallback failed: {}", e2);
                        let _ = self.event_tx.send(Event::SocialLinkFailed {
                            reason: e2.to_string(),
                        });
                    }
                }
            }
            Err(e) => {
                log::error!("[auth] Google link failed: {}", e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn handle_link_discord(&mut self) {
        let client_id = match self.nakama.config().discord_client_id.clone() {
            Some(id) => id,
            None => {
                log::warn!("[auth] DISCORD_CLIENT_ID not configured");
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: "Discord login not configured".into(),
                });
                return;
            }
        };

        let oauth_result = tokio::task::spawn_blocking(move || {
            crate::auth_discord::DiscordAuth::authenticate(&client_id)
        })
        .await;

        let token = match oauth_result {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                log::error!("[auth] Discord OAuth flow failed: {}", e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: format!("Discord sign-in failed: {}", e),
                });
                return;
            }
            Err(e) => {
                log::error!("[auth] Discord OAuth task panicked: {}", e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: "Discord sign-in failed unexpectedly".into(),
                });
                return;
            }
        };

        match self.nakama.link_custom(&token, "discord").await {
            Ok(()) => {
                log::info!("[auth] Discord identity linked to device account");
                let _ = self.event_tx.send(Event::SocialLinked);
            }
            Err(e) if e.to_string().contains("already in use") => {
                log::info!("[auth] Discord already linked elsewhere, falling back to authenticate");
                match self.nakama.authenticate_custom(&token, "discord").await {
                    Ok(user) => self.on_social_login(user).await,
                    Err(e2) => {
                        log::error!("[auth] Discord authenticate fallback failed: {}", e2);
                        let _ = self.event_tx.send(Event::SocialLinkFailed {
                            reason: e2.to_string(),
                        });
                    }
                }
            }
            Err(e) => {
                log::error!("[auth] Discord link failed: {}", e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn handle_update_profile(&self, display_name: &str) {
        match self.nakama.update_account(display_name).await {
            Ok(()) => {
                log::info!("Profile updated: display_name={}", display_name);
            }
            Err(e) => {
                log::error!("Failed to update profile: {}", e);
            }
        }
    }

    async fn handle_logout(&mut self) {
        // Notify server we're going offline
        if let Err(e) = self
            .nakama
            .presence_update(&PresenceStatus::Offline, None)
            .await
        {
            log::warn!("Failed to set offline presence on logout: {}", e);
        }

        // Leave voice (local + server-side)
        self.sfu_leave_if_connected().await;
        if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
            if let Err(e) = self.nakama.voice_leave(&crew_id).await {
                log::warn!("Failed to voice_leave RPC on logout: {}", e);
            }
        }
        self.voice.leave_voice();
        let _ = self
            .event_tx
            .send(Event::VoiceStateChanged { in_call: false });

        session::clear();
        if let Err(e) = self.nakama.leave_crew_channel().await {
            log::warn!("Leave channel on logout: {}", e);
        }
        log::info!("Logged out, session cleared");
    }

    async fn load_crews(&self) {
        match self.nakama.list_user_groups().await {
            Ok(crews) => {
                // Subscribe sidebar for all crews
                let crew_ids: Vec<String> = crews.iter().map(|c| c.id.clone()).collect();
                if !crew_ids.is_empty() {
                    self.handle_subscribe_sidebar(&crew_ids).await;
                }
                let _ = self.event_tx.send(Event::CrewsLoaded { crews });
            }
            Err(e) => {
                log::error!("Failed to load crews: {}", e);
            }
        }
    }

    async fn handle_create_crew(
        &mut self,
        name: &str,
        description: &str,
        open: bool,
        avatar: Option<&str>,
        invite_user_ids: &[String],
    ) {
        log::info!(
            "[crew] creating crew name={:?} open={} has_avatar={} invite_count={}",
            name,
            open,
            avatar.is_some(),
            invite_user_ids.len()
        );
        if let Some(a) = avatar {
            log::info!("[crew] avatar payload: {} bytes base64", a.len());
        }
        match self
            .nakama
            .create_crew(name, description, open, avatar, invite_user_ids)
            .await
        {
            Ok((crew, invite_code)) => {
                log::info!(
                    "[crew] created crew id={} name={:?} invite_code={:?}",
                    crew.id,
                    crew.name,
                    invite_code
                );
                let crew_id = crew.id.clone();
                let _ = self.event_tx.send(Event::CrewCreated { crew, invite_code });
                self.handle_select_crew(&crew_id).await;
                self.load_crews().await;
            }
            Err(e) => {
                log::error!("[crew] failed to create crew: {}", e);
                let _ = self.event_tx.send(Event::CrewCreateFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn handle_search_users(&self, query: &str) {
        log::debug!("[search] searching users query={:?}", query);
        match self.nakama.search_users(query).await {
            Ok(users) => {
                log::debug!("[search] found {} users for query={:?}", users.len(), query);
                let _ = self.event_tx.send(Event::UserSearchResults { users });
            }
            Err(e) => {
                log::warn!("[search] user search failed for query={:?}: {}", query, e);
                let _ = self
                    .event_tx
                    .send(Event::UserSearchResults { users: vec![] });
            }
        }
    }

    async fn handle_join_by_invite_code(&mut self, code: &str) {
        log::info!("[invite] joining crew by invite code={:?}", code);
        match self.nakama.join_by_invite_code(code).await {
            Ok((crew_id, name)) => {
                log::info!(
                    "[invite] joined crew id={} name={:?} via invite code",
                    crew_id,
                    name
                );
                let _ = self.event_tx.send(Event::CrewJoined {
                    crew_id: crew_id.clone(),
                });
                self.handle_select_crew(&crew_id).await;
                self.load_crews().await;
            }
            Err(e) => {
                log::error!("[invite] failed to join by invite code: {}", e);
                let _ = self.event_tx.send(Event::Error {
                    message: format!("Invalid invite code: {}", e),
                });
            }
        }
    }

    async fn handle_fetch_crew_avatars(&self, crew_ids: &[String]) {
        log::info!("[avatar] fetching avatars for {} crews", crew_ids.len());
        for crew_id in crew_ids {
            match self.nakama.get_crew_avatar(crew_id).await {
                Ok(raw) if !raw.is_empty() => {
                    // RPC returns the storage value JSON: {"data":"base64..."}
                    let data = serde_json::from_str::<serde_json::Value>(&raw)
                        .ok()
                        .and_then(|v| v.get("data")?.as_str().map(String::from))
                        .unwrap_or(raw);
                    log::info!(
                        "[avatar] loaded avatar for crew {} ({} bytes)",
                        crew_id,
                        data.len()
                    );
                    let _ = self.event_tx.send(Event::CrewAvatarLoaded {
                        crew_id: crew_id.clone(),
                        data,
                    });
                }
                Ok(_) => {
                    log::debug!("[avatar] no avatar data for crew {}", crew_id);
                }
                Err(e) => {
                    log::warn!(
                        "[avatar] failed to fetch avatar for crew {}: {}",
                        crew_id,
                        e
                    );
                }
            }
        }
    }

    async fn handle_select_crew(&mut self, crew_id: &str) {
        self.sfu_leave_if_connected().await;
        self.voice.leave_voice();
        let _ = self
            .event_tx
            .send(Event::VoiceStateChanged { in_call: false });

        if let Err(e) = self.nakama.leave_crew_channel().await {
            log::warn!("Failed to leave previous channel: {}", e);
        }

        if let Err(e) = self.nakama.join_crew_channel(crew_id).await {
            log::error!("Failed to join crew channel: {}", e);
            return;
        }

        let _ = self.event_tx.send(Event::CrewJoined {
            crew_id: crew_id.to_string(),
        });

        // Tell the server this is our active crew (registers subscription + returns state)
        let local_user_id = self
            .nakama
            .current_user_id()
            .map(String::from)
            .unwrap_or_default();
        let voice_channel_id = match self.nakama.set_active_crew(crew_id).await {
            Ok(state) => {
                // Check if user is already in a channel (server remembers from last session)
                let already_in = state
                    .voice_channels
                    .iter()
                    .find(|ch| ch.members.iter().any(|m| m.user_id == local_user_id))
                    .map(|ch| ch.id.clone());
                // Fall back to default channel
                let target = already_in.or_else(|| {
                    state
                        .voice_channels
                        .iter()
                        .find(|ch| ch.is_default)
                        .or_else(|| state.voice_channels.first())
                        .map(|ch| ch.id.clone())
                });
                let _ = self.event_tx.send(Event::CrewStateLoaded { state });
                target
            }
            Err(e) => {
                log::warn!("set_active_crew RPC failed: {}", e);
                None
            }
        };

        // Fetch members first so the display name cache is populated for chat messages
        if let Ok(members) = self.nakama.list_group_users(crew_id).await {
            let user_ids: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
            if let Err(e) = self.nakama.follow_users(&user_ids).await {
                log::warn!("Failed to follow users: {}", e);
            }
        }

        // Wait for WS reader to set channel_id (up to 2s)
        let channel_id = self.wait_for_channel_id().await;
        if let Some(ch_id) = channel_id {
            match self
                .nakama
                .list_channel_messages_with_cursor(&ch_id, 50, None)
                .await
            {
                Ok((mut messages, cursor)) => {
                    messages.reverse();
                    self.history_cursor = cursor;
                    let _ = self.event_tx.send(Event::MessagesLoaded { messages });
                }
                Err(e) => log::error!("Failed to fetch message history: {}", e),
            }
        }

        // Auto-join voice (last-used channel, or default if first time)
        if let Some(ch_id) = &voice_channel_id {
            self.handle_join_voice(ch_id).await;
        }
    }

    /// Called after successful auth + WS connect. Sets online presence and fetches ICE config.
    async fn on_connected(&mut self) {
        if let Err(e) = self
            .nakama
            .presence_update(&PresenceStatus::Online, None)
            .await
        {
            log::warn!("Failed to set online presence: {}", e);
        }

        self.check_protocol_version().await;

        match self.nakama.get_ice_servers().await {
            Ok(urls) => {
                log::info!("Fetched {} ICE server(s) from backend", urls.len());
                self.ice_servers = urls.clone();
                self.voice.set_ice_servers(urls);
            }
            Err(e) => {
                log::warn!("Failed to fetch ICE servers, using defaults: {}", e);
            }
        }
    }

    async fn check_protocol_version(&self) {
        match self.nakama.health_check().await {
            Ok(health) => {
                log::info!(
                    "Server health: status={} version={} protocol={}",
                    health.status,
                    health.version,
                    health.protocol_version.unwrap_or(0),
                );

                if let Some(min_client) = health.min_client_protocol {
                    if crate::PROTOCOL_VERSION < min_client {
                        let msg = format!(
                            "Server requires protocol {} but client speaks {}. Please update Mello.",
                            min_client, crate::PROTOCOL_VERSION,
                        );
                        log::warn!("{}", msg);
                        let _ = self.event_tx.send(Event::ProtocolMismatch {
                            message: msg,
                            client_outdated: true,
                        });
                    }
                }

                if let Some(server_proto) = health.protocol_version {
                    if server_proto < crate::MIN_SERVER_PROTOCOL {
                        let msg = format!(
                            "Client requires server protocol {} but server speaks {}. Server needs updating.",
                            crate::MIN_SERVER_PROTOCOL, server_proto,
                        );
                        log::warn!("{}", msg);
                        let _ = self.event_tx.send(Event::ProtocolMismatch {
                            message: msg,
                            client_outdated: false,
                        });
                    }
                }
            }
            Err(e) => {
                log::warn!("Health check failed (server may be old): {}", e);
            }
        }
    }

    /// Tell the server which crew is active and get full state back.
    async fn handle_set_active_crew(&self, crew_id: &str) {
        match self.nakama.set_active_crew(crew_id).await {
            Ok(state) => {
                let _ = self.event_tx.send(Event::CrewStateLoaded { state });
            }
            Err(e) => {
                log::error!("set_active_crew failed: {}", e);
            }
        }
    }

    /// Subscribe to sidebar updates for the given crews.
    async fn handle_subscribe_sidebar(&self, crew_ids: &[String]) {
        match self.nakama.subscribe_sidebar(crew_ids).await {
            Ok(crews) => {
                let _ = self.event_tx.send(Event::SidebarUpdated { crews });
            }
            Err(e) => {
                log::warn!("subscribe_sidebar failed: {}", e);
            }
        }
    }

    async fn wait_for_channel_id(&self) -> Option<String> {
        for _ in 0..20 {
            if let Some(id) = self.nakama.channel_id().await {
                return Some(id);
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        log::warn!("Timed out waiting for channel_id");
        None
    }

    async fn handle_join_voice(&mut self, channel_id: &str) {
        let crew_id = match self.nakama.active_crew_id().map(String::from) {
            Some(id) => id,
            None => return,
        };

        // RPC returns the authoritative channel state after join
        let resp = match self.nakama.voice_join(&crew_id, channel_id).await {
            Ok(r) => r,
            Err(e) => {
                log::error!("voice_join RPC failed: {}", e);
                return;
            }
        };

        let mode = resp.mode.as_deref().unwrap_or("p2p");

        // Emit authoritative state immediately so the UI shows the initial member list.
        // Must happen BEFORE the SFU connection (which can take seconds), otherwise
        // VoiceChannelsUpdated notifications that arrive during connection get overwritten.
        let _ = self.event_tx.send(Event::VoiceJoined {
            crew_id: crew_id.clone(),
            channel_id: resp.channel_id.clone(),
            members: resp.voice_state.members.clone(),
        });

        self.sfu_leave_if_connected().await;
        self.voice.leave_voice();
        if let Some(local_id) = self.nakama.current_user_id().map(String::from) {
            match mode {
                "sfu" => {
                    let endpoint = resp.sfu_endpoint.as_deref().unwrap_or_default();
                    let token = resp.sfu_token.as_deref().unwrap_or_default();

                    let fallback_to_p2p =
                        |voice: &mut crate::voice::VoiceManager,
                         local_id: &str,
                         resp: &crate::crew_state::VoiceJoinResponse| {
                            let peer_ids: Vec<String> = resp
                                .voice_state
                                .members
                                .iter()
                                .filter(|m| m.user_id != local_id)
                                .map(|m| m.user_id.clone())
                                .collect();
                            voice.join_voice(local_id, &peer_ids);
                        };

                    match crate::transport::SfuConnection::connect(endpoint, token).await {
                        Ok(mut conn) => {
                            let peer_handle = {
                                let ctx = self.voice.mello_ctx();
                                unsafe { crate::transport::SfuConnection::create_peer(ctx) }
                            };
                            match peer_handle {
                                Ok(ph) => match conn.join_voice(ph, &crew_id, channel_id).await {
                                    Ok(_session) => {
                                        if let Err(e) = conn.wait_for_datachannel_open().await {
                                            log::error!(
                                                "SFU DataChannel failed to open: {}, falling back to P2P",
                                                e
                                            );
                                            fallback_to_p2p(&mut self.voice, &local_id, &resp);
                                        } else {
                                            let conn = std::sync::Arc::new(conn);
                                            self.voice.join_voice_sfu(&local_id, &crew_id, conn);
                                        }
                                    }
                                    Err(e) => {
                                        log::error!(
                                            "SFU voice join failed: {}, falling back to P2P",
                                            e
                                        );
                                        fallback_to_p2p(&mut self.voice, &local_id, &resp);
                                    }
                                },
                                Err(e) => {
                                    log::error!(
                                        "SFU peer creation failed: {}, falling back to P2P",
                                        e
                                    );
                                    fallback_to_p2p(&mut self.voice, &local_id, &resp);
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("SFU connect failed: {}, falling back to P2P", e);
                            fallback_to_p2p(&mut self.voice, &local_id, &resp);
                        }
                    }
                }
                _ => {
                    let peer_ids: Vec<String> = resp
                        .voice_state
                        .members
                        .iter()
                        .filter(|m| m.user_id != local_id)
                        .map(|m| m.user_id.clone())
                        .collect();
                    self.voice.join_voice(&local_id, &peer_ids);
                }
            }

            let _ = self
                .event_tx
                .send(Event::VoiceStateChanged { in_call: true });
        }

        // Note: VoiceJoined was already emitted above (before SFU/P2P connection)
        // to prevent race conditions with VoiceChannelsUpdated notifications.
    }

    async fn sfu_leave_if_connected(&self) {
        if let Some(conn) = self.voice.sfu_connection() {
            conn.leave().await;
        }
    }

    async fn handle_leave_voice(&mut self) {
        self.sfu_leave_if_connected().await;
        // Notify server
        if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
            if let Err(e) = self.nakama.voice_leave(&crew_id).await {
                log::warn!("voice_leave RPC failed: {}", e);
            }
        }
        self.voice.leave_voice();
        let _ = self
            .event_tx
            .send(Event::VoiceStateChanged { in_call: false });
    }

    async fn handle_create_voice_channel(&self, crew_id: &str, name: &str) {
        match self.nakama.channel_create(crew_id, name).await {
            Ok(channel) => {
                let _ = self.event_tx.send(Event::VoiceChannelCreated {
                    crew_id: crew_id.to_string(),
                    channel,
                });
            }
            Err(e) => {
                log::error!("channel_create RPC failed: {}", e);
                let _ = self.event_tx.send(Event::Error {
                    message: format!("Failed to create voice channel: {}", e),
                });
            }
        }
    }

    async fn handle_rename_voice_channel(&self, crew_id: &str, channel_id: &str, name: &str) {
        match self.nakama.channel_rename(crew_id, channel_id, name).await {
            Ok(()) => {
                let _ = self.event_tx.send(Event::VoiceChannelRenamed {
                    crew_id: crew_id.to_string(),
                    channel_id: channel_id.to_string(),
                    name: name.to_string(),
                });
            }
            Err(e) => {
                log::error!("channel_rename RPC failed: {}", e);
                let _ = self.event_tx.send(Event::Error {
                    message: format!("Failed to rename voice channel: {}", e),
                });
            }
        }
    }

    async fn handle_delete_voice_channel(&self, crew_id: &str, channel_id: &str) {
        match self.nakama.channel_delete(crew_id, channel_id).await {
            Ok(()) => {
                let _ = self.event_tx.send(Event::VoiceChannelDeleted {
                    crew_id: crew_id.to_string(),
                    channel_id: channel_id.to_string(),
                });
            }
            Err(e) => {
                log::error!("channel_delete RPC failed: {}", e);
                let _ = self.event_tx.send(Event::Error {
                    message: format!("Failed to delete voice channel: {}", e),
                });
            }
        }
    }

    async fn handle_leave_crew(&mut self) {
        // Leave voice (local + server)
        self.sfu_leave_if_connected().await;
        if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
            if let Err(e) = self.nakama.voice_leave(&crew_id).await {
                log::warn!("voice_leave RPC on crew leave: {}", e);
            }
        }
        self.voice.leave_voice();
        let _ = self
            .event_tx
            .send(Event::VoiceStateChanged { in_call: false });
        let crew_id = self.nakama.active_crew_id().map(String::from);
        if let Err(e) = self.nakama.leave_crew_channel().await {
            log::error!("Failed to leave crew: {}", e);
        }
        if let Some(id) = crew_id {
            let _ = self.event_tx.send(Event::CrewLeft { crew_id: id });
        }
    }

    async fn handle_send_message(&self, content: &str, reply_to: Option<&str>) {
        let envelope = crate::chat::MessageEnvelope::text(content, reply_to.map(String::from));
        let json = match serde_json::to_string(&envelope) {
            Ok(j) => j,
            Err(e) => {
                log::error!("Failed to serialize message envelope: {}", e);
                return;
            }
        };
        if let Err(e) = self.nakama.send_raw_chat_message(&json).await {
            log::error!("Failed to send message: {}", e);
        }
    }

    async fn handle_send_gif(&self, gif: crate::chat::GifData, body: &str) {
        let envelope = crate::chat::MessageEnvelope::gif(gif, body);
        let json = match serde_json::to_string(&envelope) {
            Ok(j) => j,
            Err(e) => {
                log::error!("Failed to serialize GIF envelope: {}", e);
                return;
            }
        };
        if let Err(e) = self.nakama.send_raw_chat_message(&json).await {
            log::error!("Failed to send GIF message: {}", e);
        }
    }

    async fn handle_edit_message(&self, message_id: &str, new_body: &str) {
        let envelope = crate::chat::MessageEnvelope::text(new_body, None);
        let json = match serde_json::to_string(&envelope) {
            Ok(j) => j,
            Err(e) => {
                log::error!("Failed to serialize edit envelope: {}", e);
                return;
            }
        };
        if let Err(e) = self.nakama.update_chat_message(message_id, &json).await {
            log::error!("Failed to edit message: {}", e);
        }
    }

    async fn handle_delete_message(&self, message_id: &str) {
        if let Err(e) = self.nakama.remove_chat_message(message_id).await {
            log::error!("Failed to delete message: {}", e);
        }
    }

    async fn handle_search_gifs(&self, query: &str) {
        match self.giphy.search(query, 20).await {
            Ok(results) => {
                let gifs: Vec<_> = results.iter().filter_map(|r| r.to_gif_data()).collect();
                let _ = self.event_tx.send(Event::GifsLoaded { gifs });
            }
            Err(e) => log::error!("GIF search failed: {}", e),
        }
    }

    async fn handle_trending_gifs(&self) {
        match self.giphy.trending(20).await {
            Ok(results) => {
                let gifs: Vec<_> = results.iter().filter_map(|r| r.to_gif_data()).collect();
                let _ = self.event_tx.send(Event::GifsLoaded { gifs });
            }
            Err(e) => log::error!("Trending GIFs failed: {}", e),
        }
    }

    async fn handle_load_history(&mut self, cursor: Option<&str>) {
        let effective_cursor = cursor.or(self.history_cursor.as_deref());
        if effective_cursor.is_none() {
            log::debug!("No history cursor, nothing more to load");
            return;
        }

        let channel_id = match self.nakama.channel_id().await {
            Some(id) => id,
            None => return,
        };

        match self
            .nakama
            .list_channel_messages_with_cursor(&channel_id, 50, effective_cursor)
            .await
        {
            Ok((mut messages, next_cursor)) => {
                messages.reverse();
                self.history_cursor = next_cursor.clone();
                let _ = self.event_tx.send(Event::HistoryLoaded {
                    messages,
                    cursor: next_cursor,
                });
            }
            Err(e) => log::error!("Failed to load history: {}", e),
        }
    }

    // --- Streaming ---

    fn handle_list_capture_sources(&mut self) {
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

    fn start_thumbnail_refresh(&mut self) {
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

    fn stop_thumbnail_refresh(&mut self) {
        if let Some(stop) = self.thumbnail_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_start_stream(
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
            ctx, host, &resp, config, video_rx, audio_rx, resources, sink,
        ) {
            Ok(session) => {
                let _ = self.event_tx.send(Event::StreamStarted {
                    crew_id: crew_id.to_string(),
                    session_id: session.session_id.clone(),
                    mode: session.mode.clone(),
                });
                self.stream_sink = p2p_sink;
                self.stream_session = Some(session);
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
            }
        }
    }

    async fn handle_stop_stream(&mut self) {
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

    async fn handle_watch_stream(
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

        // Initialize decoder immediately — we know the resolution from the UI
        let (w, h) = if stream_width > 0 && stream_height > 0 {
            (stream_width, stream_height)
        } else {
            let config = crate::stream::StreamConfig::default();
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
            recv_buf: vec![0u8; VIEWER_RECV_BUF_SIZE],
            stream_viewer: StreamViewer::new(config.fec_n),
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
            recv_buf: vec![0u8; VIEWER_RECV_BUF_SIZE],
            stream_viewer: StreamViewer::new(config.fec_n),
            chunk_assembler: ChunkAssembler::new(),
        });

        log::info!(
            "Stream viewer peer created, waiting for Answer from host {}",
            host_id
        );
    }

    async fn handle_stop_watching(&mut self) {
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
