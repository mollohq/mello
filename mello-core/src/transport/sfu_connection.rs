use std::borrow::Cow;
use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{
    protocol::{frame::coding::CloseCode, CloseFrame},
    Message,
};

use std::ptr::NonNull;

use crate::stream::rtp_peer::{self, ReceivedAccessUnit, RtpPeerError};
use crate::stream::StreamError;

pub use crate::stream::rtp_peer::PeerMediaRole;

fn peer_media_role_is_stream(role: PeerMediaRole) -> bool {
    matches!(
        role,
        PeerMediaRole::StreamHost | PeerMediaRole::StreamViewer
    )
}

/// Stream-side peer role for explicit `create_stream_peer` creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPeerRole {
    Host,
    Viewer,
}

impl StreamPeerRole {
    /// Map SFU signaling `join_stream` role strings to a stream peer role.
    pub fn from_signaling(role: &str) -> Result<Self, StreamError> {
        match role {
            "host" => Ok(Self::Host),
            "viewer" => Ok(Self::Viewer),
            other => Err(StreamError::SfuProtocolError(format!(
                "invalid stream signaling role: {other}"
            ))),
        }
    }

    pub fn to_media_role(self) -> PeerMediaRole {
        match self {
            Self::Host => PeerMediaRole::StreamHost,
            Self::Viewer => PeerMediaRole::StreamViewer,
        }
    }
}

fn media_role_to_ffi(role: PeerMediaRole) -> mello_sys::MelloPeerMediaRole {
    match role {
        PeerMediaRole::Voice => mello_sys::MelloPeerMediaRole_MELLO_PEER_MEDIA_ROLE_VOICE,
        PeerMediaRole::StreamHost => {
            mello_sys::MelloPeerMediaRole_MELLO_PEER_MEDIA_ROLE_STREAM_HOST
        }
        PeerMediaRole::StreamViewer => {
            mello_sys::MelloPeerMediaRole_MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER
        }
    }
}

/// Metadata for one polled Annex-B H.264 access unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpAccessUnitInfo {
    pub size: u32,
    pub is_idr: bool,
    pub rtp_timestamp: u32,
    pub capture_timestamp_us: u64,
}

/// Result of polling one received RTP video access unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoAccessUnitRecv {
    /// No complete access unit is queued.
    Empty,
    /// One access unit was copied into the caller buffer.
    Received {
        bytes: usize,
        info: RtpAccessUnitInfo,
    },
    /// The queued access unit is larger than `buffer`; retry with at least this capacity.
    BufferTooSmall { required_capacity: i32 },
}

