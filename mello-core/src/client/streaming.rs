use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::events::{Event, StreamRole};
use crate::stream::StreamMembers;
use crate::transport::sfu_connection::{MemberInfo, SfuConnection, SfuEvent, StreamPeerRole};

/// The viewer's step for one SFU membership event: apply it to the member
/// list and return the UI event when the list changed. A `member_joined`
/// with the host role, for a viewer that joined before the host, sets the
/// one host of the list (see [`StreamMembers`]).
fn viewer_membership_event(members: &mut StreamMembers, event: &SfuEvent) -> Option<Event> {
    if !members.apply(event) {
        return None;
    }
    match event {
        SfuEvent::MemberJoined { user_id, role } => {
            log::info!("Stream member joined: {} ({})", user_id, role);
        }
        SfuEvent::MemberLeft { user_id, reason } => {
            log::info!("Stream member left: {} ({})", user_id, reason);
        }
        _ => {}
    }
    Some(Event::StreamMembersChanged {
        members: members.members().to_vec(),
    })
}

/// The bundled catalogue, parsed once.
///
/// The picker re-enumerates on every open and the catalogue is static for the
/// life of the process, so re-parsing it per call is pure waste.
fn catalogue() -> Option<&'static crate::catalogue::Head> {
    static HEAD: std::sync::OnceLock<Option<crate::catalogue::Head>> = std::sync::OnceLock::new();
    HEAD.get_or_init(crate::catalogue::Head::bundled).as_ref()
}

/// One running process as a capture source, or `None` when it cannot be
/// captured. The bool is true when the catalogue knows the process.
///
/// The catalogue lookup runs once, and the name, the tier and `igdb_id` all
/// come from it. The identity and the pid therefore describe one process,
/// which the hook policy needs (game-identity plan §3).
fn game_capture_source(
    head: Option<&crate::catalogue::Head>,
    pid: u32,
    exe: String,
    path: &str,
    title: String,
    process_name: String,
    is_fullscreen: bool,
) -> Option<(bool, crate::events::CaptureSource)> {
    let entry = head.and_then(|h| h.lookup_exe(&exe, path));
    // A process with no window cannot be captured, so it has no business
    // in a capture picker regardless of which tier it would land in.
    if entry.is_none() && title.is_empty() {
        return None;
    }
    let name = match &entry {
        Some(e) => e.name.to_string(),
        None if !title.is_empty() => title,
        None => process_name,
    };
    let source = crate::events::CaptureSource {
        id: format!("game-{pid}"),
        name,
        mode: "process".to_string(),
        monitor_index: None,
        hwnd: None,
        pid: Some(pid),
        exe,
        igdb_id: entry.as_ref().map(|e| e.igdb_id).unwrap_or(0),
        is_fullscreen,
        resolution: String::new(),
    };
    Some((entry.is_some(), source))
}
use super::stream_ffi::{
    feed_viewer_audio_rtp, log_viewer_native_stats, tick_viewer_congestion_sfu,
    timeline_report_fields, viewer_stats, StreamHostHandle,
};

const STREAM_DEBUG_EVENT_INTERVAL_SECS: f32 = 1.0;
/// Longest wait for the SFU leave message when a host stops its stream or a
/// viewer stops watching.
const SFU_LEAVE_DEADLINE: std::time::Duration = std::time::Duration::from_millis(1500);

/// Send `leave` to the SFU, bounded by [`SFU_LEAVE_DEADLINE`]. For a host the
/// leave ends the stream for the viewers at once; without it the SFU holds
/// the session for a rejoin (mello-sfu 01-SFU.md section 4.1).
pub(super) async fn leave_sfu_bounded(conn: &SfuConnection, role: &str) {
    if tokio::time::timeout(SFU_LEAVE_DEADLINE, conn.leave())
        .await
        .is_err()
    {
        log::warn!(
            "SFU {} leave did not finish within {} ms; dropping the connection",
            role,
            SFU_LEAVE_DEADLINE.as_millis()
        );
    }
}
/// Viewer report cadence to the SFU. Above the server's 5s rate limit so a
/// message is never dropped for arriving too soon.
const VIEWER_STATS_INTERVAL_SECS: u64 = 10;
const HOST_PACING_DEBUG_EVENT_INTERVAL_SECS: f32 = 1.0;

// ---------------------------------------------------------------------------
// Stage 6 (streaming-quality): stream work off the command loop
// ---------------------------------------------------------------------------
//
// Start, watch, rejoin and token refresh run on Tokio tasks
// (`stream_host_start.rs`, `stream_watch.rs`). Each task reports through
// `StreamTasks`; the command loop takes the reports in its `select!` and
// finishes the step. Nothing on the stream path waits on the loop, and the
// loop waits on no stream step (plan section 3).

/// An RPC request that owns everything it needs (`NakamaClient::rpc_detached`).
pub(super) type RpcFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = crate::error::Result<String>> + Send + 'static>,
>;

/// A report of a stream task to the command loop.
pub(super) enum StreamTaskMsg {
    Host(super::stream_host_start::HostMsg),
    Viewer(super::stream_watch::ViewerMsg),
}

/// The channel of the stream task reports. Owned by the command loop.
pub(super) struct StreamTasks {
    tx: tokio::sync::mpsc::UnboundedSender<StreamTaskMsg>,
    rx: tokio::sync::mpsc::UnboundedReceiver<StreamTaskMsg>,
}

impl Default for StreamTasks {
    fn default() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self { tx, rx }
    }
}

impl StreamTasks {
    pub fn sender(&self) -> tokio::sync::mpsc::UnboundedSender<StreamTaskMsg> {
        self.tx.clone()
    }

    /// The next report. Never ends: the channel keeps its own sender.
    /// Cancel safe, so it is a `select!` branch.
    pub async fn next(&mut self) -> StreamTaskMsg {
        match self.rx.recv().await {
            Some(msg) => msg,
            None => std::future::pending().await,
        }
    }
}

/// The limits of the two SFU steps of a stream join: the WebSocket connect
/// and welcome, then the join, offer and answer and the transport wait.
#[derive(Debug, Clone, Copy)]
pub(super) struct StreamJoinLimits {
    pub connect: Duration,
    pub session: Duration,
}

/// The connect limit of a rejoin attempt. Shorter than a first join's: a
/// connect on a pulled cable hangs, and the next attempt (2 s after this
/// one started) opens a new connection when the cable is back.
const REJOIN_CONNECT_LIMIT: Duration = Duration::from_secs(2);

impl StreamJoinLimits {
    /// The limits of a rejoin attempt.
    pub fn for_rejoin(self) -> Self {
        Self {
            connect: self.connect.min(REJOIN_CONNECT_LIMIT),
            session: self.session,
        }
    }
}

impl Default for StreamJoinLimits {
    fn default() -> Self {
        Self {
            connect: super::sfu_voice_join::SFU_CONNECT_STEP_LIMIT,
            session: super::sfu_voice_join::SFU_SESSION_STEP_LIMIT,
        }
    }
}

/// The life of an SFU stream token (backend `sfu_auth.go`).
const STREAM_TOKEN_LIFETIME: Duration = Duration::from_secs(5 * 60);
/// A token this close to its expiry counts as expired.
const STREAM_TOKEN_MARGIN: Duration = Duration::from_secs(15);
/// The background refresh asks for a new token at this age.
const STREAM_TOKEN_REFRESH_AT: Duration = Duration::from_secs(4 * 60);

/// An SFU endpoint and token for one stream session.
#[derive(Debug, Clone)]
pub(super) struct StreamGrant {
    pub endpoint: String,
    pub token: String,
    pub issued_at: tokio::time::Instant,
}

impl StreamGrant {
    pub fn new(endpoint: String, token: String) -> Self {
        Self {
            endpoint,
            token,
            issued_at: tokio::time::Instant::now(),
        }
    }

    /// True while the SFU still takes the token at `now`, with the margin.
    pub fn is_fresh(&self, now: tokio::time::Instant) -> bool {
        now < self.issued_at + STREAM_TOKEN_LIFETIME - STREAM_TOKEN_MARGIN
    }

    /// True when the background refresh asks for a new token.
    pub fn needs_refresh(&self, now: tokio::time::Instant) -> bool {
        now >= self.issued_at + STREAM_TOKEN_REFRESH_AT
    }
}

