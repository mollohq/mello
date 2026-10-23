use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{
    protocol::{frame::coding::CloseCode, CloseFrame},
    Message,
};

use std::ptr::NonNull;

use crate::events::Event;
use crate::stream::rtp_peer::{self, ReceivedAccessUnit, RtpPeerError};
use crate::stream::StreamError;

pub use crate::stream::rtp_peer::PeerMediaRole;

/// Must stay in step with the SFU's `clientStatsMaxBytes`. Over-cap payloads are
/// dropped server-side and counted as `stream_client_stats_oversized`, so the
/// client checks first and logs locally rather than reporting into a void.
const STREAM_STATS_MAX_BYTES: usize = 2048;

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
    MemberJoined {
        user_id: String,
        role: String,
    },
    MemberLeft {
        user_id: String,
        reason: String,
    },
    MediaPacket {
        data: Vec<u8>,
    },
    ControlPacket {
        data: Vec<u8>,
    },
    /// One complete RTP packet (header and Opus payload) of a sender's audio
    /// track. Voice gives it to `mello_voice_feed_rtp`; a stream viewer takes
    /// the Opus payload.
    AudioRtp {
        sender_id: String,
        rtp: Vec<u8>,
    },
    /// The SFU released the last receive slot of `user_id` while the direct
    /// voice sink was not set. Voice drops the member's receive state.
    SlotReleased {
        user_id: String,
    },
    Disconnected {
        reason: String,
    },
    /// Stream viewer: the host disconnected without a leave. The SFU holds
    /// the session for `hold_ms` for a host rejoin; the viewer keeps its
    /// connection (mello-sfu 01-SFU.md section 4.1, session hold).
    HostReconnecting {
        user_id: String,
        hold_ms: u64,
    },
    /// Stream viewer: a host joined during the hold. Its media comes on the
    /// viewer's existing connection.
    HostReconnected {
        user_id: String,
    },
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsSink = Arc<tokio::sync::Mutex<futures::stream::SplitSink<WsStream, Message>>>;

pub struct SfuConnection {
    peer: *mut mello_sys::MelloPeerConnection,
    _peer_id_c: Option<CString>,
    ice_cb_data: *mut IceCallbackData,
    audio_cb_data: *mut AudioTrackCallbackData,
    ws_tx: WsSink,
    ws_rx: Option<futures::stream::SplitStream<WsStream>>,
    event_rx: tokio::sync::Mutex<mpsc::Receiver<SfuEvent>>,
    #[allow(dead_code)]
    event_tx: mpsc::Sender<SfuEvent>,
    server_id: String,
    region: String,
    ice_state: Arc<AtomicI32>,
    /// Woken by the peer's transport callback: a state change, or a
    /// DataChannel or the video track opened or closed.
    transport_changed: Arc<tokio::sync::Notify>,
    last_signaling_activity_ms: Arc<AtomicU64>,
    /// Background tasks (signaling listener, stats reporter) spawned for this
    /// connection. Aborted on drop so a stale connection's tasks don't linger
    /// ticking against a dead socket.
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Set after a successful join; drives readiness and RTP guardrails.
    media_role: Option<PeerMediaRole>,
    /// The peer as a send target for the voice packet sink on the capture
    /// thread. Holds the peer from the join until drop.
    audio_send: Arc<AudioSendTarget>,
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
// ICE callback data
// ---------------------------------------------------------------------------

struct IceCallbackData {
    rt_handle: tokio::runtime::Handle,
    /// Local candidates, and where they go. The SFU refuses an
    /// `ice_candidate` before the `offer` (01-SFU.md section 5.5), so they
    /// wait here until the offer is sent.
    ice_out: Mutex<IceOut>,
    ice_state: Arc<AtomicI32>,
    transport_changed: Arc<tokio::sync::Notify>,
}

/// Where a local ICE candidate goes: the queue before the offer is sent,
/// the signaling socket after.
struct IceOut {
    queue: Vec<serde_json::Value>,
    /// Set once the offer is on the wire. A candidate then goes straight
    /// to the socket.
    live: Option<WsSink>,
}

impl IceCallbackData {
    fn new(ice_state: Arc<AtomicI32>, transport_changed: Arc<tokio::sync::Notify>) -> Self {
        Self {
            rt_handle: tokio::runtime::Handle::current(),
            ice_out: Mutex::new(IceOut {
                queue: Vec::new(),
                live: None,
            }),
            ice_state,
            transport_changed,
        }
    }

    /// One local candidate from libmello (libdatachannel thread). Queued
    /// before the offer is sent, sent after.
    fn on_local_candidate(&self, msg: serde_json::Value) {
        let Ok(mut out) = self.ice_out.lock() else {
            return;
        };
        match out.live.clone() {
            Some(ws_tx) => {
                drop(out);
                // The order of candidates among themselves does not matter;
                // the offer went out before any of them.
                self.rt_handle.spawn(async move {
                    let mut tx = ws_tx.lock().await;
                    if let Err(e) = tx.send(Message::Text(msg.to_string())).await {
                        log::debug!("SFU: late ICE candidate send failed: {}", e);
                    } else {
                        log::debug!("SFU: sent a local ICE candidate after the offer");
                    }
                });
            }
            None => out.queue.push(msg),
        }
    }

    /// The offer is on the wire: take the queued candidates, and send every
    /// later one straight to `ws_tx`.
    fn go_live(&self, ws_tx: WsSink) -> Vec<serde_json::Value> {
        match self.ice_out.lock() {
            Ok(mut out) => {
                out.live = Some(ws_tx);
                std::mem::take(&mut out.queue)
            }
            Err(_) => Vec::new(),
        }
    }
}

struct AudioTrackCallbackData {
    event_tx: mpsc::Sender<SfuEvent>,
    /// Voice decoder to feed directly from the native track callback. None for
    /// stream connections and until voice attaches. See
    /// `SfuConnection::set_direct_voice_sink`.
    voice_sink: std::sync::RwLock<Option<VoiceSinkCtx>>,
    /// Voice control channel: slot map and remote speaking state.
    control: VoiceControl,
    /// When the SFU's `joined` arrived, for the first-packet log.
    joined_at: Instant,
    first_rx_logged: AtomicBool,
    /// First packet per sender, for the join latency measurement.
    #[cfg(test)]
    first_rx: Mutex<HashMap<String, Instant>>,
}

/// Voice control-channel state of one connection (mello-sfu 01-SFU.md
/// section 5.4): the SFU's slot map and the remote speaking states.
struct VoiceControl {
    /// The peer that routes incoming RTP by mid. The control callback is
    /// cleared before the peer is destroyed.
    peer: SendPeer,
    /// mid -> sender user id, as the SFU announced it.
    slots: Mutex<HashMap<String, String>>,
    /// user id -> latest in-band speaking state.
    speaking: Mutex<HashMap<String, bool>>,
    /// Receives remote speaking changes as `Event::VoiceActivity` while
    /// voice is attached (`SfuConnection::set_activity_sink`).
    activity: std::sync::RwLock<Option<std_mpsc::Sender<Event>>>,
}

impl VoiceControl {
    fn new(peer: *mut mello_sys::MelloPeerConnection) -> Self {
        Self {
            peer: SendPeer(peer),
            slots: Mutex::new(HashMap::new()),
            speaking: Mutex::new(HashMap::new()),
            activity: std::sync::RwLock::new(None),
        }
    }