/// One host-side viewer feedback event (PLI, REMB, local IDR needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFeedback {
    pub kind: VideoFeedbackKind,
    pub remb_bitrate_bps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoFeedbackKind {
    Pli,
    Remb,
    LocalIdrNeeded,
    GccTarget,
}

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
    last_signaling_activity_ms: Arc<AtomicU64>,
    /// Background tasks (signaling listener, stats reporter) spawned for this
    /// connection. Aborted on drop so a stale connection's tasks don't linger
    /// ticking against a dead socket.
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Set after a successful join; drives readiness and RTP guardrails.
    media_role: Option<PeerMediaRole>,
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
    pub(crate) media_role: PeerMediaRole,
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
        Ok(PeerHandle {
            peer,
            peer_id_c,
            media_role: PeerMediaRole::Voice,
        })
    }

    /// Create a stream PeerConnection with an explicit host/viewer media role.
    ///
    /// # Safety
    /// `ctx` must be a valid, non-null `MelloContext` pointer.
    pub unsafe fn create_stream_peer(
        ctx: *mut mello_sys::MelloContext,
        role: StreamPeerRole,
    ) -> Result<PeerHandle, StreamError> {
        let media_role = role.to_media_role();
        let peer_id_c = CString::new("sfu").expect("CString::new failed");
        let peer = unsafe {
            mello_sys::mello_peer_create_for_role(
                ctx,
                peer_id_c.as_ptr(),
                media_role_to_ffi(media_role),
            )
        };
        if peer.is_null() {
            return Err(StreamError::SfuConnectFailed(format!(
                "failed to create stream {:?} PeerConnection",
                role
            )));
        }
        Ok(PeerHandle {
            peer,
            peer_id_c,
            media_role,
        })
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
        let last_signaling_activity_ms = Arc::new(AtomicU64::new(now_millis()));

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
            last_signaling_activity_ms,
            tasks: Mutex::new(Vec::new()),
            media_role: None,
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
        if let Err(e) = self.send_signaling(&msg).await {
            log::debug!("SFU: leave signaling send failed: {}", e);
        }
        let mut tx = self.ws_tx.lock().await;
        let _ = tx
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: Cow::Borrowed("client_leave"),
            })))
            .await;
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

    /// Whether the WebRTC connection is established.
    pub fn is_connected(&self) -> bool {
        unsafe { mello_sys::mello_peer_is_connected(self.peer) }
    }

    /// ICE connection state from the peer state callback (0=New … 5=Closed).
    pub fn ice_connection_state(&self) -> i32 {
        self.ice_state.load(Ordering::Acquire)
    }

    /// Whether ICE is connected and native RTP polling is expected to be safe.
    pub fn is_ice_connected(&self) -> bool {
        self.ice_connection_state() == 2
    }

    /// Whether the unreliable/media DataChannel is open.
    pub fn is_media_channel_open(&self) -> bool {
        unsafe { mello_sys::mello_peer_is_unreliable_open(self.peer) }
    }

    /// Whether the native RTP video track is open (stream peers only).
    pub fn is_video_track_open(&self) -> bool {
        if self.peer.is_null() {
            return false;
        }
        unsafe { mello_sys::mello_peer_video_is_open(self.peer) != 0 }
    }

    /// Whether the reliable/control DataChannel is open.
    pub fn is_control_channel_open(&self) -> bool {
        unsafe { mello_sys::mello_peer_is_reliable_open(self.peer) }
    }

    /// Media role negotiated for this connection, if join completed.
    pub fn media_role(&self) -> Option<PeerMediaRole> {
        self.media_role
    }

    pub fn send_ping(&self) {
        if !self.peer.is_null() {
            unsafe { mello_sys::mello_peer_send_ping(self.peer) }
        }
    }

    pub fn rtt_ms(&self) -> f32 {
        if self.peer.is_null() {
            return 0.0;
        }
        unsafe { mello_sys::mello_peer_rtt_ms(self.peer) }
    }

    /// Milliseconds since the last control-channel pong; -1 if none observed.
    pub fn pong_age_ms(&self) -> i64 {
        if self.peer.is_null() {
            return -1;
        }
        unsafe { mello_sys::mello_peer_pong_age_ms(self.peer) }
    }

    /// Milliseconds since the last signaling message from SFU.
    pub fn signaling_idle_ms(&self) -> u64 {
        let last = self.last_signaling_activity_ms.load(Ordering::Relaxed);
        now_millis().saturating_sub(last)
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    /// Wait for ICE and the role-appropriate transport surfaces to be ready.
    ///
    /// Voice peers require unreliable media + reliable control DataChannels.
    /// Stream peers require reliable control + an open native RTP video track.
    /// Returns an error if ICE fails/closes or a 5-second timeout expires.
    pub async fn wait_for_datachannel_open(&self) -> Result<(), StreamError> {
        let role = self.media_role.ok_or_else(|| {
            StreamError::SfuProtocolError("wait_for_datachannel_open before join".into())
        })?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let state = self.ice_state.load(Ordering::Acquire);
            match state {
                2 => {
                    if self.transport_ready(role) {
                        return Ok(());
                    }
                }
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
                    self.transport_timeout_message(role, state),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Send one Annex-B H.264 access unit on a stream-host peer.
    pub fn send_video_access_unit(
        &self,
        data: &[u8],
        capture_ts_us: u64,
    ) -> Result<(), StreamError> {
        self.require_stream_role(PeerMediaRole::StreamHost, "send_video_access_unit")?;
        let result = unsafe {
            mello_sys::mello_peer_video_send_access_unit(
                self.peer,
                data.as_ptr(),
                data.len() as i32,
                capture_ts_us,
            )
        };
        if result != mello_sys::MelloResult_MELLO_OK {
            return Err(StreamError::SfuSendFailed(
                "video access unit send failed".into(),
            ));
        }
        Ok(())
    }

    /// Poll one received RTP video access unit from a stream-viewer peer.
    pub fn recv_video_access_unit(
        &self,
        buffer: &mut [u8],
    ) -> Result<VideoAccessUnitRecv, StreamError> {
        self.require_stream_role(PeerMediaRole::StreamViewer, "recv_video_access_unit")?;
        let mut vec_buf = buffer.to_vec();
        match self.poll_received_access_unit(&mut vec_buf)? {
            None => Ok(VideoAccessUnitRecv::Empty),
            Some(au) => {
                if vec_buf.len() > buffer.len() {
                    return Ok(VideoAccessUnitRecv::BufferTooSmall {
                        required_capacity: i32::try_from(vec_buf.len()).unwrap_or(i32::MAX),
                    });
                }
                buffer[..vec_buf.len()].copy_from_slice(&vec_buf);
                Ok(VideoAccessUnitRecv::Received {
                    bytes: vec_buf.len(),
                    info: RtpAccessUnitInfo {
                        size: u32::try_from(vec_buf.len()).unwrap_or(u32::MAX),
                        is_idr: au.is_idr,
                        rtp_timestamp: au.rtp_timestamp,
                        capture_timestamp_us: au.capture_timestamp_us,
                    },
                })
            }
        }
    }

    /// Poll one complete Annex-B access unit via the shared RTP peer wrapper.
    pub fn poll_received_access_unit(
        &self,
        buffer: &mut Vec<u8>,
    ) -> Result<Option<ReceivedAccessUnit>, StreamError> {
        self.require_stream_role(PeerMediaRole::StreamViewer, "poll_received_access_unit")?;
        let peer = self.peer_nonnull()?;
        rtp_peer::poll_received_access_unit(peer, buffer).map_err(rtp_peer_error_to_stream_error)
    }

    /// Valid peer pointer for native RTP helpers.
    pub fn peer_nonnull(&self) -> Result<NonNull<mello_sys::MelloPeerConnection>, StreamError> {
        NonNull::new(self.peer)
            .ok_or_else(|| StreamError::SfuProtocolError("peer not initialized".into()))
    }

    /// Poll one queued host-side viewer feedback event.
    pub fn take_video_feedback(&self) -> Result<Option<VideoFeedback>, StreamError> {
        self.require_stream_role(PeerMediaRole::StreamHost, "take_video_feedback")?;
        let mut feedback = mello_sys::MelloPeerVideoFeedback {
            type_: mello_sys::MelloPeerVideoFeedbackType_MELLO_PEER_VIDEO_FEEDBACK_PLI,
            remb_bitrate_bps: 0,
        };
        let has_feedback =
            unsafe { mello_sys::mello_peer_video_take_feedback(self.peer, &mut feedback) != 0 };
        if !has_feedback {
            return Ok(None);
        }
        Ok(Some(VideoFeedback {
            kind: match feedback.type_ {
                mello_sys::MelloPeerVideoFeedbackType_MELLO_PEER_VIDEO_FEEDBACK_REMB => {
                    VideoFeedbackKind::Remb
                }
                mello_sys::MelloPeerVideoFeedbackType_MELLO_PEER_VIDEO_FEEDBACK_LOCAL_IDR_NEEDED => {
                    VideoFeedbackKind::LocalIdrNeeded
                }
                mello_sys::MelloPeerVideoFeedbackType_MELLO_PEER_VIDEO_FEEDBACK_GCC_TARGET => {
                    VideoFeedbackKind::GccTarget
                }
                _ => VideoFeedbackKind::Pli,
            },
            remb_bitrate_bps: feedback.remb_bitrate_bps,
        }))
    }

    /// Set the stream-host RTP pacing target in bits per second.
    pub fn set_video_pacing_target(&self, bps: u64) -> Result<(), StreamError> {
        self.require_stream_role(PeerMediaRole::StreamHost, "set_video_pacing_target")?;
        let result = unsafe { mello_sys::mello_peer_video_set_pacing_target(self.peer, bps) };
        if result != mello_sys::MelloResult_MELLO_OK {
            return Err(StreamError::SfuSendFailed(
                "set_video_pacing_target failed".into(),
            ));
        }
        Ok(())
    }

    /// Set the stream-viewer receive target in bits per second.
    pub fn set_video_receive_target(&self, bps: u32) -> Result<(), StreamError> {
        self.require_stream_role(PeerMediaRole::StreamViewer, "set_video_receive_target")?;
        let result = unsafe { mello_sys::mello_peer_video_set_receive_target(self.peer, bps) };
        if result != mello_sys::MelloResult_MELLO_OK {
            return Err(StreamError::SfuSendFailed(
                "set_video_receive_target failed".into(),
            ));
        }
        Ok(())
    }

    /// Snapshot native RTP video stats for stream peers.
    pub fn video_stats(&self) -> Result<mello_sys::MelloRtpVideoStats, StreamError> {
        let role = self
            .media_role
            .ok_or_else(|| StreamError::SfuProtocolError("video_stats before join".into()))?;
        if !peer_media_role_is_stream(role) {
            return Err(StreamError::SfuProtocolError(
                "video_stats requires a stream peer".into(),
            ));
        }
        let mut stats = unsafe { std::mem::zeroed::<mello_sys::MelloRtpVideoStats>() };
        unsafe { mello_sys::mello_peer_video_get_stats(self.peer, &mut stats) };
        Ok(stats)
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn transport_ready(&self, role: PeerMediaRole) -> bool {
        match role {
            PeerMediaRole::Voice => self.is_media_channel_open() && self.is_control_channel_open(),
            PeerMediaRole::StreamHost | PeerMediaRole::StreamViewer => {
                self.is_control_channel_open() && self.is_video_track_open()
            }
        }
    }

    fn transport_timeout_message(&self, role: PeerMediaRole, ice_state: i32) -> String {
        match role {
            PeerMediaRole::Voice => format!(
                "DataChannel open timeout (5s): media_open={} control_open={} ice_state={}",
                self.is_media_channel_open(),
                self.is_control_channel_open(),
                ice_state
            ),
            PeerMediaRole::StreamHost | PeerMediaRole::StreamViewer => format!(
                "stream transport open timeout (5s): control_open={} video_open={} ice_state={}",
                self.is_control_channel_open(),
                self.is_video_track_open(),
                ice_state
            ),
        }
    }

    fn require_stream_role(
        &self,
        expected: PeerMediaRole,
        operation: &str,
    ) -> Result<(), StreamError> {
        let role = self
            .media_role
            .ok_or_else(|| StreamError::SfuProtocolError(format!("{operation} before join")))?;
        if role != expected {
            return Err(StreamError::SfuProtocolError(format!(
                "{operation} requires {expected:?}, connection is {role:?}"
            )));
        }
        Ok(())
    }

    fn validate_peer_join(
        peer: &PeerHandle,
        join_type: &str,
        signaling_role: Option<&str>,
    ) -> Result<PeerMediaRole, StreamError> {
        match join_type {
            "join_voice" => {
                if peer.media_role != PeerMediaRole::Voice {
                    return Err(StreamError::SfuProtocolError(format!(
                        "voice join requires a voice peer, got {:?}",
                        peer.media_role
                    )));
                }
                Ok(peer.media_role)
            }
            "join_stream" => {
                if !peer_media_role_is_stream(peer.media_role) {
                    return Err(StreamError::SfuProtocolError(format!(
                        "stream join requires a stream peer, got {:?}",
                        peer.media_role
                    )));
                }
                let signaling_role = signaling_role.ok_or_else(|| {
                    StreamError::SfuProtocolError("join_stream missing role".into())
                })?;
                let expected = StreamPeerRole::from_signaling(signaling_role)?.to_media_role();
                if peer.media_role != expected {
                    return Err(StreamError::SfuProtocolError(format!(
                        "stream peer role {:?} does not match signaling role {:?}",
                        peer.media_role, expected
                    )));
                }
                Ok(peer.media_role)
            }
            other => Err(StreamError::SfuProtocolError(format!(
                "unsupported join type: {other}"
            ))),
        }
    }

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
        let join_type = join_msg
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let signaling_role = join_msg
            .get("data")
            .and_then(|data| data.get("role"))
            .and_then(|value| value.as_str());
        let media_role = Self::validate_peer_join(&peer_handle, join_type, signaling_role)?;
        self.media_role = Some(media_role);

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
                static DROP_COUNT: AtomicU64 = AtomicU64::new(0);
                let n = CB_COUNT.fetch_add(1, AtOrd::Relaxed) + 1;
                let cb_data = &*(user_data as *const AudioTrackCallbackData);
                let sid = CStr::from_ptr(sender_id).to_string_lossy().into_owned();
                if n <= 5 {
                    log::info!("SFU audio_track_cb #{}: sender={} size={}", n, sid, size);
                } else if n.is_multiple_of(500) {
                    log::debug!("SFU audio_track_cb #{}: sender={} size={}", n, sid, size);
                }
                let pkt = std::slice::from_raw_parts(data, size as usize).to_vec();
                if cb_data
                    .event_tx
                    .try_send(SfuEvent::AudioTrackData {
                        sender_id: sid.clone(),
                        data: pkt,
                    })
                    .is_err()
                {
                    // event_rx not draining fast enough (or closed). Audible as
                    // breakup; log periodically so we can correlate with SFU stats.
                    let dn = DROP_COUNT.fetch_add(1, AtOrd::Relaxed) + 1;
                    if dn == 1 || dn.is_multiple_of(50) {
                        log::warn!(
                            "SFU audio_track_cb DROPPED #{}: sender={} size={} (mpsc full)",
                            dn,
                            sid,
                            size
                        );
                    }
                }
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
        let signaling_activity_for_task = Arc::clone(&self.last_signaling_activity_ms);
        let listener_task = tokio::spawn(async move {
            while let Some(msg_result) = ws_rx.next().await {
                match msg_result {
                    Ok(msg) => {
                        if let Ok(sig) = parse_ws_message(&msg) {
                            signaling_activity_for_task.store(now_millis(), Ordering::Relaxed);
                            log::info!("SFU <- signaling: type={} data={}", sig.msg_type, sig.data);
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
                                    let code =
                                        sig.data.get("code").and_then(|v| v.as_str()).unwrap_or("");
                                    let error_msg = sig
                                        .data
                                        .get("message")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("unknown error")
                                        .to_string();
                                    log::error!("SFU signaling error [{}]: {}", code, error_msg);
                                    // Fatal errors leave the session unusable, so
                                    // surface them as a disconnect to drive a full
                                    // reconnect. Transient validation errors (e.g. a
                                    // stray ICE candidate) are logged only.
                                    const FATAL: &[&str] = &[
                                        "INVALID_TOKEN",
                                        "INVALID_ROLE",
                                        "SESSION_FULL",
                                        "WEBRTC_ERROR",
                                    ];
                                    if FATAL.contains(&code) {
                                        let _ = event_tx_clone
                                            .send(SfuEvent::Disconnected {
                                                reason: format!("error:{}", code),
                                            })
                                            .await;
                                        break;
                                    }
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
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.push(listener_task);
        }

        // Step 13: Return session info
        Ok(session_info)
    }

    /// Spawn a background task that sends `client_stats` to the SFU every 10s.
    /// Requires a valid MelloContext pointer (used for `mello_get_debug_stats`).
    ///
    /// # Safety
    /// `ctx` must remain valid for the lifetime of the SfuConnection.
    pub unsafe fn start_stats_reporter(&self, ctx: *mut mello_sys::MelloContext) {
        let ws_tx = Arc::clone(&self.ws_tx);
        let peer_addr = self.peer as usize;
        let ctx_addr = ctx as usize;
        let stats_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            interval.tick().await; // skip first immediate tick
            loop {
                interval.tick().await;
                let payload = unsafe { collect_client_stats(ctx_addr, peer_addr) };
                let msg = serde_json::json!({
                    "type": "client_stats",
                    "seq": 0,
                    "data": payload,
                });
                let body = msg.to_string();
                log::debug!("SFU -> client_stats ({} bytes)", body.len());
                let mut tx = ws_tx.lock().await;
                if let Err(e) = tx.send(Message::Text(body)).await {
                    log::warn!("SFU: client_stats send failed: {}", e);
                    break;
                }
            }
            log::info!("SFU: stats reporter ended");
        });
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.push(stats_task);
        }
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
        if let Ok(tasks) = self.tasks.lock() {
            for t in tasks.iter() {
                t.abort();
            }
        }
        if !self.peer.is_null() {
            unsafe {
                mello_sys::mello_peer_set_ice_callback(self.peer, None, std::ptr::null_mut());
                mello_sys::mello_peer_set_state_callback(self.peer, None, std::ptr::null_mut());
                mello_sys::mello_peer_set_audio_track_callback(
                    self.peer,
                    None,
                    std::ptr::null_mut(),
                );
                mello_sys::mello_peer_destroy(self.peer);
            }
            self.peer = std::ptr::null_mut();
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

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn rtp_peer_error_to_stream_error(err: RtpPeerError) -> StreamError {
    match err {
        RtpPeerError::RecvFailed => {
            StreamError::SfuProtocolError("video access unit recv failed".into())
        }
        RtpPeerError::NullPeer | RtpPeerError::CreateFailed => {
            StreamError::SfuConnectFailed(err.to_string())
        }
        other => StreamError::SfuProtocolError(other.to_string()),
    }
}

/// # Safety
/// `ctx_addr` and `peer_addr` must be valid pointers cast to usize.
unsafe fn collect_client_stats(ctx_addr: usize, peer_addr: usize) -> serde_json::Value {
    let ctx = ctx_addr as *mut mello_sys::MelloContext;
    let peer = peer_addr as *mut mello_sys::MelloPeerConnection;
    let mut debug: mello_sys::MelloDebugStats = std::mem::zeroed();
    mello_sys::mello_get_debug_stats(ctx, &mut debug);
    let rtt = mello_sys::mello_peer_rtt_ms(peer);
    let send_skips = mello_sys::mello_peer_send_audio_skips(peer);
    let recv_tracks = mello_sys::mello_peer_recv_track_count(peer);
    serde_json::json!({
        "packets_encoded": debug.packets_encoded,
        "rtp_recv_total": debug.rtp_recv_total,
        "underrun_count": debug.underrun_count,
        "incoming_streams": debug.incoming_streams,
        "input_level": (debug.input_level * 100.0) as u32,
        "is_speaking": debug.is_speaking,
        "is_capturing": debug.is_capturing,
        "is_muted": debug.is_muted,
        "pipeline_delay_ms": debug.pipeline_delay_ms as u32,
        "rtt_ms": rtt as u32,
        "send_audio_skips": send_skips,
        "recv_tracks": recv_tracks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice_peer() -> PeerHandle {
        PeerHandle {
            peer: std::ptr::null_mut(),
            peer_id_c: CString::new("sfu").expect("CString::new failed"),
            media_role: PeerMediaRole::Voice,
        }
    }

    fn stream_peer(role: PeerMediaRole) -> PeerHandle {
        PeerHandle {
            peer: std::ptr::null_mut(),
            peer_id_c: CString::new("sfu").expect("CString::new failed"),
            media_role: role,
        }
    }

    #[test]
    fn stream_peer_role_maps_signaling_strings() {
        assert_eq!(
            StreamPeerRole::from_signaling("host").expect("host"),
            StreamPeerRole::Host
        );
        assert_eq!(
            StreamPeerRole::from_signaling("viewer").expect("viewer"),
            StreamPeerRole::Viewer
        );
        assert!(StreamPeerRole::from_signaling("spectator").is_err());
    }

    #[test]
    fn voice_join_rejects_stream_peer() {
        let err = SfuConnection::validate_peer_join(
            &stream_peer(PeerMediaRole::StreamHost),
            "join_voice",
            None,
        )
        .expect_err("stream peer must not join voice");
        assert!(err.to_string().contains("voice join requires a voice peer"));
    }

    #[test]
    fn stream_join_rejects_voice_peer() {
        let err = SfuConnection::validate_peer_join(&voice_peer(), "join_stream", Some("host"))
            .expect_err("voice peer must not join stream");
        assert!(err
            .to_string()
            .contains("stream join requires a stream peer"));
    }

    #[test]
    fn stream_join_rejects_role_mismatch() {
        let err = SfuConnection::validate_peer_join(
            &stream_peer(PeerMediaRole::StreamHost),
            "join_stream",
            Some("viewer"),
        )
        .expect_err("host peer cannot join as viewer");
        assert!(err.to_string().contains("does not match signaling role"));
    }

    #[test]
    fn stream_join_accepts_matching_role() {
        let role = SfuConnection::validate_peer_join(
            &stream_peer(PeerMediaRole::StreamViewer),
            "join_stream",
            Some("viewer"),
        )
        .expect("viewer peer should join as viewer");
        assert_eq!(role, PeerMediaRole::StreamViewer);
    }

    #[test]
    fn stream_media_role_helper_matches_host_and_viewer() {
        assert!(peer_media_role_is_stream(PeerMediaRole::StreamHost));
        assert!(peer_media_role_is_stream(PeerMediaRole::StreamViewer));
        assert!(!peer_media_role_is_stream(PeerMediaRole::Voice));
    }

    #[test]
    fn stream_peer_role_converts_to_shared_media_role() {
        assert_eq!(
            StreamPeerRole::Host.to_media_role(),
            PeerMediaRole::StreamHost
        );
        assert_eq!(
            StreamPeerRole::Viewer.to_media_role(),
            PeerMediaRole::StreamViewer
        );
    }
}