/// Connect to the SFU, create the stream peer and join `session_id` as
/// `role`, then wait until the video track and the control channel are open.
/// Each step has its limit; the error names the step.
pub(super) async fn join_stream_session(
    endpoint: String,
    token: String,
    session_id: String,
    role: StreamPeerRole,
    ctx: usize,
    ice_servers: Vec<String>,
    limits: StreamJoinLimits,
) -> Result<(SfuConnection, Vec<MemberInfo>), String> {
    let mut conn =
        match tokio::time::timeout(limits.connect, SfuConnection::connect(&endpoint, &token)).await
        {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => return Err(format!("connect: {e}")),
            Err(_) => return Err(format!("connect took longer than {:?}", limits.connect)),
        };
    // SAFETY: the context lives as long as the client; a null context is
    // allowed (tests).
    let peer = unsafe {
        SfuConnection::create_stream_peer(ctx as *mut mello_sys::MelloContext, role, &ice_servers)
    }
    .map_err(|e| format!("peer: {e}"))?;
    let signaling_role = match role {
        StreamPeerRole::Host => "host",
        StreamPeerRole::Viewer => "viewer",
    };
    let joined = tokio::time::timeout(limits.session, async {
        let session = conn.join_stream(peer, &session_id, signaling_role).await?;
        conn.wait_for_datachannel_open().await?;
        Ok::<_, crate::stream::StreamError>(session)
    })
    .await;
    match joined {
        Ok(Ok(session)) => Ok((conn, session.members)),
        Ok(Err(e)) => Err(format!("join: {e}")),
        Err(_) => Err(format!("join took longer than {:?}", limits.session)),
    }
}

// ---------------------------------------------------------------------------
// The native host start, on its own thread
// ---------------------------------------------------------------------------
//
// `mello_stream_start_host` blocks for about 2 s with the hook in the path
// (injection wait plus first-frame deadline). It runs on its own OS thread
// (`stream_host_start.rs`), never on the command loop. A cancelled start
// drops the outcome when it lands, which stops the host through the bounded
// teardown guard.

/// What the start thread hands back: everything the loop needs to run the
/// stream without touching native code again.
pub(super) struct NativeStartOutcome {
    pub host: StreamHostHandle,
    pub video_rx: tokio::sync::mpsc::Receiver<crate::stream::manager::VideoPacket>,
    pub audio_rx: tokio::sync::mpsc::Receiver<crate::stream::manager::AudioPacket>,
    pub teardown: crate::stream::teardown::NativeTeardownGuard,
    pub actual_w: u32,
    pub actual_h: u32,
}