    /// Send remote speaking changes to `tx` from now on. Members that speak
    /// now (the SFU's snapshot can arrive before voice attaches) go at once.
    fn set_activity(&self, tx: std_mpsc::Sender<Event>) {
        let speaking_now: Vec<String> = self
            .speaking
            .lock()
            .map(|s| {
                s.iter()
                    .filter(|(_, on)| **on)
                    .map(|(user, _)| user.clone())
                    .collect()
            })
            .unwrap_or_default();
        for member_id in speaking_now {
            let _ = tx.send(Event::VoiceActivity {
                member_id,
                speaking: true,
            });
        }
        if let Ok(mut activity) = self.activity.write() {
            *activity = Some(tx);
        }
    }
}

/// One message from the SFU on the voice control channel.
#[derive(Debug, Deserialize)]
struct ControlMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    mid: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    on: Option<bool>,
}

/// libmello context pointer used from the native audio track callback thread.
#[derive(Clone, Copy)]
struct VoiceSinkCtx(*mut mello_sys::MelloContext);
// SAFETY: `mello_voice_feed_rtp` is safe to call from any thread (the NetEQ
// playout locks its peer map, and each NetEQ locks itself). The pointer is valid while the sink is set; the voice
// manager clears the sink, under the write lock, before the context goes away.
unsafe impl Send for VoiceSinkCtx {}
unsafe impl Sync for VoiceSinkCtx {}

/// The SFU peer as a send target for a thread other than the command loop.
///
/// The voice packet sink calls [`AudioSendTarget::send`] on the audio capture
/// thread right after Opus encode (spec 10 section 4.5). The read lock guards
/// the peer pointer for the length of one send, as the `voice_sink` lock
/// guards the context on the receive path. `SfuConnection` removes the peer
/// under the write lock before it destroys the peer, so that removal waits
/// for a send that runs now.
pub struct AudioSendTarget {
    peer: std::sync::RwLock<Option<SendPeer>>,
}

/// SFU peer pointer used from the audio capture thread.
#[derive(Clone, Copy)]
struct SendPeer(*mut mello_sys::MelloPeerConnection);
// SAFETY: `mello_peer_send_audio_frame` is safe to call from any thread (it
// copies the track pointer under the peer mutex, and libdatachannel's
// `Track::send` is thread safe). The pointer is valid while it is set; the
// connection removes it, under the write lock, before the peer goes away.
unsafe impl Send for SendPeer {}
unsafe impl Sync for SendPeer {}

/// Why [`AudioSendTarget::send`] did not send a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSendSkip {
    /// The connection has no peer: before the join or after drop.
    NoPeer,
    /// libmello did not send: the audio track is not open yet, or closed.
    TrackNotOpen,
}

impl std::fmt::Display for AudioSendSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioSendSkip::NoPeer => f.write_str("no SFU peer"),
            AudioSendSkip::TrackNotOpen => f.write_str("audio track not open"),
        }
    }
}

impl AudioSendTarget {
    fn new() -> Self {
        Self {
            peer: std::sync::RwLock::new(None),
        }
    }

    fn attach(&self, peer: *mut mello_sys::MelloPeerConnection) {
        if let Ok(mut p) = self.peer.write() {
            *p = (!peer.is_null()).then_some(SendPeer(peer));
        }
    }

    /// Remove the peer. Waits for a send that runs now.
    fn detach(&self) {
        if let Ok(mut p) = self.peer.write() {
            *p = None;
        }
    }

    /// Send one Opus frame on the RTP audio track. `timestamp` is the
    /// frame's media time in 48 kHz samples. Any thread. Does not block on
    /// the network; a frame that cannot go out now is skipped.
    pub fn send(&self, data: &[u8], timestamp: u32) -> Result<(), AudioSendSkip> {
        let Ok(peer) = self.peer.read() else {
            return Err(AudioSendSkip::NoPeer);
        };
        let Some(SendPeer(peer)) = *peer else {
            return Err(AudioSendSkip::NoPeer);
        };
        // SAFETY: the read guard keeps the peer alive for this call (see
        // `SendPeer`); data outlives the call and libmello copies it.
        let result = unsafe {
            mello_sys::mello_peer_send_audio_frame(
                peer,
                data.as_ptr(),
                data.len() as i32,
                timestamp,
            )
        };
        if result == mello_sys::MelloResult_MELLO_OK {
            Ok(())
        } else {
            Err(AudioSendSkip::TrackNotOpen)
        }
    }

    /// Send one text message on the control DataChannel (the in-band
    /// speaking state). Any thread. False when there is no peer or the
    /// channel is not open.
    pub fn send_control_text(&self, text: &str) -> bool {
        let Ok(peer) = self.peer.read() else {
            return false;
        };
        let Some(SendPeer(peer)) = *peer else {
            return false;
        };
        let Ok(text) = CString::new(text) else {
            return false;
        };
        // SAFETY: the read guard keeps the peer alive for this call.
        let result = unsafe { mello_sys::mello_peer_send_control_text(peer, text.as_ptr()) };
        result == mello_sys::MelloResult_MELLO_OK
    }
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
    /// Create the voice PeerConnection synchronously (must be called where
    /// the MelloContext pointer is valid). Then call `join_voice` to
    /// negotiate WebRTC. `ice_servers` are the `get_ice_servers` URLs
    /// (STUN, and TURN over UDP, TCP and TLS); empty keeps libmello's
    /// default STUN servers.
    ///
    /// # Safety
    /// `ctx` must be a valid `MelloContext` pointer or null. With a context,
    /// the peer counts its dropped slot packets there.
    pub unsafe fn create_peer(
        ctx: *mut mello_sys::MelloContext,
        ice_servers: &[String],
    ) -> Result<PeerHandle, StreamError> {
        let peer_id_c = CString::new("sfu").expect("CString::new failed");
        let peer = unsafe { mello_sys::mello_peer_create(ctx, peer_id_c.as_ptr()) };
        if peer.is_null() {
            return Err(StreamError::SfuConnectFailed(
                "failed to create PeerConnection".into(),
            ));
        }
        // The same STUN and TURN servers as the stream peers, so a network
        // that blocks the SFU's UDP port can still relay voice (spec 10
        // section 7.6). An empty list keeps libmello's default STUN servers.
        unsafe { set_peer_ice_servers(peer, ice_servers) };
        // One recvonly m-line per remote member: a join or leave within the
        // slots needs no renegotiation (mello-sfu 01-SFU.md section 5.5).
        unsafe {
            mello_sys::mello_peer_set_voice_recv_slots(
                peer,
                mello_sys::MELLO_VOICE_RECV_SLOTS as i32,
            )
        };
        Ok(PeerHandle {
            peer,
            peer_id_c,
            media_role: PeerMediaRole::Voice,
        })
    }

