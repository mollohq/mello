use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::stream::StreamError;

const RECV_BUF_SIZE: usize = 65536;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum SfuEvent {
    MemberJoined { user_id: String, role: String },
    MemberLeft { user_id: String, reason: String },
    MediaPacket { data: Vec<u8> },
    ControlPacket { data: Vec<u8> },
    AudioTrackData { sender_id: String, data: Vec<u8> },
    Disconnected { reason: String },
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub struct SfuConnection {
    peer: *mut mello_sys::MelloPeerConnection,
    _peer_id_c: Option<CString>,
    ice_cb_data: *mut IceCallbackData,
    audio_cb_data: *mut AudioTrackCallbackData,
    ws_tx: Arc<tokio::sync::Mutex<futures::stream::SplitSink<WsStream, Message>>>,
    ws_rx: Option<futures::stream::SplitStream<WsStream>>,
    event_rx: tokio::sync::Mutex<mpsc::Receiver<SfuEvent>>,
    #[allow(dead_code)]
    event_tx: mpsc::Sender<SfuEvent>,
    server_id: String,
    region: String,
    ice_state: Arc<AtomicI32>,
}

unsafe impl Send for SfuConnection {}
unsafe impl Sync for SfuConnection {}

// ---------------------------------------------------------------------------
// Signaling message types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct SignalingMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    seq: i64,
    #[serde(default)]
    data: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemberInfo {
    pub user_id: String,
    #[serde(default)]
    pub role: String,
}

pub struct SessionInfo {
    pub session_type: String,
    pub session_id: String,
    pub members: Vec<MemberInfo>,
}

// ---------------------------------------------------------------------------
// ICE callback data (same pattern as mesh.rs)
// ---------------------------------------------------------------------------

struct IceCallbackData {
    #[allow(dead_code)]
    ws_tx: Arc<tokio::sync::Mutex<Option<futures::stream::SplitSink<WsStream, Message>>>>,
    #[allow(dead_code)]
    rt_handle: tokio::runtime::Handle,
    ice_queue: Arc<Mutex<Vec<serde_json::Value>>>,
    ice_state: Arc<AtomicI32>,
}

struct AudioTrackCallbackData {
    event_tx: mpsc::Sender<SfuEvent>,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/// Wraps a PeerConnection pointer created synchronously from a MelloContext.
/// This is `Send` because the underlying libmello uses internal locking.
pub struct PeerHandle {
    pub(crate) peer: *mut mello_sys::MelloPeerConnection,
    pub(crate) peer_id_c: CString,
}

unsafe impl Send for PeerHandle {}

/// Send-safe wrapper for raw pointers that need to cross async boundaries.
struct SendPtr<T>(*mut T);
unsafe impl<T> Send for SendPtr<T> {}

impl SfuConnection {
    /// Create the PeerConnection synchronously (must be called where the
    /// MelloContext pointer is valid). Then call `join_stream` or `join_voice`
    /// to negotiate WebRTC.
    ///
    /// # Safety
    /// `ctx` must be a valid, non-null `MelloContext` pointer.
    pub unsafe fn create_peer(
        ctx: *mut mello_sys::MelloContext,
    ) -> Result<PeerHandle, StreamError> {
        let peer_id_c = CString::new("sfu").expect("CString::new failed");
        let peer = unsafe { mello_sys::mello_peer_create(ctx, peer_id_c.as_ptr()) };
        if peer.is_null() {
            return Err(StreamError::SfuConnectFailed(
                "failed to create PeerConnection".into(),
            ));
        }
        Ok(PeerHandle { peer, peer_id_c })
    }

    /// Phase 1: WebSocket connect and welcome handshake only.
    /// No PeerConnection or DataChannels are created here.
    /// Call `join_stream` or `join_voice` afterwards to set up WebRTC.
    pub async fn connect(endpoint: &str, token: &str) -> Result<Self, StreamError> {
        let url = format!("{}?token={}", endpoint, token);
        log::info!("SFU: connecting to {}", endpoint);

        let (ws, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| StreamError::SfuConnectFailed(e.to_string()))?;

        let (ws_tx, mut ws_rx) = ws.split();
        let ws_tx = Arc::new(tokio::sync::Mutex::new(ws_tx));

        let welcome_msg = ws_rx
            .next()
            .await
            .ok_or_else(|| {
                StreamError::SfuProtocolError("connection closed before welcome".into())
            })?
            .map_err(|e| StreamError::SfuConnectFailed(e.to_string()))?;

        let welcome: SignalingMessage = parse_ws_message(&welcome_msg)?;
        if welcome.msg_type != "welcome" {
            return Err(StreamError::SfuProtocolError(format!(
                "expected welcome, got {}",
                welcome.msg_type
            )));
        }
        let server_id = welcome
            .data
            .get("server_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let region = welcome
            .data
            .get("region")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        log::info!("SFU: connected to {} ({})", server_id, region);

        let (event_tx, event_rx) = mpsc::channel(256);

        Ok(Self {
            peer: std::ptr::null_mut(),
            _peer_id_c: None,
            ice_cb_data: std::ptr::null_mut(),
            audio_cb_data: std::ptr::null_mut(),
            ws_tx,
            ws_rx: Some(ws_rx),
            event_rx: tokio::sync::Mutex::new(event_rx),
            event_tx,
            server_id,
            region,
            ice_state: Arc::new(AtomicI32::new(0)),
        })
    }

    /// Phase 2: Join a stream session, then negotiate WebRTC.
    pub async fn join_stream(
        &mut self,
        peer_handle: PeerHandle,
        session_id: &str,
        role: &str,
    ) -> Result<SessionInfo, StreamError> {
        let msg = serde_json::json!({
            "type": "join_stream",
            "seq": 1,
            "data": {
                "session_id": session_id,
                "role": role,
            }
        });
        self.join_and_negotiate(msg, peer_handle).await
    }

    /// Phase 2: Join a voice session, then negotiate WebRTC.
    pub async fn join_voice(
        &mut self,
        peer_handle: PeerHandle,
        crew_id: &str,
        channel_id: &str,
    ) -> Result<SessionInfo, StreamError> {
        let msg = serde_json::json!({
            "type": "join_voice",
            "seq": 1,
            "data": {
                "crew_id": crew_id,
                "channel_id": channel_id,
            }
        });
        self.join_and_negotiate(msg, peer_handle).await
    }

    /// Send media data (video/audio) via the unreliable DataChannel.
    pub fn send_media(&self, data: &[u8]) -> Result<(), StreamError> {
        let result = unsafe {
            mello_sys::mello_peer_send_unreliable(self.peer, data.as_ptr(), data.len() as i32)
        };
        if result != mello_sys::MelloResult_MELLO_OK {
            return Err(StreamError::SfuSendFailed("unreliable send failed".into()));
        }
        Ok(())
    }

    /// Send raw Opus frame via the RTP audio track (for voice over SFU).
    pub fn send_audio(&self, data: &[u8]) -> Result<(), StreamError> {
        let result = unsafe {
            mello_sys::mello_peer_send_audio(self.peer, data.as_ptr(), data.len() as i32)
        };
        if result != mello_sys::MelloResult_MELLO_OK {
            return Err(StreamError::SfuSendFailed("audio track send failed".into()));
        }
        Ok(())
    }

    /// Send control data (loss reports, IDR requests) via the reliable DataChannel.
    pub fn send_control(&self, data: &[u8]) -> Result<(), StreamError> {
        let result = unsafe {
            mello_sys::mello_peer_send_reliable(self.peer, data.as_ptr(), data.len() as i32)
        };
        if result != mello_sys::MelloResult_MELLO_OK {
            return Err(StreamError::SfuSendFailed("reliable send failed".into()));
        }
        Ok(())
    }

    /// Graceful leave.
    pub async fn leave(&self) {
        let msg = serde_json::json!({
            "type": "leave",
            "seq": 0,
            "data": {}
        });
        let _ = self.send_signaling(&msg).await;
    }

    /// Receive the next event from the SFU (member joins/leaves, media, disconnect).
    pub async fn recv_event(&self) -> Option<SfuEvent> {
        self.event_rx.lock().await.recv().await
    }

    /// Non-blocking poll for SFU events. Returns all currently queued events.
    pub fn poll_events(&self) -> Vec<SfuEvent> {
        let mut events = Vec::new();
        if let Ok(mut rx) = self.event_rx.try_lock() {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        events
    }

    /// Poll received packets from the DataChannel (non-blocking).
    /// Returns received media data, or empty vec if nothing pending.
    pub fn poll_recv(&self) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        let mut buf = [0u8; RECV_BUF_SIZE];
        loop {
            let size = unsafe {
                mello_sys::mello_peer_recv(self.peer, buf.as_mut_ptr(), buf.len() as i32)
            };
            if size <= 0 {
                break;
            }
            packets.push(buf[..size as usize].to_vec());
        }
        packets
    }

    /// Whether the WebRTC connection is established.
    pub fn is_connected(&self) -> bool {
        unsafe { mello_sys::mello_peer_is_connected(self.peer) }
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    /// Wait for the ICE connection (and DataChannel) to reach the Connected
    /// state. Returns an error if ICE fails or a 5-second timeout expires.
    pub async fn wait_for_datachannel_open(&self) -> Result<(), StreamError> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let state = self.ice_state.load(Ordering::Acquire);
            match state {
                2 => return Ok(()),
                4 => {
                    return Err(StreamError::SfuConnectFailed(
                        "ICE connection failed".into(),
                    ));
                }
                5 => {
                    return Err(StreamError::SfuConnectFailed(
                        "ICE connection closed".into(),
                    ));
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(StreamError::SfuConnectFailed(
                    "DataChannel open timeout (5s)".into(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Shared implementation for join_stream / join_voice:
    /// 1. Send the join message
    /// 2. Receive "joined" confirmation
    /// 3. Set up the PeerConnection (ICE callbacks, SDP offer/answer)
    /// 4. Spawn the signaling listener
    async fn join_and_negotiate(
        &mut self,
        join_msg: serde_json::Value,
        peer_handle: PeerHandle,
    ) -> Result<SessionInfo, StreamError> {
        let mut ws_rx = self.ws_rx.take().ok_or_else(|| {
            StreamError::SfuProtocolError("already joined (ws_rx consumed)".into())
        })?;

        // Step 1: Send join message
        self.send_signaling(&join_msg).await?;

        // Step 2: Receive "joined" response
        let joined_msg = ws_rx
            .next()
            .await
            .ok_or_else(|| {
                StreamError::SfuProtocolError("connection closed before joined response".into())
            })?
            .map_err(|e| StreamError::SfuConnectFailed(e.to_string()))?;

        let joined: SignalingMessage = parse_ws_message(&joined_msg)?;
        if joined.msg_type == "error" {
            let err_msg = joined
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(StreamError::SfuJoinFailed(err_msg.to_string()));
        }
        if joined.msg_type != "joined" {
            return Err(StreamError::SfuProtocolError(format!(
                "expected joined, got {}",
                joined.msg_type
            )));
        }

        let session_info = SessionInfo {
            session_type: joined
                .data
                .get("session_type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            session_id: joined
                .data
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            members: joined
                .data
                .get("members")
                .and_then(|v| serde_json::from_value::<Vec<MemberInfo>>(v.clone()).ok())
                .unwrap_or_default(),
        };
        log::info!(
            "SFU: joined session {} ({} members)",
            session_info.session_id,
            session_info.members.len()
        );

        // Steps 3-6: Create PeerConnection, set callbacks, generate SDP offer
        // All raw pointer work in this sync block — peer_handle is Send across awaits
        let ice_queue: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (offer_sdp, cb_data_wrapped, audio_cb_wrapped) = {
            let peer = peer_handle.peer;

            let cb_data = Box::into_raw(Box::new(IceCallbackData {
                ws_tx: Arc::new(tokio::sync::Mutex::new(None)),
                rt_handle: tokio::runtime::Handle::current(),
                ice_queue: Arc::clone(&ice_queue),
                ice_state: Arc::clone(&self.ice_state),
            }));

            unsafe extern "C" fn ice_callback(
                user_data: *mut std::ffi::c_void,
                candidate: *const mello_sys::MelloIceCandidate,
            ) {
                if user_data.is_null() || candidate.is_null() {
                    return;
                }
                let data = &*(user_data as *const IceCallbackData);
                let c = &*candidate;
                let cand = CStr::from_ptr(c.candidate).to_string_lossy().into_owned();
                let mid = CStr::from_ptr(c.sdp_mid).to_string_lossy().into_owned();
                let idx = c.sdp_mline_index;

                let msg = serde_json::json!({
                    "type": "ice_candidate",
                    "seq": 0,
                    "data": {
                        "candidate": cand,
                        "sdp_mid": mid,
                        "sdp_mline_index": idx
                    }
                });

                if let Ok(mut queue) = data.ice_queue.lock() {
                    queue.push(msg);
                }
            }

            unsafe {
                mello_sys::mello_peer_set_ice_callback(
                    peer,
                    Some(ice_callback),
                    cb_data as *mut std::ffi::c_void,
                );
            }

            unsafe extern "C" fn state_callback(user_data: *mut std::ffi::c_void, state: i32) {
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
                    log::error!("SFU peer ICE state: {} — connection failed", label);
                } else if state == 2 {
                    log::info!("SFU peer ICE state: {}", label);
                } else {
                    log::debug!("SFU peer ICE state: {}", label);
                }
                if !user_data.is_null() {
                    let data = &*(user_data as *const IceCallbackData);
                    data.ice_state.store(state, Ordering::Release);
                }
            }

            unsafe {
                mello_sys::mello_peer_set_state_callback(
                    peer,
                    Some(state_callback),
                    cb_data as *mut std::ffi::c_void,
                );
            }

            // Audio track callback: fires from C++ when incoming RTP audio is received
            unsafe extern "C" fn audio_track_callback(
                user_data: *mut std::ffi::c_void,
                sender_id: *const std::ffi::c_char,
                data: *const u8,
                size: i32,
            ) {
                if user_data.is_null() || sender_id.is_null() || data.is_null() || size <= 0 {
                    return;
                }
                use std::sync::atomic::{AtomicU64, Ordering as AtOrd};
                static CB_COUNT: AtomicU64 = AtomicU64::new(0);
                let n = CB_COUNT.fetch_add(1, AtOrd::Relaxed) + 1;
                let cb_data = &*(user_data as *const AudioTrackCallbackData);
                let sid = CStr::from_ptr(sender_id).to_string_lossy().into_owned();
                if n <= 5 || n.is_multiple_of(500) {
                    log::debug!("SFU audio_track_cb #{}: sender={} size={}", n, sid, size);
                }
                let pkt = std::slice::from_raw_parts(data, size as usize).to_vec();
                let _ = cb_data.event_tx.try_send(SfuEvent::AudioTrackData {
                    sender_id: sid,
                    data: pkt,
                });
            }

            let audio_cb = Box::into_raw(Box::new(AudioTrackCallbackData {
                event_tx: self.event_tx.clone(),
            }));
            unsafe {
                mello_sys::mello_peer_set_audio_track_callback(
                    peer,
                    Some(audio_track_callback),
                    audio_cb as *mut std::ffi::c_void,
                );
            }

            let offer_ptr = unsafe { mello_sys::mello_peer_create_offer(peer) };
            if offer_ptr.is_null() {
                unsafe { mello_sys::mello_peer_destroy(peer) };
                return Err(StreamError::SfuConnectFailed(
                    "failed to create SDP offer".into(),
                ));
            }
            let offer_sdp = unsafe { CStr::from_ptr(offer_ptr) }
                .to_string_lossy()
                .into_owned();

            (offer_sdp, SendPtr(cb_data), SendPtr(audio_cb))
        };

        // Step 7: Send SDP offer
        let offer_msg = serde_json::json!({
            "type": "offer",
            "seq": 0,
            "data": { "sdp": offer_sdp }
        });
        self.send_signaling(&offer_msg).await?;

        // Flush queued ICE candidates
        {
            let candidates: Vec<serde_json::Value> = {
                let mut q = ice_queue.lock().unwrap();
                q.drain(..).collect()
            };
            let mut tx = self.ws_tx.lock().await;
            for c in candidates {
                let _ = tx.send(Message::Text(c.to_string())).await;
            }
        }

        // Steps 8-9: Receive SDP answer
        let answer_msg = ws_rx
            .next()
            .await
            .ok_or_else(|| StreamError::SfuProtocolError("connection closed before answer".into()))?
            .map_err(|e| StreamError::SfuConnectFailed(e.to_string()))?;

        let answer: SignalingMessage = parse_ws_message(&answer_msg)?;
        if answer.msg_type != "answer" {
            return Err(StreamError::SfuProtocolError(format!(
                "expected answer, got {}",
                answer.msg_type
            )));
        }

        let answer_sdp = answer
            .data
            .get("sdp")
            .and_then(|v| v.as_str())
            .ok_or_else(|| StreamError::SfuProtocolError("answer missing sdp".into()))?
            .to_string();

        // Step 10: Apply SDP answer (sync FFI via peer_handle which is Send)
        {
            let answer_sdp_c = CString::new(answer_sdp)
                .map_err(|e| StreamError::SfuProtocolError(e.to_string()))?;
            unsafe {
                mello_sys::mello_peer_set_remote_description(
                    peer_handle.peer,
                    answer_sdp_c.as_ptr(),
                    false,
                );
            }
        }
        log::info!("SFU: WebRTC answer applied, waiting for ICE");

        // Step 11: Flush remaining ICE candidates
        {
            let candidates: Vec<serde_json::Value> = {
                let mut q = ice_queue.lock().unwrap();
                q.drain(..).collect()
            };
            let mut tx = self.ws_tx.lock().await;
            for c in candidates {
                let _ = tx.send(Message::Text(c.to_string())).await;
            }
        }

        // Store the peer state now that negotiation is complete
        self.peer = peer_handle.peer;
        self._peer_id_c = Some(peer_handle.peer_id_c);
        self.ice_cb_data = cb_data_wrapped.0;
        self.audio_cb_data = audio_cb_wrapped.0;

        // Step 12: Spawn background signaling listener
        let event_tx_clone = self.event_tx.clone();
        let peer_for_task = SendPtr(self.peer);
        let ws_tx_for_task = Arc::clone(&self.ws_tx);
        tokio::spawn(async move {
            while let Some(msg_result) = ws_rx.next().await {
                match msg_result {
                    Ok(msg) => {
                        if let Ok(sig) = parse_ws_message(&msg) {
                            match sig.msg_type.as_str() {
                                "member_joined" => {
                                    let user_id = sig
                                        .data
                                        .get("user_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let role = sig
                                        .data
                                        .get("role")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let _ = event_tx_clone
                                        .send(SfuEvent::MemberJoined { user_id, role })
                                        .await;
                                }
                                "member_left" => {
                                    let user_id = sig
                                        .data
                                        .get("user_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let reason = sig
                                        .data
                                        .get("reason")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("unknown")
                                        .to_string();
                                    let _ = event_tx_clone
                                        .send(SfuEvent::MemberLeft { user_id, reason })
                                        .await;
                                }
                                "ice_candidate" => {
                                    if let Some(data) = sig.data.as_object() {
                                        let raw = data
                                            .get("candidate")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        let candidate = if raw.starts_with("a=") {
                                            raw.to_string()
                                        } else {
                                            format!("a={}", raw)
                                        };
                                        let sdp_mid = data
                                            .get("sdp_mid")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("0");
                                        let sdp_mline_index = data
                                            .get("sdp_mline_index")
                                            .and_then(|v| v.as_i64())
                                            .unwrap_or(0)
                                            as i32;
                                        apply_remote_ice_candidate(
                                            &peer_for_task,
                                            &candidate,
                                            sdp_mid,
                                            sdp_mline_index,
                                        );
                                        log::debug!("SFU: applied server ICE candidate");
                                    }
                                }
                                "offer" => {
                                    // Server-initiated renegotiation (new tracks added)
                                    if let Some(sdp) = sig.data.get("sdp").and_then(|v| v.as_str())
                                    {
                                        if let Ok(sdp_c) = CString::new(sdp) {
                                            let answer_ptr = unsafe {
                                                mello_sys::mello_peer_handle_remote_offer(
                                                    peer_for_task.0,
                                                    sdp_c.as_ptr(),
                                                )
                                            };
                                            if !answer_ptr.is_null() {
                                                let answer_sdp = unsafe {
                                                    CStr::from_ptr(answer_ptr)
                                                        .to_string_lossy()
                                                        .into_owned()
                                                };
                                                let answer_msg = serde_json::json!({
                                                    "type": "answer",
                                                    "seq": 0,
                                                    "data": { "sdp": answer_sdp }
                                                });
                                                let mut ws = ws_tx_for_task.lock().await;
                                                let _ = ws
                                                    .send(Message::Text(answer_msg.to_string()))
                                                    .await;
                                                log::info!("SFU: renegotiation answer sent");
                                            } else {
                                                log::error!(
                                                    "SFU: failed to handle renegotiation offer"
                                                );
                                            }
                                        }
                                    }
                                }
                                "error" => {
                                    let error_msg = sig
                                        .data
                                        .get("message")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("unknown error")
                                        .to_string();
                                    log::error!("SFU signaling error: {}", error_msg);
                                }
                                "session_ended" => {
                                    let _ = event_tx_clone
                                        .send(SfuEvent::Disconnected {
                                            reason: "session_ended".into(),
                                        })
                                        .await;
                                    break;
                                }
                                _ => {
                                    log::debug!(
                                        "SFU: unhandled signaling message: {}",
                                        sig.msg_type
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("SFU WebSocket error: {}", e);
                        let _ = event_tx_clone
                            .send(SfuEvent::Disconnected {
                                reason: e.to_string(),
                            })
                            .await;
                        break;
                    }
                }
            }
            log::info!("SFU: signaling listener ended");
        });

        // Step 13: Return session info
        Ok(session_info)
    }

    async fn send_signaling(&self, msg: &serde_json::Value) -> Result<(), StreamError> {
        self.ws_tx
            .lock()
            .await
            .send(Message::Text(msg.to_string()))
            .await
            .map_err(|e| StreamError::SfuSendFailed(e.to_string()))
    }
}

impl Drop for SfuConnection {
    fn drop(&mut self) {
        if !self.peer.is_null() {
            unsafe {
                mello_sys::mello_peer_destroy(self.peer);
            }
        }
        if !self.ice_cb_data.is_null() {
            unsafe {
                let _ = Box::from_raw(self.ice_cb_data);
            }
        }
        if !self.audio_cb_data.is_null() {
            unsafe {
                let _ = Box::from_raw(self.audio_cb_data);
            }
        }
        log::info!("SFU: connection dropped (server_id={})", self.server_id);
    }
}

fn apply_remote_ice_candidate(
    peer: &SendPtr<mello_sys::MelloPeerConnection>,
    candidate: &str,
    sdp_mid: &str,
    sdp_mline_index: i32,
) {
    if let (Ok(cand_c), Ok(mid_c)) = (CString::new(candidate), CString::new(sdp_mid)) {
        let ice = mello_sys::MelloIceCandidate {
            candidate: cand_c.as_ptr(),
            sdp_mid: mid_c.as_ptr(),
            sdp_mline_index,
        };
        unsafe {
            mello_sys::mello_peer_add_ice_candidate(peer.0, &ice);
        }
    }
}

fn parse_ws_message(msg: &Message) -> Result<SignalingMessage, StreamError> {
    match msg {
        Message::Text(text) => serde_json::from_str(text)
            .map_err(|e| StreamError::SfuProtocolError(format!("invalid JSON: {}", e))),
        _ => Err(StreamError::SfuProtocolError(
            "expected text message".into(),
        )),
    }
}