/// The blocking half of stream start. Runs on its own thread: encoder check,
/// capture backend init (including hook injection), resolution query, audio
/// start. Touches no `Client` state; every parameter is a plain value. The
/// HWND travels as an address because a raw pointer is not `Send`; the
/// capture source struct is rebuilt on the thread.
pub(super) struct NativeStartParams {
    pub ctx: usize,
    pub mode: mello_sys::MelloCaptureMode,
    pub monitor_index: u32,
    pub hwnd: usize,
    pub pid: u32,
    pub allow_hook: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

pub(super) fn start_host_blocking(params: NativeStartParams) -> Result<NativeStartOutcome, String> {
    let ctx = params.ctx as *mut mello_sys::MelloContext;
    if !unsafe { crate::stream::encoder_available(ctx) } {
        return Err("Streaming requires a hardware encoder \
                    (NVIDIA, AMD, or Intel). None was found on this machine."
            .to_string());
    }
    #[cfg(not(target_os = "linux"))]
    let source = mello_sys::MelloCaptureSource {
        mode: params.mode,
        monitor_index: params.monitor_index,
        hwnd: params.hwnd as *mut std::ffi::c_void,
        pid: params.pid,
        allow_hook: params.allow_hook,
        pipewire_fd: -1,
        pipewire_node: 0,
    };
    // Linux: the ScreenCast portal picks the source, whatever the picker
    // chose (stream/screencast.rs). Blocks while the desktop's dialog is
    // open. The game's pid travels along for its audio.
    #[cfg(target_os = "linux")]
    log::info!(
        "[screencast] the picker chose mode {} monitor {} window 0x{:x} pid {} hook {}; the portal picks the source",
        params.mode,
        params.monitor_index,
        params.hwnd,
        params.pid,
        params.allow_hook
    );
    #[cfg(target_os = "linux")]
    let screencast = crate::stream::screencast::open()?;
    #[cfg(target_os = "linux")]
    let source = {
        use std::os::fd::AsRawFd;
        mello_sys::MelloCaptureSource {
            mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_PIPEWIRE,
            monitor_index: 0,
            hwnd: std::ptr::null_mut(),
            pid: params.pid,
            allow_hook: false,
            pipewire_fd: screencast.fd.as_raw_fd(),
            pipewire_node: screencast.node,
        }
    };
    let mello_config = mello_sys::MelloStreamConfig {
        width: params.width,
        height: params.height,
        fps: params.fps,
        bitrate_kbps: params.bitrate_kbps,
    };
    let (host, video_rx, audio_rx, teardown) =
        unsafe { crate::stream::host::start_host(ctx, &source, &mello_config) }
            .map_err(|e| e.to_string())?;
    // The portal session closes after the capture that reads from it.
    #[cfg(target_os = "linux")]
    let teardown = teardown.then("screencast portal close", move |_| drop(screencast));
    let (mut actual_w, mut actual_h) = (mello_config.width, mello_config.height);
    unsafe {
        mello_sys::mello_stream_get_host_resolution(host, &mut actual_w, &mut actual_h);
    }
    unsafe {
        mello_sys::mello_stream_start_audio(host);
    }
    Ok(NativeStartOutcome {
        host: StreamHostHandle(host),
        video_rx,
        audio_rx,
        teardown,
        actual_w,
        actual_h,
    })
}

impl super::Client {
    pub(super) async fn stream_tick(&mut self) {
        // 0. Every capture method failed (for a game: probably exclusive
        // fullscreen). Report once per failure. The UI only logs StreamError
        // today; a visible message needs a design.
        if self
            .stream_session
            .as_ref()
            .is_some_and(|session| session.take_capture_failed())
        {
            let _ = self.event_tx.send(Event::StreamError {
                message: "m3llo cannot see this game. It is probably in exclusive fullscreen. \
                          Switch the game to borderless or windowed fullscreen."
                    .to_string(),
            });
        }

        // 0b. The host's link: ping, silence, rejoin, token (stage 6).
        self.host_link_tick().await;

        self.poll_sfu_host_membership_events().await;
        self.emit_host_pacing_debug_stats().await;

        // Poll viewer for incoming stream packets
        let Some(vs) = self.viewer_state.as_mut() else {
            return;
        };
        vs.stream_tick_count = vs.stream_tick_count.saturating_add(1);
        let Some(viewer) = vs.viewer else {
            return; // Decoder not yet initialized
        };

        if let Some(conn) = vs.sfu_connection.clone() {
            // The link tick pings the SFU every 128 ms; the peer hands each
            // RTT sample to the receive timeline at once (NACK interval,
            // repair deadline).
            tick_viewer_congestion_sfu(vs, &conn);
        }

        // Pause UX: host tab-out arrives on the reliable control channel into
        // the pause slot. Report transitions to the UI; while paused the
        // freeze clock is frozen too — a paused picture is deliberate, not a
        // stall, and must not accrue freeze time or trip the resume with a
        // phantom freeze.
        let paused_now = vs.pause_slot.is_paused();
        let reason_now = vs.pause_slot.reason().as_str().to_string();
        if paused_now != vs.last_reported_paused
            || (paused_now && reason_now != vs.last_reported_pause_reason)
        {
            vs.last_reported_paused = paused_now;
            vs.last_reported_pause_reason = reason_now.clone();
            log::info!(
                "Stream pause state changed: host={} paused={} reason={}",
                vs.host_id,
                paused_now,
                reason_now
            );
            let _ = self.event_tx.send(Event::StreamPaused {
                host_id: vs.host_id.clone(),
                paused: paused_now,
                reason: reason_now,
            });
        }
        // Game audio follows the pause: the host stops sending it, and the
        // playout must not keep playing what it had queued. This also covers
        // a viewer that joins a paused stream: the host sends it the pause
        // state, and the slot turns paused on this tick.
        if let Some(muted) =
            super::stream_ffi::pause_audio_mute_change(paused_now, &mut vs.audio_muted)
        {
            let result = unsafe { mello_sys::mello_stream_viewer_set_audio_muted(viewer, muted) };
            log::info!(
                "Stream game audio {} for the host's pause (result {})",
                if muted { "muted" } else { "unmuted" },
                result
            );
        }
        // The host's idle keepalive (2 fps re-encodes of a static picture)
        // holds the freeze clock the same way: those gaps are not stalls.
        let held_now = paused_now || vs.pause_slot.is_host_idle();

        // libmello's present thread shows the frames (stage 3); the frame
        // callbacks count each one. New frames since the last tick end a
        // freeze.
        let delivered = vs
            .delivered_frames
            .load(std::sync::atomic::Ordering::Acquire);
        let new_frames =
            super::stream_ffi::new_frames_since(delivered, &mut vs.last_delivered_frames);
        if new_frames > 0 {
            vs.frames_presented = vs.frames_presented.saturating_add(new_frames);
            vs.last_new_frame_at = Instant::now();
            vs.in_freeze = false;
            vs.freeze_accounted_ms = 0;
            if vs.frames_presented <= 3 {
                log::info!("Stream frame presented #{}", vs.frames_presented);
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
        {
            let present_gap_ms = vs.last_new_frame_at.elapsed().as_millis() as u64;
            let (mut in_freeze, mut count, mut total, mut accounted) = (
                vs.in_freeze,
                vs.freeze_count,
                vs.freeze_ms_total,
                vs.freeze_accounted_ms,
            );
            if super::stream_ffi::freeze_clock_tick(
                held_now,
                vs.frames_presented > 0,
                present_gap_ms,
                &mut in_freeze,
                &mut count,
                &mut total,
                &mut accounted,
            ) {
                vs.last_new_frame_at = Instant::now();
            }
            vs.in_freeze = in_freeze;
            vs.freeze_count = count;
            vs.freeze_ms_total = total;
            vs.freeze_accounted_ms = accounted;
        }

        // SFU events: membership and game audio here; the session events
        // (a drop, the host's hold, the end) after this borrow, below.
        let mut session_events = Vec::new();
        if let Some(ref conn) = vs.sfu_connection {
            for event in conn.poll_events() {
                match event {
                    ev @ (SfuEvent::Disconnected { .. }
                    | SfuEvent::HostReconnecting { .. }
                    | SfuEvent::HostReconnected { .. }) => session_events.push(ev),
                    SfuEvent::MediaPacket { .. } => {
                        log::debug!("Ignoring SFU MediaPacket — stream video uses native RTP");
                    }
                    ev @ (SfuEvent::MemberJoined { .. } | SfuEvent::MemberLeft { .. }) => {
                        if let Some(changed) = viewer_membership_event(&mut vs.members, &ev) {
                            let _ = self.event_tx.send(changed);
                        }
                    }
                    SfuEvent::ControlPacket { .. } | SfuEvent::SlotReleased { .. } => {}
                    SfuEvent::AudioRtp { rtp, .. } => {
                        // SAFETY: `vs.viewer` is set only from a successful
                        // viewer start and cleared on stop, so it is valid
                        // for the lifetime of this ViewerState.
                        if unsafe { feed_viewer_audio_rtp(viewer, &rtp) } {
                            vs.audio_packets_received = vs.audio_packets_received.saturating_add(1);
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
            let delta_frames = vs
                .frames_presented
                .saturating_sub(vs.debug_last_frames_presented);
            let native = vs
                .sfu_connection
                .as_ref()
                .and_then(|c| c.video_stats().ok());
            // SAFETY: `viewer` is live for this ViewerState.
            let timeline = unsafe { viewer_stats(viewer) };
            let ingress_bytes = native.as_ref().map_or(0, |s| s.rx_ingress_bytes);
            let delta_bytes = ingress_bytes.saturating_sub(vs.debug_last_bytes);
            let delta_decoded = timeline
                .frames_decoded
                .saturating_sub(vs.debug_last_frames_decoded);
            let ingress_kbps = (delta_bytes as f32 * 8.0 / 1000.0) / elapsed.max(0.001);
            let present_fps = (delta_frames as f32) / elapsed.max(0.001);
            let decode_fps = (delta_decoded as f32) / elapsed.max(0.001);
            let stream_tick_hz = (delta_ticks as f32) / elapsed.max(0.001);

            let _ = self.event_tx.send(Event::StreamDebugStats {
                mode: "sfu".to_string(),
                transport_packets: timeline.frames_released,
                transport_bytes: ingress_bytes,
                transport_truncations: 0,
                frames_presented: vs.frames_presented,
                present_fps,
                ingress_kbps,
            });
            let _ = self.event_tx.send(Event::StreamViewerTimeline {
                target_delay_ms: timeline.target_delay_ms,
                jitter_ms: timeline.jitter_ms,
                decode_ms_p95: timeline.decode_ms_p95,
                frames_released: timeline.frames_released,
                frames_late: timeline.frames_late,
                frames_dropped_late: timeline.frames_dropped_late,
                decoder: timeline.decoder.clone(),
                decode_fps,
                // Stage 7: game audio and A/V sync.
                audio_delay_ms: timeline.audio_delay_ms,
                av_offset_ms: timeline.av_offset_ms,
                audio_expand_ms: timeline.audio_expand_ms,
                audio_fec_used: timeline.audio_fec_used,
            });
            super::stream_ffi::log_viewer_audio(&timeline);

            if let Some(stats) = native.as_ref() {
                log_viewer_native_stats(stats);
            }

            let rtt_ms = vs.sfu_connection.as_ref().map(|conn| conn.rtt_ms());
            log::info!(
                "Stream timeline: target_ms={:.1} jitter_ms={:.1} decode_p95_ms={:.1} released={} late={} dropped_late={} keyframe_requests={} losses={} waiting_key={} decoder={} decode_fps={:.1} present_fps={:.1} tick_hz={:.1} rtt_ms={:.0}",
                timeline.target_delay_ms,
                timeline.jitter_ms,
                timeline.decode_ms_p95,
                timeline.frames_released,
                timeline.frames_late,
                timeline.frames_dropped_late,
                timeline.keyframe_requests,
                timeline.loss_events,
                timeline.waiting_for_keyframe,
                timeline.decoder,
                decode_fps,
                present_fps,
                stream_tick_hz,
                rtt_ms.unwrap_or(-1.0)
            );

            vs.debug_last_emit = Instant::now();
            vs.debug_last_tick_count = vs.stream_tick_count;
            vs.debug_last_bytes = ingress_bytes;
            vs.debug_last_frames_presented = vs.frames_presented;
            vs.debug_last_frames_decoded = timeline.frames_decoded;
        }

        self.report_viewer_stream_stats();

        // Stage 6: the session events and the viewer's link. A drop or the
        // host's hold keeps the viewer and its last frame.
        for event in session_events {
            if !self.on_viewer_sfu_event(&event).await {
                return;
            }
        }
        self.viewer_link_tick(new_frames, held_now).await;
    }

    fn report_viewer_stream_stats(&mut self) {
        let Some(vs) = self.viewer_state.as_mut() else {
            return;
        };
        if vs.stats_last_emit.elapsed().as_secs() < VIEWER_STATS_INTERVAL_SECS {
            return;
        }
        let elapsed = vs.stats_last_emit.elapsed().as_secs_f32().max(0.001);
        vs.stats_last_emit = Instant::now();

        let Some(conn) = vs.sfu_connection.clone() else {
            return;
        };
        let Some(viewer) = vs.viewer else {
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

        // SAFETY: `viewer` is live for this ViewerState.
        let timeline = unsafe { viewer_stats(viewer) };
        let decoded = timeline
            .frames_decoded
            .saturating_sub(vs.stats_last_frames_decoded);
        vs.stats_last_frames_decoded = timeline.frames_decoded;

        let native = conn.video_stats().ok();
        let mut payload = serde_json::json!({
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
        });
        if let (Some(map), serde_json::Value::Object(extra)) = (
            payload.as_object_mut(),
            timeline_report_fields(&timeline, decoded, elapsed),
        ) {
            map.extend(extra);
        }
        // The send waits on the SFU socket: never on the command loop.
        tokio::spawn(async move {
            conn.send_stream_stats(&payload).await;
        });
    }

    async fn poll_sfu_host_membership_events(&mut self) {
        let Some(conn) = self.stream_sfu_connection.clone() else {
            return;
        };
        let Some(sink) = self.stream_host_sink.clone() else {
            return;
        };

        // The host link tick pings the SFU (every 128 ms, stage 6).
        let mut dropped = None;
        for event in conn.poll_events() {
            match event {
                SfuEvent::MemberJoined { user_id, role } => {
                    if role == "viewer" {
                        sink.on_viewer_joined(&user_id).await;
                    }
                }
                SfuEvent::MemberLeft { user_id, .. } => {
                    sink.on_viewer_left(&user_id).await;
                    self.report_stream_viewer_left(&user_id).await;
                }
                SfuEvent::Disconnected { reason } => {
                    dropped = Some(reason);
                }
                SfuEvent::MediaPacket { .. } | SfuEvent::ControlPacket { .. } => {}
                SfuEvent::AudioRtp { .. } | SfuEvent::SlotReleased { .. } => {}
                SfuEvent::HostReconnecting { .. } | SfuEvent::HostReconnected { .. } => {}
            }
        }
        // Stage 6: a lost host connection rejoins the same session; before,
        // it was logged and the encoder sent into a dead peer.
        if let Some(reason) = dropped {
            self.on_host_link_dropped(&reason).await;
        }
    }

    /// Tell the backend that a viewer left, so the crew's viewer count goes
    /// down (`stream_viewer_left`; `watch_stream` counts the viewer in). The
    /// SFU sends `member_left` only for a viewer that left, not for one that
    /// reconnected.
    pub(super) async fn report_stream_viewer_left(&mut self, viewer_id: &str) {
        let Some(crew_id) = self
            .stream_session
            .as_ref()
            .map(|s| s.crew_id.clone())
            .filter(|c| !c.is_empty())
        else {
            return;
        };
        let payload = serde_json::json!({ "crew_id": crew_id, "viewer_id": viewer_id });
        // A slow backend must not hold the stream tick: the RPC runs on its
        // own task.
        match self.nakama.rpc_detached("stream_viewer_left", &payload) {
            Ok(request) => {
                let viewer_id = viewer_id.to_string();
                tokio::spawn(async move {
                    if let Err(e) = request.await {
                        log::warn!("stream_viewer_left RPC failed for {}: {}", viewer_id, e);
                    }
                });
            }
            Err(e) => log::warn!("stream_viewer_left RPC failed for {}: {}", viewer_id, e),
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
        // The picker opens: a stream start follows. Probe the SFU regions
        // now, so the start goes to the region that is fastest now
        // (stage 6).
        self.probe_regions_for_stream();
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
                igdb_id: 0,
                is_fullscreen: false,
                resolution: format!("{}x{}", mon.width, mon.height),
            });
        }

        // libmello's enumerator returns the whole process table by design — the
        // game database decides what is a game (spec 17 §2.1), not libmello. The
        // sensor's enumerator owns the buffer sizing: a fixed buffer truncated
        // in boot order, first at 32 entries (System, smss, csrss... and never a
        // game) and then at 512 (cs2.exe at entry 961 of 1036).
        let games_raw = crate::game_sensing::enumerate_game_processes(ctx);

        let head = catalogue();
        let own_pid = std::process::id();

        // Two tiers. Database matches are what the user is almost always after,
        // so they come first and carry the catalogue's display name. Everything
        // else that owns a visible window follows, so a game the database has
        // never heard of — an indie title, a beta, a private build — is still
        // streamable instead of disappearing from the picker entirely.
        let mut known = Vec::new();
        let mut other = Vec::new();
        for game in games_raw {
            if game.pid == own_pid {
                continue;
            }
            let Some((is_known, source)) = game_capture_source(
                head,
                game.pid,
                game.exe,
                &game.path,
                game.window_title,
                game.name,
                game.is_fullscreen,
            ) else {
                continue;
            };
            if is_known {
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
                igdb_id: 0,
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
    pub(super) async fn handle_stop_stream(&mut self) {
        // A start in flight: END STREAM answers at once. The start's steps
        // are cancelled, the native host stops through its teardown guard,
        // and the session is taken back.
        if let Some(start) = self.host_start.take() {
            log::info!(
                "Stopping stream while its start is in flight for crew {}",
                start.crew_id
            );
            let _ = self.event_tx.send(Event::StreamEnded {
                crew_id: start.crew_id.clone(),
                role: StreamRole::Host,
                reason: "stopped".to_string(),
            });
            self.cancel_host_start(start);
            return;
        }
        self.end_hosted_stream("stopped").await;
    }

    /// End the hosted stream: END STREAM ("stopped"), no SFU rejoin within
    /// 30 s ("connection_lost"), or the backend lost it ("lost_on_server").
    ///
    /// The UI is told first. Nothing after it waits on the network on the
    /// loop: the SFU leave and `stop_stream` run on their own tasks, the
    /// manager join is bounded, and native teardown runs on its own thread.
    /// On 2026-09-15 the UI event waited behind a native stop that never
    /// returned, and the button looked dead; a stalled backend must not do
    /// the same (stage 6 gate: a 10 s RPC stall freezes no END STREAM).
    pub(super) async fn end_hosted_stream(&mut self, reason: &str) {
        let Some(session) = self.stream_session.take() else {
            return;
        };
        log::info!(
            "Stopping stream session {} ({})",
            session.session_id,
            reason
        );
        let leave_conn = self.stream_sfu_connection.take();
        self.host_link = None;

        // The crew the stream is advertised in, not the crew the UI shows
        // now: the host can switch crews while it streams.
        let crew_id = Some(session.crew_id.clone()).filter(|c| !c.is_empty());
        let _ = self.event_tx.send(Event::StreamEnded {
            crew_id: crew_id.clone().unwrap_or_default(),
            role: StreamRole::Host,
            reason: reason.to_string(),
        });

        // A leave tells the SFU that the stream ended: the viewers get
        // session_ended at once. A host that only drops the socket looks
        // like a crash, and the SFU holds the session for a rejoin. Before
        // the RPC, so a slow backend cannot delay the viewers' end.
        if let Some(conn) = leave_conn {
            tokio::spawn(async move { leave_sfu_bounded(&conn, "host").await });
        }
        if let Some(crew_id) = crew_id {
            let payload = serde_json::json!({
                "crew_id": crew_id,
                "session_id": session.session_id,
            });
            self.spawn_rpc_logged("stop_stream", payload);
        }

        // Bounded. The native host stops on the teardown thread when the
        // manager task ends or is aborted.
        if let Some(sink) = self.stream_host_sfu_sink.take() {
            sink.detach();
        }
        session.stop_and_wait().await;

        self.stream_host_sink = None;
        self.host_pacing_last = None;
        self.host_pacing_last_at = Instant::now();
        self.stream_encode_width = 0;
        self.stream_encode_height = 0;
        self.stream_bitrate_kbps = 0;
    }

    /// Re-advertise the hosted stream after a Nakama realtime reconnect.
    ///
    /// The backend's session-end cleanup removes the stream records when the
    /// realtime socket drops (OnSessionEnd), and the SFU session and the
    /// encoder keep running. `start_stream` with `resume_session_id` restores
    /// the records for the same session. When the backend cannot restore
    /// them, no viewer can find the stream, so the host ends it. A start in
    /// flight is not advertised yet: `stream_live` advertises it.
    pub(super) async fn readvertise_stream_after_reconnect(&mut self) {
        let Some(session) = self.stream_session.as_ref() else {
            return;
        };
        if session.crew_id.is_empty() {
            return;
        }
        let crew_id = session.crew_id.clone();
        let session_id = session.session_id.clone();
        let resumed = tokio::time::timeout(
            super::stream_host_start::START_RPC_LIMIT,
            crate::stream::host::request_resume_stream(&self.nakama, &crew_id, &session_id),
        )
        .await;
        match resumed {
            Ok(Ok(_)) => log::info!(
                "Reconnect: stream session {} advertised again in crew {}",
                session_id,
                crew_id
            ),
            Ok(Err(e)) => {
                log::error!(
                    "Reconnect: stream session {} could not be advertised again: {}; ending it",
                    session_id,
                    e
                );
                self.end_hosted_stream("lost_on_server").await;
            }
            // The backend did not answer: keep the stream. The next resync
            // asks again.
            Err(_) => log::warn!(
                "Reconnect: the re-advertise of stream session {} got no answer",
                session_id
            ),
        }
    }

    /// A stream task reported (stage 6).
    pub(super) async fn on_stream_task(&mut self, msg: StreamTaskMsg) {
        match msg {
            StreamTaskMsg::Host(msg) => self.on_host_msg(msg).await,
            StreamTaskMsg::Viewer(msg) => self.on_viewer_msg(msg).await,
        }
    }

    /// Wake from sleep: the stream connections are dead. The host and the
    /// viewer rejoin their sessions at once, before the link checks see the
    /// silence.
    pub(super) async fn on_wake_for_streams(&mut self) {
        self.on_host_wake().await;
        self.on_viewer_wake().await;
    }
}

#[cfg(test)]
mod async_start_tests {
    use super::{NativeStartOutcome, NativeStartParams};

    fn assert_send<T: Send>() {}

    /// The start thread's params and outcome must cross threads. This fails
    /// to compile if a non-Send field (e.g. a raw HWND pointer) sneaks back
    /// into either type — the exact regression that blocked the off-loop
    /// start.
    #[test]
    fn start_thread_types_are_send() {
        assert_send::<NativeStartParams>();
        assert_send::<NativeStartOutcome>();
    }
}

#[cfg(test)]
mod capture_source_tests {
    use super::game_capture_source;
    use crate::catalogue::Head;

    #[test]
    fn a_known_game_carries_its_igdb_id() {
        let head = Head::bundled().expect("bundled head.bin must parse");
        let (known, source) = game_capture_source(
            Some(&head),
            4242,
            "witcher3.exe".into(),
            r"C:\Games\The Witcher 3\bin\x64\witcher3.exe",
            "The Witcher 3".into(),
            "witcher3.exe".into(),
            true,
        )
        .expect("a catalogue game is a source");
        assert!(known);
        assert_eq!(source.pid, Some(4242));
        assert_eq!(source.igdb_id, 1942, "The Witcher 3: Wild Hunt");
    }

    #[test]
    fn an_unknown_process_with_a_window_carries_zero() {
        let head = Head::bundled().expect("bundled head.bin must parse");
        let (known, source) = game_capture_source(
            Some(&head),
            7777,
            "my-indie-build.exe".into(),
            r"C:\dev\my-indie-build.exe",
            "My Indie Build".into(),
            "my-indie-build.exe".into(),
            false,
        )
        .expect("a process with a window is a source");
        assert!(!known);
        assert_eq!(source.igdb_id, 0);
        assert_eq!(source.name, "My Indie Build");
    }

    #[test]
    fn an_unknown_process_without_a_window_is_not_a_source() {
        let head = Head::bundled().expect("bundled head.bin must parse");
        assert!(game_capture_source(
            Some(&head),
            7778,
            "svchost.exe".into(),
            r"C:\Windows\System32\svchost.exe",
            String::new(),
            "svchost.exe".into(),
            false,
        )
        .is_none());
    }
}

#[cfg(test)]
mod streaming_rtp_tests {
    use crate::stream::rtp_peer::PeerMediaRole;
    use crate::transport::sfu_connection::StreamPeerRole;

    #[test]
    fn sfu_topology_roles_map_to_native_media_roles() {
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
    fn sfu_host_membership_events_route_to_sink() {
        let src = include_str!("streaming.rs");
        assert!(src.contains("poll_sfu_host_membership_events"));
        assert!(src.contains("SfuEvent::MemberJoined"));
        assert!(src.contains("sink.on_viewer_joined"));
        assert!(src.contains("sink.on_viewer_left"));
        assert!(src.contains("stream_sfu_connection"));
    }
}

/// A stream viewer against a fake SFU: the membership events reach the
/// viewer's member list (mello-sfu 01-SFU.md section 4.1).
#[cfg(test)]
mod stream_members_tests {
    use std::time::Duration;

    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    use super::super::stream_watch::{viewer_sfu_step, ViewerSfuStep};
    use super::{leave_sfu_bounded, viewer_membership_event};
    use crate::events::{Event, StreamMember};
    use crate::stream::StreamMembers;
    use crate::transport::sfu_connection::{SfuConnection, SfuEvent, StreamPeerRole};

    const LIMIT: Duration = Duration::from_secs(10);
    const SESSION: &str = "stream-1";

    fn text(value: serde_json::Value) -> Message {
        Message::Text(value.to_string())
    }

    /// A real SDP answer to a viewer's offer, from a native stream host peer
    /// that lives only inside this call.
    fn host_answer(offer: &str) -> String {
        let id = std::ffi::CString::new("fake-sfu").expect("static id");
        let offer = std::ffi::CString::new(offer).expect("offer has no NUL");
        // SAFETY: a null context is allowed; the peer is destroyed before
        // return and the answer is copied first.
        unsafe {
            let peer = mello_sys::mello_peer_create_for_role(
                std::ptr::null_mut(),
                id.as_ptr(),
                mello_sys::MelloPeerMediaRole_MELLO_PEER_MEDIA_ROLE_STREAM_HOST,
            );
            assert!(!peer.is_null(), "native answerer");
            let answer = mello_sys::mello_peer_create_answer(peer, offer.as_ptr());
            assert!(!answer.is_null(), "native answer");
            let text = std::ffi::CStr::from_ptr(answer)
                .to_string_lossy()
                .into_owned();
            mello_sys::mello_peer_destroy(peer);
            text
        }
    }

    /// A fake stream SFU. It answers `join_stream` with `joined` and
    /// `members`, answers the offer, then sends `after` in order and keeps
    /// the socket open.
    async fn stream_sfu(members: serde_json::Value, after: Vec<serde_json::Value>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let endpoint = format!(
            "ws://{}/ws",
            listener.local_addr().expect("listener has an address")
        );
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("the client connects");
            let mut ws = tokio_tungstenite::accept_async(socket)
                .await
                .expect("websocket handshake");
            ws.send(text(serde_json::json!({
                "type": "welcome",
                "data": { "server_id": "fake", "region": "test" }
            })))
            .await
            .expect("send welcome");
            let mut after = Some(after);
            while let Some(Ok(msg)) = ws.next().await {
                let Message::Text(t) = msg else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).expect("client sends JSON");
                match v["type"].as_str() {
                    Some("join_stream") => {
                        assert_eq!(v["data"]["role"], "viewer", "{v}");
                        ws.send(text(serde_json::json!({
                            "type": "joined",
                            "data": {
                                "session_type": "stream",
                                "session_id": SESSION,
                                "members": members.clone(),
                            }
                        })))
                        .await
                        .expect("send joined");
                    }
                    Some("offer") => {
                        let answer = host_answer(v["data"]["sdp"].as_str().expect("offer sdp"));
                        ws.send(text(
                            serde_json::json!({ "type": "answer", "data": { "sdp": answer } }),
                        ))
                        .await
                        .expect("send answer");
                        for msg in after.take().unwrap_or_default() {
                            ws.send(text(msg)).await.expect("send membership event");
                        }
                    }
                    _ => {}
                }
            }
        });
        endpoint
    }

    async fn join_as_viewer(endpoint: &str) -> (SfuConnection, StreamMembers) {
        let mut conn = tokio::time::timeout(LIMIT, SfuConnection::connect(endpoint, "token"))
            .await
            .expect("connect in time")
            .expect("connect");
        // SAFETY: a null context is allowed.
        let peer = unsafe {
            SfuConnection::create_stream_peer(std::ptr::null_mut(), StreamPeerRole::Viewer, &[])
        }
        .expect("viewer peer");
        let session = tokio::time::timeout(LIMIT, conn.join_stream(peer, SESSION, "viewer"))
            .await
            .expect("join in time")
            .expect("the join succeeds");
        assert_eq!(session.session_id, SESSION);
        (conn, StreamMembers::from_joined(&session.members))
    }

    fn member(user_id: &str, role: &str) -> StreamMember {
        StreamMember {
            user_id: user_id.into(),
            role: role.into(),
        }
    }

    fn joined(user_id: &str, role: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "member_joined",
            "data": { "user_id": user_id, "role": role }
        })
    }

    fn left(user_id: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "member_left",
            "data": { "user_id": user_id, "reason": "disconnect" }
        })
    }

    /// A viewer that joins before the host learns of the host and of a later
    /// viewer. The host's reconnect does not add a second host.
    #[tokio::test]
    async fn a_viewer_learns_of_a_late_host_and_of_later_viewers() {
        let endpoint = stream_sfu(
            serde_json::json!([]),
            vec![
                joined("host-1", "host"),
                joined("viewer-2", "viewer"),
                joined("host-1", "host"),
                left("viewer-2"),
            ],
        )
        .await;
        let (conn, mut list) = join_as_viewer(&endpoint).await;
        assert!(list.members().is_empty(), "the viewer joined first");

        let mut ui = Vec::new();
        for _ in 0..4 {
            let ev = tokio::time::timeout(LIMIT, conn.recv_event())
                .await
                .expect("a membership event in time")
                .expect("event channel open");
            assert!(
                matches!(
                    ev,
                    SfuEvent::MemberJoined { .. } | SfuEvent::MemberLeft { .. }
                ),
                "unexpected event {ev:?}"
            );
            ui.push(match viewer_membership_event(&mut list, &ev) {
                Some(Event::StreamMembersChanged { members }) => Some(members),
                Some(other) => panic!("unexpected UI event {other:?}"),
                None => None,
            });
        }
        assert_eq!(
            ui,
            [
                Some(vec![member("host-1", "host")]),
                Some(vec![member("host-1", "host"), member("viewer-2", "viewer")]),
                None,
                Some(vec![member("host-1", "host")]),
            ]
        );
        assert_eq!(list.host(), Some("host-1"));
        drop(conn);
    }

    /// mello-sfu holds a stream session when the host drops: the viewer gets
    /// `host_reconnecting`, and later `host_reconnected` if the host comes
    /// back. Stage 6: the viewer keeps its leg and its last frame on
    /// `host_reconnecting`; only `session_ended` ends the watch. Before, the
    /// viewer stopped on `host_reconnecting` as on a disconnect.
    #[tokio::test]
    async fn host_reconnecting_keeps_the_viewer() {
        let endpoint = stream_sfu(
            serde_json::json!([{ "user_id": "host-1", "role": "host" }]),
            vec![
                serde_json::json!({
                    "type": "host_reconnecting",
                    "data": { "user_id": "host-1", "reason": "host_disconnected", "hold_ms": 30000 }
                }),
                serde_json::json!({
                    "type": "host_reconnected",
                    "data": { "user_id": "host-1", "role": "host" }
                }),
                serde_json::json!({ "type": "session_ended", "data": {} }),
            ],
        )
        .await;
        let (conn, _) = join_as_viewer(&endpoint).await;

        let ev = tokio::time::timeout(LIMIT, conn.recv_event())
            .await
            .expect("host_reconnecting in time")
            .expect("event channel open");
        assert!(
            matches!(
                ev,
                SfuEvent::HostReconnecting { ref user_id, hold_ms: 30000 } if user_id == "host-1"
            ),
            "unexpected event {ev:?}"
        );
        assert_eq!(viewer_sfu_step(&ev), ViewerSfuStep::HostReconnecting);

        let ev = tokio::time::timeout(LIMIT, conn.recv_event())
            .await
            .expect("host_reconnected in time")
            .expect("event channel open");
        assert!(
            matches!(ev, SfuEvent::HostReconnected { ref user_id } if user_id == "host-1"),
            "unexpected event {ev:?}"
        );
        assert_eq!(viewer_sfu_step(&ev), ViewerSfuStep::Continue);

        let ev = tokio::time::timeout(LIMIT, conn.recv_event())
            .await
            .expect("session_ended in time")
            .expect("event channel open");
        assert_eq!(viewer_sfu_step(&ev), ViewerSfuStep::End("host_left"));
        drop(conn);
    }

    /// The bounded leave that a host sends on END STREAM reaches the SFU as
    /// `leave`, so mello-sfu ends the stream for the viewers at once instead
    /// of holding the session (01-SFU.md section 4.1).
    #[tokio::test]
    async fn the_bounded_leave_reaches_the_sfu() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let endpoint = format!(
            "ws://{}/ws",
            listener.local_addr().expect("listener has an address")
        );
        let (got_leave, leave_seen) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("the client connects");
            let mut ws = tokio_tungstenite::accept_async(socket)
                .await
                .expect("websocket handshake");
            ws.send(text(serde_json::json!({
                "type": "welcome",
                "data": { "server_id": "fake", "region": "test" }
            })))
            .await
            .expect("send welcome");
            let mut got_leave = Some(got_leave);
            while let Some(Ok(msg)) = ws.next().await {
                let Message::Text(t) = msg else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).expect("client sends JSON");
                if v["type"] == "leave" {
                    if let Some(tx) = got_leave.take() {
                        let _ = tx.send(());
                    }
                }
            }
        });
        let conn = tokio::time::timeout(LIMIT, SfuConnection::connect(&endpoint, "token"))
            .await
            .expect("connect in time")
            .expect("connect");
        leave_sfu_bounded(&conn, "host").await;
        tokio::time::timeout(LIMIT, leave_seen)
            .await
            .expect("the SFU got leave in time")
            .expect("leave signal");
    }

    /// An SFU that sends no membership events to viewers (the contract
    /// before mello-sfu stage 4d): the join works and the list is the
    /// `joined.members` snapshot.
    #[tokio::test]
    async fn an_sfu_without_viewer_events_leaves_the_join_snapshot() {
        let endpoint = stream_sfu(
            serde_json::json!([
                { "user_id": "host-1", "role": "host" },
                { "user_id": "viewer-0", "role": "viewer" }
            ]),
            Vec::new(),
        )
        .await;
        let (conn, list) = join_as_viewer(&endpoint).await;
        assert_eq!(
            list.members(),
            [member("host-1", "host"), member("viewer-0", "viewer")]
        );
        assert_eq!(list.host(), Some("host-1"));
        let quiet = tokio::time::timeout(Duration::from_millis(300), conn.recv_event()).await;
        assert!(quiet.is_err(), "no event from this SFU: {quiet:?}");
        drop(conn);
    }

    /// Join `session` on a real SFU in test-password mode as `user`.
    async fn live_join(
        endpoint: &str,
        password: &str,
        session: &str,
        user: &str,
        role: StreamPeerRole,
    ) -> (SfuConnection, StreamMembers) {
        let token = format!(
            "unused&test_password={password}&test_type=stream&test_user_id={user}&test_username={user}"
        );
        let mut conn = SfuConnection::connect(endpoint, &token)
            .await
            .expect("connect to the SFU");
        // SAFETY: a null context is allowed.
        let peer = unsafe { SfuConnection::create_stream_peer(std::ptr::null_mut(), role, &[]) }
            .expect("stream peer");
        let signaling_role = match role {
            StreamPeerRole::Host => "host",
            StreamPeerRole::Viewer => "viewer",
        };
        let joined = tokio::time::timeout(LIMIT, conn.join_stream(peer, session, signaling_role))
            .await
            .expect("join in time")
            .expect("join");
        (conn, StreamMembers::from_joined(&joined.members))
    }

    /// Against a real SFU: a viewer joins first, then the host, then a
    /// second viewer that leaves again. The first viewer's list must follow.
    /// Needs the mello-sfu contract where viewers get `member_joined` and
    /// `member_left` (mello-sfu stage 4d). An older SFU fails the test: the
    /// first viewer's list stays empty.
    ///
    /// ```bash
    /// MELLO_TEST_SFU=ws://127.0.0.1:8443/ws MELLO_TEST_SFU_PASSWORD=devpass \
    ///   cargo test -p mello-core --lib -- --ignored --nocapture stream_members_live
    /// ```
    #[tokio::test]
    #[ignore = "needs a local SFU: MELLO_TEST_SFU and MELLO_TEST_SFU_PASSWORD"]
    async fn stream_members_live() {
        let (Ok(endpoint), Ok(password)) = (
            std::env::var("MELLO_TEST_SFU"),
            std::env::var("MELLO_TEST_SFU_PASSWORD"),
        ) else {
            panic!("set MELLO_TEST_SFU and MELLO_TEST_SFU_PASSWORD");
        };
        let session = format!(
            "members-live-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default()
        );

        let (first, mut list) = live_join(
            &endpoint,
            &password,
            &session,
            "viewer-1",
            StreamPeerRole::Viewer,
        )
        .await;
        println!("viewer-1 joined: {:?}", list.members());
        let (host, _) = live_join(
            &endpoint,
            &password,
            &session,
            "host-1",
            StreamPeerRole::Host,
        )
        .await;
        let (second, second_list) = live_join(
            &endpoint,
            &password,
            &session,
            "viewer-2",
            StreamPeerRole::Viewer,
        )
        .await;
        println!("viewer-2 joined: {:?}", second_list.members());
        second.leave().await;
        drop(second);

        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, first.recv_event()).await {
            if let Some(Event::StreamMembersChanged { members }) =
                viewer_membership_event(&mut list, &ev)
            {
                println!("viewer-1 list: {members:?}");
                seen.push(members);
            }
        }
        drop(host);
        drop(first);

        assert!(
            seen.iter()
                .any(|m| m.iter().any(|x| x.user_id == "viewer-2")),
            "viewer-1 never learned of viewer-2: {seen:?}"
        );
        assert_eq!(
            list.members(),
            [member("host-1", "host")],
            "the late host stays, viewer-2 left"
        );
    }
}