    /// Create a stream PeerConnection with an explicit host/viewer media role.
    /// `ice_servers` are the `get_ice_servers` URLs, as for `create_peer`;
    /// empty keeps libmello's default STUN servers.
    ///
    /// # Safety
    /// `ctx` must be a valid `MelloContext` pointer or null.
    pub unsafe fn create_stream_peer(
        ctx: *mut mello_sys::MelloContext,
        role: StreamPeerRole,
        ice_servers: &[String],
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
        // The same STUN and TURN servers as the voice peer: a network that
        // blocks the SFU's UDP port can still relay the stream (spec 10
        // section 7.6). Stream peers used to get none.
        unsafe { set_peer_ice_servers(peer, ice_servers) };
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
            transport_changed: Arc::new(tokio::sync::Notify::new()),
            last_signaling_activity_ms,
            tasks: Mutex::new(Vec::new()),
            media_role: None,
            audio_send: Arc::new(AudioSendTarget::new()),
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

    /// Send one Opus frame via the RTP audio track (voice and stream game
    /// audio over SFU). `timestamp` is the frame's media time in 48 kHz
    /// samples; libmello adds the track's RTP start timestamp.
    pub fn send_audio(&self, data: &[u8], timestamp: u32) -> Result<(), StreamError> {
        let result = unsafe {
            mello_sys::mello_peer_send_audio_frame(
                self.peer,
                data.as_ptr(),
                data.len() as i32,
                timestamp,
            )
        };
        if result != mello_sys::MelloResult_MELLO_OK {
            return Err(StreamError::SfuSendFailed("audio track send failed".into()));
        }
        Ok(())
    }

    /// Report stream host/viewer diagnostics to the SFU over signaling.
    ///
    /// Stream sessions are the ones we cannot debug from the user's machine, so
    /// this is the only view an operator gets of a remote host's capture and
    /// encoder or a remote viewer's decode cadence and freeze time.
    ///
    /// Best-effort and non-fatal: telemetry must never disturb a working stream,
    /// so a send failure is logged at debug and dropped. The SFU rate-limits to
    /// one message per 5s per peer and caps the payload at 2048 bytes.
    pub async fn send_stream_stats(&self, payload: &serde_json::Value) {
        let msg = serde_json::json!({
            "type": "stream_client_stats",
            "seq": 0,
            "data": payload,
        });
        let body = msg.to_string();
        if body.len() > STREAM_STATS_MAX_BYTES {
            log::warn!(
                "SFU: stream_client_stats payload {} bytes exceeds {} — dropping",
                body.len(),
                STREAM_STATS_MAX_BYTES
            );
            return;
        }
        let mut tx = self.ws_tx.lock().await;
        if let Err(e) = tx.send(Message::Text(body)).await {
            log::debug!("SFU: stream_client_stats send failed: {}", e);
        }
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
    /// Voice peers require the reliable control DataChannel (the audio is on
    /// the RTP track; there is no voice media DataChannel).
    /// Stream peers require reliable control + an open native RTP video track.
    /// Returns an error if ICE fails/closes or a 5-second timeout expires.
    ///
    /// Event driven: the peer's transport callback wakes the wait on each
    /// state change and each DataChannel or video track open or close.
    pub async fn wait_for_datachannel_open(&self) -> Result<(), StreamError> {
        let role = self.media_role.ok_or_else(|| {
            StreamError::SfuProtocolError("wait_for_datachannel_open before join".into())
        })?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            // Register before the check: a change between the check and the
            // wait still wakes it (notify_one also keeps one permit).
            let changed = self.transport_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
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
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return Err(StreamError::SfuConnectFailed(
                    self.transport_timeout_message(role, self.ice_state.load(Ordering::Acquire)),
                ));
            }
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
            if result == mello_sys::MelloResult_MELLO_ERROR_TRANSPORT_BACKPRESSURE {
                return Err(StreamError::SfuSendBackpressure(
                    "video access unit send backpressure".into(),
                ));
            }
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

    /// goog_cc limits of the stream host's video sender, in bits per second
    /// (streaming-quality stage 4). `start_bps` 0 keeps the default.
    pub fn set_video_bitrate_limits(
        &self,
        min_bps: u64,
        start_bps: u64,
        max_bps: u64,
    ) -> Result<(), StreamError> {
        self.require_stream_role(PeerMediaRole::StreamHost, "set_video_bitrate_limits")?;
        let result = unsafe {
            mello_sys::mello_peer_video_set_bitrate_limits(self.peer, min_bps, start_bps, max_bps)
        };
        if result != mello_sys::MelloResult_MELLO_OK {
            return Err(StreamError::SfuSendFailed(
                "set_video_bitrate_limits failed".into(),
            ));
        }
        Ok(())
    }

    /// The encoder allocation of the stream host's video sender, in bits per
    /// second (streaming-quality stage 4). A raise makes the sender re-probe.
    pub fn set_video_max_allocated(&self, bps: u64) -> Result<(), StreamError> {
        self.require_stream_role(PeerMediaRole::StreamHost, "set_video_max_allocated")?;
        let result = unsafe { mello_sys::mello_peer_video_set_max_allocated(self.peer, bps) };
        if result != mello_sys::MelloResult_MELLO_OK {
            return Err(StreamError::SfuSendFailed(
                "set_video_max_allocated failed".into(),
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
            // The audio track opens when DTLS-SRTP is up. The control
            // channel opens after it, because SCTP runs over the same DTLS
            // session: an open control channel also means an open audio
            // track. The SFU has no data path on an "audio" DataChannel
            // (mello-sfu 01-SFU.md section 5.3).
            PeerMediaRole::Voice => self.is_control_channel_open(),
            PeerMediaRole::StreamHost | PeerMediaRole::StreamViewer => {
                self.is_control_channel_open() && self.is_video_track_open()
            }
        }
    }

    fn transport_timeout_message(&self, role: PeerMediaRole, ice_state: i32) -> String {
        match role {
            PeerMediaRole::Voice => format!(
                "DataChannel open timeout (5s): control_open={} ice_state={}",
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

        // Step 2: Receive "joined" response. Signaling that arrives before
        // it is kept for the listener (read_signal_until).
        let mut deferred: Vec<SignalingMessage> = Vec::new();
        let joined = read_signal_until(&mut ws_rx, "joined", &mut deferred).await?;
        let joined_at = Instant::now();
        if joined.msg_type == "error" {
            let err_msg = joined
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(StreamError::SfuJoinFailed(err_msg.to_string()));
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
        let (offer_sdp, cb_data_wrapped, audio_cb_wrapped) = {
            let peer = peer_handle.peer;

            let cb_data = Box::into_raw(Box::new(IceCallbackData::new(
                Arc::clone(&self.ice_state),
                Arc::clone(&self.transport_changed),
            )));

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
                // Queued until the offer is on the wire, even when
                // libdatachannel emits it before create_offer returns.
                data.on_local_candidate(msg);
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

            // Wakes `wait_for_datachannel_open` on each state change and
            // each DataChannel or video track open or close.
            unsafe extern "C" fn transport_callback(user_data: *mut std::ffi::c_void) {
                if user_data.is_null() {
                    return;
                }
                let data = &*(user_data as *const IceCallbackData);
                data.transport_changed.notify_one();
            }

            unsafe {
                mello_sys::mello_peer_set_transport_callback(
                    peer,
                    Some(transport_callback),
                    cb_data as *mut std::ffi::c_void,
                );
            }

            // Audio RTP callback: fires from C++ with each incoming RTP audio
            // packet, header included (mello_peer_set_audio_rtp_callback).
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
                if !cb_data.first_rx_logged.swap(true, AtOrd::Relaxed) {
                    log::info!(
                        "SFU: first voice packet {} ms after joined (sender {})",
                        cb_data.joined_at.elapsed().as_millis(),
                        CStr::from_ptr(sender_id).to_string_lossy()
                    );
                }
                #[cfg(test)]
                if let Ok(mut first) = cb_data.first_rx.lock() {
                    let sender = CStr::from_ptr(sender_id).to_string_lossy().into_owned();
                    first.entry(sender).or_insert_with(Instant::now);
                }

                // Voice: feed the decoder here, on the native callback thread.
                // Before, packets went through an mpsc that only the core
                // command loop's voice tick drained, so any command that blocked
                // the loop muted every other speaker (2026-09-15 beta session).
                if let Ok(sink) = cb_data.voice_sink.read() {
                    if let Some(VoiceSinkCtx(ctx)) = *sink {
                        if n <= 5 {
                            log::info!("SFU audio_track_cb #{} (direct voice): size={}", n, size);
                        }
                        // The whole RTP packet: NetEQ takes the real
                        // sequence, timestamp and SSRC. Receive time: now.
                        mello_sys::mello_voice_feed_rtp(ctx, sender_id, data, size, -1);
                        return;
                    }
                }

                let sid = CStr::from_ptr(sender_id).to_string_lossy().into_owned();
                if n <= 5 {
                    log::info!("SFU audio_track_cb #{}: sender={} size={}", n, sid, size);
                } else if n.is_multiple_of(500) {
                    log::debug!("SFU audio_track_cb #{}: sender={} size={}", n, sid, size);
                }
                let pkt = std::slice::from_raw_parts(data, size as usize).to_vec();
                if cb_data
                    .event_tx
                    .try_send(SfuEvent::AudioRtp {
                        sender_id: sid.clone(),
                        rtp: pkt,
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
                voice_sink: std::sync::RwLock::new(None),
                control: VoiceControl::new(peer),
                joined_at,
                first_rx_logged: AtomicBool::new(false),
                #[cfg(test)]
                first_rx: Mutex::new(HashMap::new()),
            }));
            unsafe {
                mello_sys::mello_peer_set_audio_rtp_callback(
                    peer,
                    Some(audio_track_callback),
                    audio_cb as *mut std::ffi::c_void,
                );
            }
            if media_role == PeerMediaRole::Voice {
                // Slot map and speaking messages, handled on the
                // libdatachannel thread (handle_control_text).
                unsafe {
                    mello_sys::mello_peer_set_control_text_callback(
                        peer,
                        Some(control_text_callback),
                        audio_cb as *mut std::ffi::c_void,
                    );
                }
                // The SFU's receiver reports about the microphone track go
                // to the encoder adaptation on the libdatachannel thread
                // (spec 10 section 4.6).
                unsafe {
                    mello_sys::mello_peer_set_send_feedback_callback(
                        peer,
                        Some(send_feedback_callback),
                        audio_cb as *mut std::ffi::c_void,
                    );
                }
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

        // The offer is on the wire. Send the queued candidates, and from now
        // on each new candidate as it comes (a TURN relay candidate can take
        // longer than the answer).
        {
            let candidates = {
                // SAFETY: the callback data lives until the teardown frees
                // it, after it clears the ICE callback.
                let cb_data = unsafe { &*cb_data_wrapped.0 };
                cb_data.go_live(Arc::clone(&self.ws_tx))
            };
            let mut tx = self.ws_tx.lock().await;
            for c in candidates {
                let _ = tx.send(Message::Text(c.to_string())).await;
            }
        }

        // Steps 8-9: Receive SDP answer. Signaling that arrives before it
        // (member changes, server ICE candidates) is kept for the listener.
        let answer = read_signal_until(&mut ws_rx, "answer", &mut deferred).await?;
        if answer.msg_type != "answer" {
            // Surface the relay's code/message: without them a failed setup
            // logs only "got error" and the cause is undebuggable in the field.
            let code = answer
                .data
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let detail = answer
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            return Err(StreamError::SfuProtocolError(format!(
                "expected answer, got {} (code={} message={})",
                answer.msg_type, code, detail
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
        // Store the peer state now that negotiation is complete
        self.peer = peer_handle.peer;
        self.audio_send.attach(self.peer);
        self._peer_id_c = Some(peer_handle.peer_id_c);
        self.ice_cb_data = cb_data_wrapped.0;
        self.audio_cb_data = audio_cb_wrapped.0;

        // Step 12: Spawn background signaling listener. It first handles the
        // signaling that arrived while the join waited for `joined` and
        // `answer`, in arrival order, then reads the socket.
        let mut listener = SignalListener {
            event_tx: self.event_tx.clone(),
            peer: SendPtr(self.peer),
            ws_tx: Arc::clone(&self.ws_tx),
            activity: Arc::clone(&self.last_signaling_activity_ms),
        };
        let listener_task = tokio::spawn(async move {
            let mut open = true;
            for sig in deferred {
                if !listener.handle(sig).await {
                    open = false;
                    break;
                }
            }
            while open {
                // A close frame or the end of the stream without a `left`
                // first: the SFU dropped this peer, or it went down. Report
                // it at once instead of waiting for the liveness check.
                let msg_result = match ws_rx.next().await {
                    None | Some(Ok(Message::Close(_))) => {
                        log::warn!("SFU WebSocket closed by the server");
                        let _ = listener
                            .event_tx
                            .send(SfuEvent::Disconnected {
                                reason: "ws_closed".into(),
                            })
                            .await;
                        break;
                    }
                    Some(result) => result,
                };
                match msg_result {
                    Ok(msg) => {
                        if let Ok(sig) = parse_ws_message(&msg) {
                            open = listener.handle(sig).await;
                        }
                    }
                    Err(e) => {
                        log::warn!("SFU WebSocket error: {}", e);
                        let _ = listener
                            .event_tx
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

    /// Feed received voice audio straight into the libmello voice decoder, from
    /// the native audio track callback, instead of through the event queue.
    ///
    /// # Safety
    /// `ctx` must stay valid until `clear_direct_voice_sink` returns or this
    /// connection is dropped.
    pub unsafe fn set_direct_voice_sink(&self, ctx: *mut mello_sys::MelloContext) {
        if self.audio_cb_data.is_null() || ctx.is_null() {
            log::warn!("SFU: direct voice sink not set (no audio callback or context)");
            return;
        }
        let cb = unsafe { &*self.audio_cb_data };
        if let Ok(mut sink) = cb.voice_sink.write() {
            *sink = Some(VoiceSinkCtx(ctx));
            log::info!("SFU: voice audio feeds the decoder directly");
        }
    }

    /// Stop feeding the voice decoder directly, and stop remote speaking
    /// events (`set_activity_sink`). Waits for a callback that is feeding
    /// right now, so the context may be released after this returns.
    pub fn clear_direct_voice_sink(&self) {
        if self.audio_cb_data.is_null() {
            return;
        }
        let cb = unsafe { &*self.audio_cb_data };
        if let Ok(mut sink) = cb.voice_sink.write() {
            if sink.take().is_some() {
                log::info!("SFU: direct voice sink cleared");
            }
        }
        if let Ok(mut activity) = cb.control.activity.write() {
            activity.take();
        }
    }

    /// Raise `Event::VoiceActivity` on `tx` for each remote speaking change
    /// from the SFU's control channel, from the libdatachannel thread. The
    /// members that speak now are sent at once. Cleared by
    /// `clear_direct_voice_sink`.
    pub fn set_activity_sink(&self, tx: std_mpsc::Sender<Event>) {
        if self.audio_cb_data.is_null() {
            return;
        }
        let cb = unsafe { &*self.audio_cb_data };
        cb.control.set_activity(tx);
    }

    /// When the SFU's `joined` arrived, and when the first packet of
    /// `sender` arrived (join latency, stage 4 gate).
    #[cfg(test)]
    pub(crate) fn first_audio_from(&self, sender: &str) -> Option<(Instant, Instant)> {
        if self.audio_cb_data.is_null() {
            return None;
        }
        let cb = unsafe { &*self.audio_cb_data };
        let first = cb.first_rx.lock().ok()?.get(sender).copied()?;
        Some((cb.joined_at, first))
    }

    /// The peer as a send target for the voice packet sink. Sends go to this
    /// connection's peer until the connection is dropped, then skip.
    pub fn audio_send_target(&self) -> Arc<AudioSendTarget> {
        Arc::clone(&self.audio_send)
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
        // Native peer close joins libdatachannel threads. It runs on the
        // teardown thread, not on whatever thread dropped the last Arc (often
        // the core command loop). Callback contexts are freed there, after the
        // peer that calls into them is destroyed.
        self.clear_direct_voice_sink();
        // A voice packet sink can hold the send target past this drop. It
        // must not reach the peer that the teardown below destroys. This
        // waits for a send that runs now on the capture thread.
        self.audio_send.detach();
        use crate::stream::teardown::TeardownPtr;
        let peer = TeardownPtr(std::mem::replace(&mut self.peer, std::ptr::null_mut()));
        let ice_cb = TeardownPtr(std::mem::replace(
            &mut self.ice_cb_data,
            std::ptr::null_mut(),
        ));
        let audio_cb = TeardownPtr(std::mem::replace(
            &mut self.audio_cb_data,
            std::ptr::null_mut(),
        ));
        if !peer.get().is_null() || !ice_cb.get().is_null() || !audio_cb.get().is_null() {
            crate::stream::teardown::spawn("sfu_peer", move |steps| unsafe {
                if !peer.get().is_null() {
                    steps.step("clear peer callbacks");
                    mello_sys::mello_peer_set_ice_callback(peer.get(), None, std::ptr::null_mut());
                    mello_sys::mello_peer_set_state_callback(
                        peer.get(),
                        None,
                        std::ptr::null_mut(),
                    );
                    mello_sys::mello_peer_set_transport_callback(
                        peer.get(),
                        None,
                        std::ptr::null_mut(),
                    );
                    mello_sys::mello_peer_set_audio_rtp_callback(
                        peer.get(),
                        None,
                        std::ptr::null_mut(),
                    );
                    mello_sys::mello_peer_set_control_text_callback(
                        peer.get(),
                        None,
                        std::ptr::null_mut(),
                    );
                    mello_sys::mello_peer_set_send_feedback_callback(
                        peer.get(),
                        None,
                        std::ptr::null_mut(),
                    );
                    steps.step("mello_peer_destroy");
                    mello_sys::mello_peer_destroy(peer.get());
                }
                steps.step("free callback contexts");
                if !ice_cb.get().is_null() {
                    drop(Box::from_raw(ice_cb.get()));
                }
                if !audio_cb.get().is_null() {
                    drop(Box::from_raw(audio_cb.get()));
                }
            });
        }
        log::info!("SFU: connection dropped (server_id={})", self.server_id);
    }
}

/// Handles signaling after a join: membership events, server ICE candidates,
/// server-initiated renegotiation, errors. Owned by the listener task.
struct SignalListener {
    event_tx: mpsc::Sender<SfuEvent>,
    peer: SendPtr<mello_sys::MelloPeerConnection>,
    ws_tx: Arc<tokio::sync::Mutex<futures::stream::SplitSink<WsStream, Message>>>,
    activity: Arc<AtomicU64>,
}

impl SignalListener {
    /// Handle one signaling message. Returns false when the session is over
    /// (a fatal error or `session_ended`) and the listener must stop.
    async fn handle(&mut self, sig: SignalingMessage) -> bool {
        self.activity.store(now_millis(), Ordering::Relaxed);
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
                let _ = self
                    .event_tx
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
                let _ = self
                    .event_tx
                    .send(SfuEvent::MemberLeft { user_id, reason })
                    .await;
            }
            "ice_candidate" => {
                if let Some(data) = sig.data.as_object() {
                    let raw = data.get("candidate").and_then(|v| v.as_str()).unwrap_or("");
                    let candidate = if raw.starts_with("a=") {
                        raw.to_string()
                    } else {
                        format!("a={}", raw)
                    };
                    let sdp_mid = data.get("sdp_mid").and_then(|v| v.as_str()).unwrap_or("0");
                    let sdp_mline_index = data
                        .get("sdp_mline_index")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0) as i32;
                    apply_remote_ice_candidate(&self.peer, &candidate, sdp_mid, sdp_mline_index);
                    log::debug!("SFU: applied server ICE candidate");
                }
            }
            "offer" => {
                // Server-initiated renegotiation (new tracks added)
                if let Some(sdp) = sig.data.get("sdp").and_then(|v| v.as_str()) {
                    if let Ok(sdp_c) = CString::new(sdp) {
                        let answer_ptr = unsafe {
                            mello_sys::mello_peer_handle_remote_offer(self.peer.0, sdp_c.as_ptr())
                        };
                        if !answer_ptr.is_null() {
                            let answer_sdp = unsafe {
                                CStr::from_ptr(answer_ptr).to_string_lossy().into_owned()
                            };
                            let answer_msg = serde_json::json!({
                                "type": "answer",
                                "seq": 0,
                                "data": { "sdp": answer_sdp }
                            });
                            let mut ws = self.ws_tx.lock().await;
                            let _ = ws.send(Message::Text(answer_msg.to_string())).await;
                            log::info!("SFU: renegotiation answer sent");
                        } else {
                            log::error!("SFU: failed to handle renegotiation offer");
                        }
                    }
                }
            }
            "error" => {
                let code = sig.data.get("code").and_then(|v| v.as_str()).unwrap_or("");
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
                    let _ = self
                        .event_tx
                        .send(SfuEvent::Disconnected {
                            reason: format!("error:{}", code),
                        })
                        .await;
                    return false;
                }
            }
            "session_ended" => {
                let _ = self
                    .event_tx
                    .send(SfuEvent::Disconnected {
                        reason: "session_ended".into(),
                    })
                    .await;
                return false;
            }
            "host_reconnecting" => {
                let user_id = sig
                    .data
                    .get("user_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let hold_ms = sig
                    .data
                    .get("hold_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                log::info!(
                    "SFU: host {} reconnecting, session held for {} ms",
                    user_id,
                    hold_ms
                );
                // The connection stays: the session is held, not ended.
                let _ = self
                    .event_tx
                    .send(SfuEvent::HostReconnecting { user_id, hold_ms })
                    .await;
            }
            "host_reconnected" => {
                let user_id = sig
                    .data
                    .get("user_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                log::info!("SFU: host {} reconnected", user_id);
                let _ = self
                    .event_tx
                    .send(SfuEvent::HostReconnected { user_id })
                    .await;
            }
            "left" => {
                // The SFU acknowledged our `leave` and closes the socket.
                // That close is not a drop.
                log::info!("SFU: leave acknowledged");
                return false;
            }
            _ => {
                log::debug!("SFU: unhandled signaling message: {}", sig.msg_type);
            }
        }
        true
    }
}

/// Control-channel text from libmello (mello_peer_set_control_text_callback).
///
/// # Safety
/// `user_data` is the connection's `AudioTrackCallbackData`, alive until
/// the teardown clears this callback.
unsafe extern "C" fn control_text_callback(
    user_data: *mut std::ffi::c_void,
    text: *const std::ffi::c_char,
    size: i32,
) {
    if user_data.is_null() || text.is_null() || size <= 0 {
        return;
    }
    let cb = &*(user_data as *const AudioTrackCallbackData);
    let bytes = std::slice::from_raw_parts(text as *const u8, size as usize);
    if let Ok(text) = std::str::from_utf8(bytes) {
        handle_control_text(cb, text);
    }
}

/// A receiver report of the SFU about the microphone track
/// (mello_peer_set_send_feedback_callback). Goes to the voice context's
/// encoder adaptation while the direct voice sink is set (spec 10 section
/// 4.6). libdatachannel thread; it never waits on the command loop.
///
/// # Safety
/// `user_data` is the connection's `AudioTrackCallbackData`, alive until
/// the teardown clears this callback; `feedback` is valid for the call.
unsafe extern "C" fn send_feedback_callback(
    user_data: *mut std::ffi::c_void,
    feedback: *const mello_sys::MelloSendFeedback,
) {
    if user_data.is_null() || feedback.is_null() {
        return;
    }
    let cb = &*(user_data as *const AudioTrackCallbackData);
    handle_send_feedback(cb, &*feedback);
}

fn handle_send_feedback(cb: &AudioTrackCallbackData, feedback: &mello_sys::MelloSendFeedback) {
    if let Ok(sink) = cb.voice_sink.read() {
        if let Some(VoiceSinkCtx(ctx)) = *sink {
            // SAFETY: the context is valid while the sink is set (the voice
            // manager clears it under the write lock before the context
            // goes). libmello copies the feedback and takes only its own
            // adaptation lock.
            unsafe { mello_sys::mello_voice_report_send_feedback(ctx, feedback) };
        }
    }
}

/// Handle one message from the SFU's voice control channel (01-SFU.md
/// section 5.4). Runs on the libdatachannel thread, so neither a slot change
/// nor a speaking ring waits on the command loop.
///
/// - `slot`: libmello routes RTP on the mid to the named member from now on.
///   A member that no longer holds any mid loses its receive state (its
///   NetEQ), so state never carries over between members on one SSRC.
/// - `speaking`: raises `Event::VoiceActivity` for the remote member.
fn handle_control_text(cb: &AudioTrackCallbackData, text: &str) {
    let msg: ControlMessage = match serde_json::from_str(text) {
        Ok(msg) => msg,
        Err(e) => {
            log::debug!("SFU control: unreadable message ({}): {}", e, text);
            return;
        }
    };
    match msg.msg_type.as_str() {
        "slot" => {
            let Some(mid) = msg.mid.filter(|m| !m.is_empty()) else {
                return;
            };
            apply_slot(cb, &mid, msg.user.filter(|u| !u.is_empty()));
        }
        "speaking" => {
            let (Some(user), Some(on)) = (msg.user.filter(|u| !u.is_empty()), msg.on) else {
                return;
            };
            if let Ok(mut speaking) = cb.control.speaking.lock() {
                speaking.insert(user.clone(), on);
            }
            if let Ok(activity) = cb.control.activity.read() {
                if let Some(tx) = activity.as_ref() {
                    let _ = tx.send(Event::VoiceActivity {
                        member_id: user,
                        speaking: on,
                    });
                }
            }
        }
        other => log::debug!("SFU control: unhandled message type {}", other),
    }
}

fn apply_slot(cb: &AudioTrackCallbackData, mid: &str, user: Option<String>) {
    let (previous, still_held) = {
        let Ok(mut slots) = cb.control.slots.lock() else {
            return;
        };
        let previous = match &user {
            Some(u) => slots.insert(mid.to_string(), u.clone()),
            None => slots.remove(mid),
        };
        let still_held = previous
            .as_ref()
            .is_some_and(|p| slots.values().any(|u| u == p));
        (previous, still_held)
    };

    // Route first: from here on, packets on `mid` belong to `user`.
    let (Ok(mid_c), Ok(user_c)) = (
        CString::new(mid),
        CString::new(user.clone().unwrap_or_default()),
    ) else {
        return;
    };
    let SendPeer(peer) = cb.control.peer;
    if !peer.is_null() {
        // SAFETY: the peer outlives this callback (see VoiceControl::peer).
        unsafe {
            mello_sys::mello_peer_set_audio_mid_sender(
                peer,
                mid_c.as_ptr(),
                if user.is_some() {
                    user_c.as_ptr()
                } else {
                    std::ptr::null()
                },
            )
        };
    }
    log::info!(
        "SFU slot {}: {} -> {}",
        mid,
        previous.as_deref().unwrap_or("-"),
        user.as_deref().unwrap_or("-")
    );

    if let Some(prev) = previous {
        if !still_held && user.as_deref() != Some(prev.as_str()) {
            release_receive_state(cb, &prev);
        }
    }
}

/// Drop `user`'s receive state (its NetEQ): directly when the voice sink is
/// set, else through the event queue for the voice tick.
fn release_receive_state(cb: &AudioTrackCallbackData, user: &str) {
    if let Ok(sink) = cb.voice_sink.read() {
        if let Some(VoiceSinkCtx(ctx)) = *sink {
            if let Ok(id) = CString::new(user) {
                // SAFETY: the sink's context is valid while it is set; the
                // read guard holds it (see VoiceSinkCtx).
                unsafe { mello_sys::mello_voice_remove_peer(ctx, id.as_ptr()) };
            }
            log::info!("SFU: receive state of {} released with its slot", user);
            return;
        }
    }
    let _ = cb.event_tx.try_send(SfuEvent::SlotReleased {
        user_id: user.to_string(),
    });
}

/// Most signaling messages kept while a join waits for one reply. A server
/// that sends more is broken; the join fails instead of buffering forever.
const MAX_DEFERRED_SIGNALS: usize = 256;

/// Read signaling until a message of type `wanted` or `error`, and return it.
///
/// A busy session sends other messages while a join waits for `joined` or
/// `answer`: `member_joined`, `member_left`, `ice_candidate`, a server
/// offer. They go to `deferred` in arrival order, for the signaling listener,
/// so the join neither fails on them nor loses them. Control frames (ping,
/// pong) are skipped.
async fn read_signal_until<S>(
    ws_rx: &mut S,
    wanted: &str,
    deferred: &mut Vec<SignalingMessage>,
) -> Result<SignalingMessage, StreamError>
where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let msg = ws_rx
            .next()
            .await
            .ok_or_else(|| {
                StreamError::SfuProtocolError(format!("connection closed before {}", wanted))
            })?
            .map_err(|e| StreamError::SfuConnectFailed(e.to_string()))?;
        match msg {
            Message::Text(_) => {}
            Message::Close(_) => {
                return Err(StreamError::SfuProtocolError(format!(
                    "connection closed before {}",
                    wanted
                )));
            }
            _ => continue,
        }
        let sig = parse_ws_message(&msg)?;
        if sig.msg_type == wanted || sig.msg_type == "error" {
            return Ok(sig);
        }
        if deferred.len() >= MAX_DEFERRED_SIGNALS {
            return Err(StreamError::SfuProtocolError(format!(
                "more than {} signaling messages before {}",
                MAX_DEFERRED_SIGNALS, wanted
            )));
        }
        log::info!(
            "SFU: {} arrived before {}; kept for the signaling listener",
            sig.msg_type,
            wanted
        );
        deferred.push(sig);
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

/// Give `peer` the ICE servers `urls` (`get_ice_servers`, TURN credentials
/// in the URL). An empty list changes nothing: libmello keeps its default
/// STUN servers (an empty list there would bind ICE to loopback).
///
/// # Safety
/// `peer` must be a live peer.
unsafe fn set_peer_ice_servers(peer: *mut mello_sys::MelloPeerConnection, urls: &[String]) {
    let urls: Vec<CString> = urls
        .iter()
        .filter_map(|u| CString::new(u.as_str()).ok())
        .collect();
    if urls.is_empty() || peer.is_null() {
        return;
    }
    let ptrs: Vec<*const std::os::raw::c_char> = urls.iter().map(|s| s.as_ptr()).collect();
    // SAFETY: the peer is live; libmello copies the strings before return.
    unsafe {
        mello_sys::mello_peer_set_ice_servers(
            peer,
            ptrs.as_ptr() as *mut *const std::os::raw::c_char,
            ptrs.len() as std::os::raw::c_int,
        )
    };
    log::info!("SFU peer: {} ICE server URL(s) set", urls.len());
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
        // NetEQ's target delay (includes the talk-spurt hold) and the late
        // playout count (spec 10 section 5.4).
        "target_delay_ms": debug.rx_target_delay_ms as u32,
        "late_underruns": debug.rx_late_underruns,
        "rtt_ms": rtt as u32,
        "send_audio_skips": send_skips,
        "recv_tracks": recv_tracks,
        // Native teardowns (stream host, viewer, peer) stuck past their
        // deadline. Non-zero means a native stop hung on this client.
        "hung_teardowns": crate::stream::teardown::hung_teardowns(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Callback data for a native voice peer, as `join_and_negotiate` builds
    /// it, with `ctx` as the direct voice sink.
    fn control_fixture(
        peer: *mut mello_sys::MelloPeerConnection,
        ctx: Option<*mut mello_sys::MelloContext>,
    ) -> (AudioTrackCallbackData, mpsc::Receiver<SfuEvent>) {
        let (event_tx, event_rx) = mpsc::channel(16);
        let data = AudioTrackCallbackData {
            event_tx,
            voice_sink: std::sync::RwLock::new(ctx.map(VoiceSinkCtx)),
            control: VoiceControl::new(peer),
            joined_at: Instant::now(),
            first_rx_logged: AtomicBool::new(false),
            first_rx: Mutex::new(HashMap::new()),
        };
        (data, event_rx)
    }

    /// One Opus RTP packet (the 3-byte Opus silence frame).
    fn opus_rtp(seq: u16, ssrc: u32) -> Vec<u8> {
        let mut pkt = vec![0x80, 111];
        pkt.extend_from_slice(&seq.to_be_bytes());
        pkt.extend_from_slice(&(u32::from(seq) * 960).to_be_bytes());
        pkt.extend_from_slice(&ssrc.to_be_bytes());
        pkt.extend_from_slice(&[0xF8, 0xFF, 0xFE]);
        pkt
    }

    fn receive_peers(ctx: *mut mello_sys::MelloContext) -> i32 {
        let mut stats: mello_sys::MelloDebugStats = unsafe { std::mem::zeroed() };
        unsafe { mello_sys::mello_get_debug_stats(ctx, &mut stats) };
        stats.incoming_streams
    }

    fn feed(ctx: *mut mello_sys::MelloContext, sender: &str, seq: u16) {
        let id = CString::new(sender).expect("sender id");
        let pkt = opus_rtp(seq, 0x5107);
        let result = unsafe {
            mello_sys::mello_voice_feed_rtp(ctx, id.as_ptr(), pkt.as_ptr(), pkt.len() as i32, -1)
        };
        assert_eq!(result, mello_sys::MelloResult_MELLO_OK);
    }

    /// A receiver report about the microphone track reaches the encoder
    /// adaptation of the voice context while voice is attached, and nothing
    /// happens after voice detaches (spec 10 section 4.6).
    #[test]
    fn send_feedback_reaches_the_encoder_adaptation_while_voice_is_attached() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let voice = crate::voice::VoiceManager::with_test_audio_backend(tx);
        let ctx = voice.mello_ctx();
        let (cb, _events) = control_fixture(std::ptr::null_mut(), Some(ctx));
        let stats = || {
            let mut s: mello_sys::MelloDebugStats = unsafe { std::mem::zeroed() };
            unsafe { mello_sys::mello_get_debug_stats(ctx, &mut s) };
            s
        };
        let mut fb: mello_sys::MelloSendFeedback = unsafe { std::mem::zeroed() };
        fb.reports = 1;
        fb.fraction_lost = 0.4;
        fb.interval_expected = 50;
        fb.interval_lost = 20;
        fb.rtt_ms = 80.0;
        handle_send_feedback(&cb, &fb);
        let s = stats();
        assert_eq!(s.tx_rr_reports, 1);
        assert_eq!(
            s.tx_packet_loss_perc, 30,
            "40 % loss raises the expected loss"
        );
        assert_eq!(s.tx_bitrate_kbps, 64, "loss alone is no congestion");
        assert_eq!(s.tx_rtt_ms, 80.0);

        // Voice detached: the report goes nowhere.
        if let Ok(mut sink) = cb.voice_sink.write() {
            sink.take();
        }
        handle_send_feedback(&cb, &fb);
        assert_eq!(stats().tx_rr_reports, 1);
    }

    /// A `slot` message routes the mid to the new member, and the old owner
    /// of the slot loses its NetEQ: two members on one slot SSRC never share
    /// receive state (stage 4).
    #[test]
    fn a_slot_message_rebinds_the_mid_and_removes_the_old_receive_peer() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let voice = crate::voice::VoiceManager::with_test_audio_backend(tx);
        let ctx = voice.mello_ctx();
        let id = CString::new("slot-test").expect("static id");
        // SAFETY: a null context is allowed.
        let peer = unsafe { mello_sys::mello_peer_create(std::ptr::null_mut(), id.as_ptr()) };
        assert!(!peer.is_null());
        let (cb, mut events) = control_fixture(peer, Some(ctx));

        handle_control_text(&cb, r#"{"type":"slot","mid":"s0","user":"alice"}"#);
        assert_eq!(unsafe { mello_sys::mello_peer_recv_track_count(peer) }, 1);
        feed(ctx, "alice", 1);
        assert_eq!(receive_peers(ctx), 1, "alice's NetEQ exists");

        // The SFU gives alice's slot to bob (she left; release and assign).
        handle_control_text(&cb, r#"{"type":"slot","mid":"s0","user":null}"#);
        assert_eq!(receive_peers(ctx), 0, "the release removed alice's NetEQ");
        assert_eq!(unsafe { mello_sys::mello_peer_recv_track_count(peer) }, 0);
        handle_control_text(&cb, r#"{"type":"slot","mid":"s0","user":"bob"}"#);
        assert_eq!(unsafe { mello_sys::mello_peer_recv_track_count(peer) }, 1);

        // A reassignment without a release in between removes the old owner too.
        feed(ctx, "bob", 1);
        assert_eq!(receive_peers(ctx), 1);
        handle_control_text(&cb, r#"{"type":"slot","mid":"s0","user":"carol"}"#);
        assert_eq!(receive_peers(ctx), 0, "bob's NetEQ went with his slot");

        // A member that still holds another mid keeps its receive state.
        handle_control_text(&cb, r#"{"type":"slot","mid":"7","user":"carol"}"#);
        feed(ctx, "carol", 1);
        handle_control_text(&cb, r#"{"type":"slot","mid":"s0","user":null}"#);
        assert_eq!(receive_peers(ctx), 1, "carol is still on mid 7");

        // A repeat of the current map (the snapshot on channel open) changes nothing.
        handle_control_text(&cb, r#"{"type":"slot","mid":"7","user":"carol"}"#);
        assert_eq!(receive_peers(ctx), 1);
        assert!(events.try_recv().is_err(), "the direct sink needs no event");

        unsafe { mello_sys::mello_peer_destroy(peer) };
    }

    /// Without the direct voice sink the release goes to the voice tick.
    #[test]
    fn a_slot_release_without_the_direct_sink_raises_an_event() {
        let (cb, mut events) = control_fixture(std::ptr::null_mut(), None);
        handle_control_text(&cb, r#"{"type":"slot","mid":"s1","user":"alice"}"#);
        handle_control_text(&cb, r#"{"type":"slot","mid":"s1","user":null}"#);
        match events.try_recv() {
            Ok(SfuEvent::SlotReleased { user_id }) => assert_eq!(user_id, "alice"),
            other => panic!("expected SlotReleased, got {other:?}"),
        }
    }

    /// A `speaking` message raises VoiceActivity for the remote member, and
    /// a member that speaks before voice attaches is not lost.
    #[test]
    fn a_speaking_message_raises_voice_activity_for_the_remote_member() {
        let (cb, _events) = control_fixture(std::ptr::null_mut(), None);

        // The SFU's snapshot arrives while the join still waits.
        handle_control_text(&cb, r#"{"type":"speaking","user":"alice","on":true}"#);
        handle_control_text(&cb, r#"{"type":"speaking","user":"dave","on":false}"#);

        let (tx, rx) = std::sync::mpsc::channel();
        cb.control.set_activity(tx);
        match rx.try_recv() {
            Ok(Event::VoiceActivity {
                member_id,
                speaking,
            }) => {
                assert_eq!(member_id, "alice");
                assert!(speaking);
            }
            other => panic!("expected alice speaking, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "a silent member raises nothing");

        handle_control_text(&cb, r#"{"type":"speaking","user":"bob","on":true}"#);
        handle_control_text(&cb, r#"{"type":"speaking","user":"bob","on":false}"#);
        let got: Vec<(String, bool)> = rx
            .try_iter()
            .map(|ev| match ev {
                Event::VoiceActivity {
                    member_id,
                    speaking,
                } => (member_id, speaking),
                other => panic!("unexpected event {other:?}"),
            })
            .collect();
        assert_eq!(got, [("bob".to_string(), true), ("bob".to_string(), false)]);

        // Malformed and unknown messages are ignored.
        handle_control_text(&cb, r#"{"type":"speaking","on":true}"#);
        handle_control_text(&cb, r#"{"type":"future_thing","x":1}"#);
        handle_control_text(&cb, "not json");
        assert!(rx.try_recv().is_err());
    }

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

    /// The voice packet sink sends through the target from the capture
    /// thread. Before the join and after the connection drops the peer, a
    /// send skips and never touches a peer pointer.
    /// A local WebSocket pair: the client's send half as an `IceOut` sink,
    /// and the text messages the server side receives.
    async fn ws_pair() -> (WsSink, mpsc::UnboundedReceiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let addr = listener.local_addr().expect("listener has an address");
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("the client connects");
            let mut ws = tokio_tungstenite::accept_async(socket)
                .await
                .expect("websocket handshake");
            while let Some(Ok(Message::Text(t))) = ws.next().await {
                let _ = tx.send(t);
            }
        });
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("connect");
        let (sink, _stream) = ws.split();
        (Arc::new(tokio::sync::Mutex::new(sink)), rx)
    }

    /// mello-sfu 01-SFU.md section 5.5: no local candidate leaves before the
    /// offer. A candidate before `go_live` (libdatachannel can emit one
    /// while `create_offer` runs) waits in the queue; `go_live` hands the
    /// queue over in order; a later candidate goes straight to the socket.
    #[tokio::test]
    async fn local_candidates_wait_for_the_offer_then_go_straight_out() {
        let (sink, mut received) = ws_pair().await;
        let cb = IceCallbackData::new(
            Arc::new(AtomicI32::new(0)),
            Arc::new(tokio::sync::Notify::new()),
        );
        let candidate = |n: u32| serde_json::json!({ "type": "ice_candidate", "data": { "candidate": format!("c{n}") } });

        cb.on_local_candidate(candidate(1));
        cb.on_local_candidate(candidate(2));
        {
            let out = cb.ice_out.lock().expect("ice_out");
            assert_eq!(out.queue.len(), 2, "candidates before the offer wait");
            assert!(out.live.is_none());
        }

        // The offer went out: the queue comes back in order.
        let queued = cb.go_live(Arc::clone(&sink));
        assert_eq!(queued, vec![candidate(1), candidate(2)]);
        assert!(cb.ice_out.lock().expect("ice_out").queue.is_empty());

        cb.on_local_candidate(candidate(3));
        let sent = tokio::time::timeout(std::time::Duration::from_secs(5), received.recv())
            .await
            .expect("the late candidate is sent")
            .expect("server open");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&sent).expect("JSON"),
            candidate(3)
        );
    }

    /// Spec 10 section 7.6: the voice SFU peer gets the ICE servers from
    /// `get_ice_servers`, and they reach libdatachannel. A TURN server on a
    /// local UDP port gets the peer's Allocate request once the offer starts
    /// ICE gathering. libjuice (the ICE agent in this build) uses TURN over
    /// UDP only; it skips the TCP and TLS entries with a warning.
    #[tokio::test]
    async fn create_peer_gives_the_ice_servers_to_libdatachannel() {
        let turn = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind a local UDP port");
        let port = turn.local_addr().expect("socket has an address").port();
        let ice = [
            "stun:stun.l.google.com:19302".to_string(),
            format!("turn:user%3A1:secret@127.0.0.1:{port}?transport=udp"),
            format!("turn:user%3A1:secret@127.0.0.1:{port}?transport=tcp"),
            "turns:user%3A1:secret@127.0.0.1:5349?transport=tcp".to_string(),
        ];
        // SAFETY: a null context is allowed.
        let handle =
            unsafe { SfuConnection::create_peer(std::ptr::null_mut(), &ice) }.expect("peer");
        // SAFETY: the peer is live until the destroy below.
        let offer = unsafe { mello_sys::mello_peer_create_offer(handle.peer) };
        assert!(!offer.is_null(), "offer");

        let mut buf = [0u8; 1500];
        let (n, _) =
            tokio::time::timeout(std::time::Duration::from_secs(5), turn.recv_from(&mut buf))
                .await
                .expect("the TURN server got a request")
                .expect("recv");
        assert!(n >= 20, "a STUN message");
        assert_eq!(&buf[0..2], &[0x00, 0x03], "a TURN Allocate request");
        assert_eq!(
            &buf[4..8],
            &[0x21, 0x12, 0xA4, 0x42],
            "the STUN magic cookie"
        );
        // SAFETY: created above, destroyed once.
        unsafe { mello_sys::mello_peer_destroy(handle.peer) };
    }

    /// Stream peers get the same ICE servers as the voice peer: the TURN
    /// server gets an Allocate request from a host and from a viewer peer.
    #[tokio::test]
    async fn create_stream_peer_gives_the_ice_servers_to_libdatachannel() {
        for role in [StreamPeerRole::Host, StreamPeerRole::Viewer] {
            let turn = tokio::net::UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("bind a local UDP port");
            let port = turn.local_addr().expect("socket has an address").port();
            let ice = [format!(
                "turn:user%3A1:secret@127.0.0.1:{port}?transport=udp"
            )];
            // SAFETY: a null context is allowed.
            let handle =
                unsafe { SfuConnection::create_stream_peer(std::ptr::null_mut(), role, &ice) }
                    .expect("stream peer");
            // SAFETY: the peer is live until the destroy below.
            let offer = unsafe { mello_sys::mello_peer_create_offer(handle.peer) };
            assert!(!offer.is_null(), "offer");

            let mut buf = [0u8; 1500];
            let (n, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), turn.recv_from(&mut buf))
                    .await
                    .unwrap_or_else(|_| panic!("{role:?}: the TURN server got no request"))
                    .expect("recv");
            assert!(n >= 20, "a STUN message");
            assert_eq!(
                &buf[0..2],
                &[0x00, 0x03],
                "{role:?}: a TURN Allocate request"
            );
            // SAFETY: created above, destroyed once.
            unsafe { mello_sys::mello_peer_destroy(handle.peer) };
        }
    }

    #[test]
    fn audio_send_target_skips_without_a_peer() {
        let target = AudioSendTarget::new();
        assert_eq!(target.send(&[1, 2, 3], 0), Err(AudioSendSkip::NoPeer));

        // A native peer that never connected has no open audio track.
        let id = CString::new("send-target").expect("static id");
        // SAFETY: a null context is allowed; the peer is
        // destroyed below, after the target let go of it.
        let peer = unsafe { mello_sys::mello_peer_create(std::ptr::null_mut(), id.as_ptr()) };
        assert!(!peer.is_null());
        target.attach(peer);
        assert_eq!(target.send(&[1, 2, 3], 0), Err(AudioSendSkip::TrackNotOpen));

        target.detach();
        assert_eq!(target.send(&[1, 2, 3], 0), Err(AudioSendSkip::NoPeer));
        // SAFETY: created above, destroyed once.
        unsafe { mello_sys::mello_peer_destroy(peer) };
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