/// Host stream lifecycle against a fake Nakama HTTP API and a fake SFU:
/// a failed start takes the advertised stream back, END STREAM and the
/// re-advertise after a Nakama reconnect use the crew the stream is in.
#[cfg(test)]
pub(super) mod stream_lifecycle_tests {
    use std::sync::atomic::{AtomicBool, AtomicU8};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    use futures::{SinkExt, StreamExt};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio_tungstenite::tungstenite::Message;

    use super::super::stream_host_start::{HostMsg, HostStart};
    use super::{join_stream_session, NativeStartOutcome, StreamHostHandle, StreamJoinLimits};
    use crate::client::Client;
    use crate::config::Config;
    use crate::events::{Event, StreamRole};
    use crate::stream::host::StartStreamResponse;
    use crate::stream::manager::StreamSession;
    use crate::transport::sfu_connection::StreamPeerRole;
    use crate::voice::VoiceManager;

    const LIMIT: Duration = Duration::from_secs(10);
    const STREAMED_CREW: &str = "crew-streamed";
    const ACTIVE_CREW: &str = "crew-active";
    const SESSION: &str = "stream_host_1";

    /// The RPCs the fake Nakama received: name and payload, in order.
    type Rpcs = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

    /// A fake Nakama HTTP API. It answers every RPC with an empty payload,
    /// or with an error for the names in `fail`, and records each call.
    async fn fake_nakama(fail: &'static [&'static str]) -> (u16, Rpcs) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let port = listener.local_addr().expect("listener address").port();
        let rpcs: Rpcs = Arc::default();
        let seen = Arc::clone(&rpcs);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut reader = BufReader::new(read);
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let mut content_length = 0usize;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            content_length = v.trim().parse().unwrap_or(0);
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let mut body = vec![0u8; content_length];
                    if reader.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    let path = request_line.split_whitespace().nth(1).unwrap_or_default();
                    let name = path
                        .strip_prefix("/v2/rpc/")
                        .unwrap_or(path)
                        .split('?')
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    // Nakama wraps the RPC payload in a JSON string.
                    let payload = serde_json::from_slice::<String>(&body)
                        .ok()
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or(serde_json::Value::Null);
                    let failed = fail.contains(&name.as_str());
                    seen.lock().expect("rpc log").push((name, payload));
                    let (status, reply) = if failed {
                        (
                            "404 Not Found",
                            r#"{"error":"stream not resumable","code":5}"#,
                        )
                    } else {
                        ("200 OK", r#"{"payload":"{}"}"#)
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = write.write_all(response.as_bytes()).await;
                });
            }
        });
        (port, rpcs)
    }

    fn client_for(port: u16) -> (Client, mpsc::Receiver<Event>) {
        let mut config = Config::development();
        config.nakama_host = "127.0.0.1".into();
        config.nakama_port = port;
        config.nakama_ssl = false;
        let (event_tx, events) = mpsc::channel();
        let voice = VoiceManager::with_test_audio_backend(event_tx.clone());
        let mut client = Client::with_voice(
            config,
            event_tx,
            voice,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU8::new(0)),
            false,
            false,
        );
        // The UI shows another crew than the one the stream is in.
        client
            .nakama
            .set_test_session("session-token", Some(ACTIVE_CREW));
        (client, events)
    }

    /// Wait until the fake Nakama got `name`: stage 6 sends the stream
    /// RPCs on their own tasks.
    async fn wait_for_rpc(rpcs: &Rpcs, name: &str) -> Vec<serde_json::Value> {
        let deadline = tokio::time::Instant::now() + LIMIT;
        loop {
            let calls = rpc_calls(rpcs, name);
            if !calls.is_empty() {
                return calls;
            }
            assert!(tokio::time::Instant::now() < deadline, "no {name} RPC");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// A host start whose `start_stream` answered with `endpoint`, as the
    /// start task leaves it. Returns its generation.
    pub(in crate::client) fn install_host_start(client: &mut Client, endpoint: &str) -> u64 {
        let generation = 7;
        let config = crate::stream::StreamConfig::from_preset(
            crate::stream::config::QualityPreset::Medium,
            crate::stream::config::Codec::H264,
        );
        client.host_start = Some(HostStart {
            generation,
            crew_id: STREAMED_CREW.to_string(),
            clicked_at: std::time::Instant::now(),
            config,
            configured_bitrate_kbps: 4_000,
            task: tokio::spawn(async {}),
            resp: Some(start_response(endpoint)),
            advertise_on_live: true,
            rpc_ms: Some(60),
            sink: Arc::new(crate::stream::sink_sfu::SfuSink::detached()),
            session: None,
            native_ms: None,
            encode_size: (0, 0),
            conn: None,
            sfu_ms: None,
        });
        generation
    }

    fn rpc_calls(rpcs: &Rpcs, name: &str) -> Vec<serde_json::Value> {
        rpcs.lock()
            .expect("rpc log")
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, p)| p.clone())
            .collect()
    }

    /// A native start that finished with no host: the tests drive the SFU
    /// half of the start.
    pub(in crate::client) fn native_outcome() -> NativeStartOutcome {
        let (_video_tx, video_rx) = tokio::sync::mpsc::channel(1);
        let (_audio_tx, audio_rx) = tokio::sync::mpsc::channel(1);
        NativeStartOutcome {
            host: StreamHostHandle(std::ptr::null_mut()),
            video_rx,
            audio_rx,
            teardown: crate::stream::teardown::NativeTeardownGuard::new("test-host", |_| {}),
            actual_w: 1280,
            actual_h: 720,
        }
    }

    fn start_response(endpoint: &str) -> StartStreamResponse {
        serde_json::from_value(serde_json::json!({
            "session_id": SESSION,
            "stream_id": SESSION,
            "mode": "sfu",
            "sfu_endpoint": endpoint,
            "sfu_token": "token",
        }))
        .expect("valid start_stream response")
    }

    fn text(value: serde_json::Value) -> Message {
        Message::Text(value.to_string())
    }

    /// A stream SFU that rejects `join_stream`.
    async fn sfu_rejecting_join() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let endpoint = format!("ws://{}/ws", listener.local_addr().expect("address"));
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("the client connects");
            let mut ws = tokio_tungstenite::accept_async(socket)
                .await
                .expect("websocket handshake");
            ws.send(text(serde_json::json!({
                "type": "welcome",
                "data": { "server_id": "fake", "region": "test" }
            })))
            .await
            .expect("send welcome");
            while let Some(Ok(msg)) = ws.next().await {
                let Message::Text(t) = msg else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).expect("client sends JSON");
                if v["type"] == "join_stream" {
                    ws.send(text(serde_json::json!({
                        "type": "error",
                        "data": { "code": "SESSION_FULL", "message": "Session at capacity" }
                    })))
                    .await
                    .expect("send error");
                }
            }
        });
        endpoint
    }

    /// The SFU step of a start runs for real against `endpoint` (stage 6:
    /// on the start task) and fails; the loop takes the report.
    async fn assert_start_failure_retracts(endpoint: String) {
        let (port, rpcs) = fake_nakama(&[]).await;
        let (mut client, events) = client_for(port);
        let generation = install_host_start(&mut client, &endpoint);
        // The capture started already: the manager drains.
        tokio::time::timeout(
            LIMIT,
            client.on_host_msg(HostMsg::StartNative {
                generation,
                result: Ok(native_outcome()),
            }),
        )
        .await
        .expect("the capture step ends in time");
        let limits = StreamJoinLimits {
            connect: Duration::from_secs(2),
            session: Duration::from_secs(5),
        };
        let result = join_stream_session(
            endpoint,
            "token".into(),
            SESSION.into(),
            StreamPeerRole::Host,
            0,
            Vec::new(),
            limits,
        )
        .await
        .map(|(conn, _)| conn);
        assert!(result.is_err(), "the SFU step must fail");
        tokio::time::timeout(
            LIMIT,
            client.on_host_msg(HostMsg::StartSfu { generation, result }),
        )
        .await
        .expect("the start ends in time");

        assert_eq!(
            wait_for_rpc(&rpcs, "stop_stream").await,
            [serde_json::json!({ "crew_id": STREAMED_CREW, "session_id": SESSION })],
            "the started session is taken back in its own crew"
        );
        assert!(client.stream_session.is_none());
        assert!(client.host_start.is_none());
        let failures: Vec<Event> = events
            .try_iter()
            .filter(|e| matches!(e, Event::StreamFailed { .. }))
            .collect();
        assert!(
            matches!(
                failures.as_slice(),
                [Event::StreamFailed { role: StreamRole::Host, reason }]
                    if reason == "Could not reach the stream server."
            ),
            "one failure that names the cause: {failures:?}"
        );
    }

    /// The SFU does not answer: the crew must not keep a stream that no SFU
    /// session carries.
    #[tokio::test]
    async fn a_failed_sfu_connect_takes_the_advertised_stream_back() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("ws://{}/ws", closed.local_addr().expect("address"));
        drop(closed);
        assert_start_failure_retracts(endpoint).await;
    }

    #[tokio::test]
    async fn a_rejected_sfu_join_takes_the_advertised_stream_back() {
        assert_start_failure_retracts(sfu_rejecting_join().await).await;
    }

    fn hosted_session(client: &mut Client) {
        let (stop_tx, _stop_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async {});
        client.stream_session = Some(
            StreamSession::new(SESSION.to_string(), "sfu".to_string(), stop_tx, task)
                .with_crew_id(STREAMED_CREW),
        );
    }

    /// END STREAM stops the stream in the crew it is in, not in the crew the
    /// UI shows now.
    #[tokio::test]
    async fn end_stream_uses_the_crew_of_the_stream() {
        let (port, rpcs) = fake_nakama(&[]).await;
        let (mut client, events) = client_for(port);
        hosted_session(&mut client);

        tokio::time::timeout(LIMIT, client.handle_stop_stream())
            .await
            .expect("END STREAM ends in time");

        assert_eq!(
            wait_for_rpc(&rpcs, "stop_stream").await,
            [serde_json::json!({ "crew_id": STREAMED_CREW, "session_id": SESSION })]
        );
        let ended: Vec<Event> = events
            .try_iter()
            .filter(|e| matches!(e, Event::StreamEnded { .. }))
            .collect();
        assert!(
            matches!(
                ended.as_slice(),
                [Event::StreamEnded { crew_id, role: StreamRole::Host, reason }]
                    if crew_id == STREAMED_CREW && reason == "stopped"
            ),
            "{ended:?}"
        );
    }

    /// After a Nakama realtime reconnect the backend has removed the stream
    /// records (OnSessionEnd), while the SFU session still runs. The host
    /// advertises the same session again, in its own crew.
    #[tokio::test]
    async fn a_nakama_reconnect_advertises_the_live_stream_again() {
        let (port, rpcs) = fake_nakama(&[]).await;
        let (mut client, _events) = client_for(port);
        // No active crew: the resync of the crew view returns at once, and
        // the stream is advertised all the same.
        client.nakama.set_test_session("session-token", None);
        hosted_session(&mut client);

        tokio::time::timeout(LIMIT, client.resync_after_reconnect())
            .await
            .expect("the resync ends in time");

        assert_eq!(
            rpc_calls(&rpcs, "start_stream"),
            [serde_json::json!({ "crew_id": STREAMED_CREW, "resume_session_id": SESSION })]
        );
        assert!(client.stream_session.is_some(), "the stream keeps running");
    }

    /// A real SDP answer to the host's offer, from a native viewer peer that
    /// lives only inside this call.
    fn viewer_answer(offer: &str) -> String {
        let id = std::ffi::CString::new("fake-sfu").expect("static id");
        let offer = std::ffi::CString::new(offer).expect("offer has no NUL");
        // SAFETY: a null context is allowed; the peer is destroyed before
        // return and the answer is copied first.
        unsafe {
            let peer = mello_sys::mello_peer_create_for_role(
                std::ptr::null_mut(),
                id.as_ptr(),
                mello_sys::MelloPeerMediaRole_MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER,
            );
            assert!(!peer.is_null(), "native answerer");
            let answer = mello_sys::mello_peer_create_answer(peer, offer.as_ptr());
            assert!(!answer.is_null(), "native answer");
            let text = std::ffi::CStr::from_ptr(answer)
                .to_string_lossy()
                .into_owned();
            mello_sys::mello_peer_destroy(peer);
            text
        }
    }

    /// A fake stream SFU: it answers the host's `join_stream` and offer, then
    /// sends `member_left` for `viewer`.
    async fn sfu_sending_member_left(viewer: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let endpoint = format!("ws://{}/ws", listener.local_addr().expect("address"));
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("the client connects");
            let mut ws = tokio_tungstenite::accept_async(socket)
                .await
                .expect("websocket handshake");
            ws.send(text(serde_json::json!({
                "type": "welcome",
                "data": { "server_id": "fake", "region": "test" }
            })))
            .await
            .expect("send welcome");
            while let Some(Ok(msg)) = ws.next().await {
                let Message::Text(t) = msg else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).expect("client sends JSON");
                match v["type"].as_str() {
                    Some("join_stream") => {
                        ws.send(text(serde_json::json!({
                            "type": "joined",
                            "data": { "session_type": "stream", "session_id": SESSION, "members": [] }
                        })))
                        .await
                        .expect("send joined");
                    }
                    Some("offer") => {
                        let answer = viewer_answer(v["data"]["sdp"].as_str().expect("offer sdp"));
                        ws.send(text(
                            serde_json::json!({ "type": "answer", "data": { "sdp": answer } }),
                        ))
                        .await
                        .expect("send answer");
                        ws.send(text(serde_json::json!({
                            "type": "member_left",
                            "data": { "user_id": viewer, "reason": "disconnect", "role": "viewer" }
                        })))
                        .await
                        .expect("send member_left");
                    }
                    _ => {}
                }
            }
        });
        endpoint
    }

    /// A viewer that leaves the SFU session leaves the crew's viewer count:
    /// the host reports it with `stream_viewer_left`.
    #[tokio::test]
    async fn a_viewer_leaving_the_sfu_session_is_reported() {
        let (port, rpcs) = fake_nakama(&[]).await;
        let (mut client, _events) = client_for(port);
        hosted_session(&mut client);
        let endpoint = sfu_sending_member_left("viewer-7").await;
        let mut conn = tokio::time::timeout(
            LIMIT,
            crate::transport::SfuConnection::connect(&endpoint, "token"),
        )
        .await
        .expect("connect in time")
        .expect("connect");
        // SAFETY: a null context is allowed.
        let peer = unsafe {
            crate::transport::SfuConnection::create_stream_peer(
                std::ptr::null_mut(),
                crate::transport::sfu_connection::StreamPeerRole::Host,
                &[],
            )
        }
        .expect("host peer");
        tokio::time::timeout(LIMIT, conn.join_stream(peer, SESSION, "host"))
            .await
            .expect("join in time")
            .expect("the join succeeds");
        let conn = Arc::new(conn);
        client.stream_sfu_connection = Some(Arc::clone(&conn));
        client.stream_host_sink = Some(Arc::new(crate::stream::sink_sfu::SfuSink::new(conn)));

        let deadline = tokio::time::Instant::now() + LIMIT;
        while rpc_calls(&rpcs, "stream_viewer_left").is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no stream_viewer_left RPC"
            );
            client.poll_sfu_host_membership_events().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            rpc_calls(&rpcs, "stream_viewer_left"),
            [serde_json::json!({ "crew_id": STREAMED_CREW, "viewer_id": "viewer-7" })]
        );
    }

    /// When the backend cannot restore the stream, no viewer can find it:
    /// the host ends it and says why.
    #[tokio::test]
    async fn a_stream_the_backend_cannot_restore_ends() {
        let (port, rpcs) = fake_nakama(&["start_stream"]).await;
        let (mut client, events) = client_for(port);
        hosted_session(&mut client);

        tokio::time::timeout(LIMIT, client.readvertise_stream_after_reconnect())
            .await
            .expect("the re-advertise ends in time");

        assert!(client.stream_session.is_none());
        assert_eq!(
            wait_for_rpc(&rpcs, "stop_stream").await,
            [serde_json::json!({ "crew_id": STREAMED_CREW, "session_id": SESSION })]
        );
        assert!(events.try_iter().any(|e| matches!(
            e,
            Event::StreamEnded { role: StreamRole::Host, ref reason, .. } if reason == "lost_on_server"
        )));
    }
}
