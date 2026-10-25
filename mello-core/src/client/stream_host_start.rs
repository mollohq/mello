//! Stream host start and the host's SFU rejoin, off the command loop
//! (streaming-quality stage 6). Built like `sfu_voice_join.rs`.
//!
//! A start has three network or native steps. They run on one Tokio task;
//! each reports to the loop through [`super::streaming::StreamTasks`] when it
//! ends, and the loop finishes the start from those reports. The loop never
//! waits on a step.
//!
//! | Step | Runs on | Limit | Work |
//! |---|---|---|---|
//! | RPC | task | 10 s | `start_stream` with `advertise: "on_live"`: session id, SFU token, hook policy |
//! | Native | own thread | 10 s | Encoder check, capture init (hook injection included), encoder, audio |
//! | SFU | task | 5 s connect, 10 s join | WebSocket, stream peer, `join_stream`, offer and answer, track and control channel open |
//!
//! Native and SFU run at the same time, after the RPC (the hook policy comes
//! with it). When the native step ends, the stream manager starts at once on
//! a sink with no connection: it drains the encoder from the first access
//! unit, so the 32-AU channel cannot overflow while the SFU join runs, and it
//! drops what it cannot send as backpressure. When the SFU step ends, the
//! sink takes the connection and asks for an IDR, so the first access unit
//! on the open track is a key frame. When both ended the stream is live: the
//! UI gets `StreamStarted`, and `stream_live` advertises the stream to the
//! crew (until then the host card shows "starting").
//!
//! A failed step ends the start: the other steps are cancelled, the native
//! host stops through its teardown guard, `stop_stream` takes the session
//! back, and the UI gets `StreamFailed` with the cause (the stage 2b retract
//! paths, now spawned instead of awaited on the loop).
//!
//! After the start, [`HostLinkState`] keeps the host on the SFU: a silent
//! link shows "reconnecting"; a lost connection rejoins the same session on
//! the voice schedule with a new connection, and the sink moves to it. The
//! SFU token lives 5 minutes; the host refreshes it every 4 minutes in the
//! background (`refresh_stream_token`), so a rejoin needs no backend round
//! trip.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::events::{Event, StreamRole};
use crate::stream::host::StartStreamResponse;
use crate::stream::manager::StreamSession;
use crate::stream::sink_sfu::SfuSink;
use crate::stream::StreamConfig;
use crate::transport::sfu_connection::StreamPeerRole;
use crate::transport::SfuConnection;

use super::stream_link::{LinkAction, LinkObservation, StreamLink, LINK_PING_EVERY_TICKS};
use super::streaming::{
    join_stream_session, leave_sfu_bounded, start_host_blocking, NativeStartOutcome,
    NativeStartParams, RpcFuture, StreamGrant, StreamJoinLimits, StreamTaskMsg,
};

/// The longest the `start_stream` RPC may take. A stalled backend fails the
/// start instead of holding it.
pub(super) const START_RPC_LIMIT: Duration = Duration::from_secs(10);
/// The longest the native start may take (the hook injection wait and the
/// first-frame deadline are about 2 s).
pub(super) const NATIVE_START_LIMIT: Duration = Duration::from_secs(10);
/// The longest a token refresh may take before a rejoin.
pub(super) const TOKEN_RPC_LIMIT: Duration = Duration::from_secs(5);

/// A step of a host start or rejoin that ended. `generation` names the start
/// or the link it belongs to; a report of a cancelled one is dropped.
pub(super) enum HostMsg {
    StartRpc {
        generation: u64,
        result: Result<StartStreamResponse, String>,
        advertise_on_live: bool,
    },
    StartNative {
        generation: u64,
        result: Result<NativeStartOutcome, String>,
    },
    StartSfu {
        generation: u64,
        result: Result<SfuConnection, String>,
    },
    Rejoin {
        generation: u64,
        result: Result<(SfuConnection, Vec<String>, Option<StreamGrant>), String>,
    },
    Token {
        generation: u64,
        result: Result<StreamGrant, String>,
    },
}

/// What the native thread needs, less the hook decision (from the RPC).
pub(super) struct NativeStartPlan {
    pub params: NativeStartParams,
    pub igdb_id: u32,
    pub exe: String,
    pub is_process: bool,
}

/// A host start in flight. Owned by the command loop.
pub(super) struct HostStart {
    pub generation: u64,
    pub crew_id: String,
    pub clicked_at: Instant,
    pub config: StreamConfig,
    pub configured_bitrate_kbps: u32,
    pub task: JoinHandle<()>,
    pub resp: Option<StartStreamResponse>,
    pub advertise_on_live: bool,
    pub rpc_ms: Option<u128>,
    pub sink: Arc<SfuSink>,
    pub session: Option<StreamSession>,
    pub native_ms: Option<u128>,
    pub encode_size: (u32, u32),
    pub conn: Option<Arc<SfuConnection>>,
    pub sfu_ms: Option<u128>,
}

/// The step durations of a start, logged with the first RTP.
#[derive(Debug, Clone, Copy)]
pub(super) struct StartTiming {
    pub clicked_at: Instant,
    pub rpc_ms: u128,
    pub native_ms: u128,
    pub sfu_ms: u128,
    pub live_ms: u128,
}

/// The host's link to the SFU after the start.
pub(super) struct HostLinkState {
    pub generation: u64,
    pub link: StreamLink,
    pub grant: StreamGrant,
    pub session_id: String,
    /// When the current connection opened: the silence before its first
    /// pong counts from here.
    pub connected_at: Instant,
    pub attempt: Option<JoinHandle<()>>,
    pub token_task: Option<JoinHandle<()>>,
    pub token_tried_at: Option<tokio::time::Instant>,
    pub ticks: u64,
    /// Logged once, when the first access unit went out.
    pub start_timing: Option<StartTiming>,
    /// Stage 9 (streaming-quality): the viewers the UI was last told of
    /// (`StreamViewerJoined` / `StreamViewerLeft`), for the hosted card's
    /// count.
    pub reported_viewers: std::collections::BTreeSet<String>,
}

impl Drop for HostLinkState {
    fn drop(&mut self) {
        if let Some(t) = self.attempt.take() {
            t.abort();
        }
        if let Some(t) = self.token_task.take() {
            t.abort();
        }
    }
}

/// The user-facing cause of a failed start step.
pub(super) fn start_failure_reason(step: &str, detail: &str) -> String {
    match step {
        "rpc" if detail.contains("SFU_UNAVAILABLE") => {
            "Streaming is not available on the server right now.".to_string()
        }
        "rpc" if detail.contains("took longer") => {
            "The server did not answer the stream start.".to_string()
        }
        "rpc" => "The server did not accept the stream.".to_string(),
        "sfu" => "Could not reach the stream server.".to_string(),
        // The native step carries its own sentence (no encoder, capture).
        _ => detail.to_string(),
    }
}

/// Run a start: the RPC, then the native start and the SFU join at the
/// same time. Each step reports to `tx` when it ends.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_host_start(
    generation: u64,
    rpc: impl std::future::Future<Output = crate::error::Result<String>> + Send + 'static,
    plan: NativeStartPlan,
    ctx: usize,
    ice_servers: Vec<String>,
    limits: StreamJoinLimits,
    tx: UnboundedSender<StreamTaskMsg>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let send = |msg: HostMsg| {
            let _ = tx.send(StreamTaskMsg::Host(msg));
        };
        let rpc_result = match tokio::time::timeout(START_RPC_LIMIT, rpc).await {
            Ok(Ok(body)) => serde_json::from_str::<StartStreamResponse>(&body)
                .map(|resp| {
                    let on_live = serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| {
                            v.get("advertise")
                                .and_then(|a| a.as_str())
                                .map(str::to_string)
                        })
                        .is_some_and(|a| a == "on_live");
                    (resp, on_live)
                })
                .map_err(|e| format!("start_stream answer: {e}")),
            Ok(Err(e)) => Err(format!("start_stream: {e}")),
            Err(_) => Err(format!(
                "start_stream took longer than {:?}",
                START_RPC_LIMIT
            )),
        };
        let (resp, advertise_on_live) = match rpc_result {
            Ok(v) => v,
            Err(e) => {
                send(HostMsg::StartRpc {
                    generation,
                    result: Err(e),
                    advertise_on_live: false,
                });
                return;
            }
        };
        send(HostMsg::StartRpc {
            generation,
            result: Ok(resp.clone()),
            advertise_on_live,
        });

        // Gate 1 of the hook policy (plan section 8): the backend capture
        // block carries the kill switch and the id lists.
        let mut params = plan.params;
        if plan.is_process {
            params.allow_hook =
                crate::stream::host::hook_allowed_for_game(plan.igdb_id, resp.capture.as_ref());
            log::info!(
                "Hook policy: exe={:?} igdb_id={} allow_hook={} policy_version={}",
                plan.exe,
                plan.igdb_id,
                params.allow_hook,
                resp.capture
                    .as_ref()
                    .map(|c| c.policy_version.as_str())
                    .unwrap_or("none")
            );
        }

        let native = async {
            let (native_tx, native_rx) = tokio::sync::oneshot::channel();
            let spawned = std::thread::Builder::new()
                .name("stream_start".to_string())
                .spawn(move || {
                    // A cancelled start drops the receiver: the outcome then
                    // drops here, and its teardown guard stops the host.
                    let _ = native_tx.send(start_host_blocking(params));
                });
            let result = match spawned {
                Err(e) => Err(format!(
                    "Could not start capture (thread spawn failed: {e})"
                )),
                Ok(_) => match tokio::time::timeout(NATIVE_START_LIMIT, native_rx).await {
                    Ok(Ok(outcome)) => outcome,
                    Ok(Err(_)) => Err("The capture start ended without a result".to_string()),
                    Err(_) => Err(format!(
                        "The capture did not start within {} s",
                        NATIVE_START_LIMIT.as_secs()
                    )),
                },
            };
            send(HostMsg::StartNative { generation, result });
        };
        let sfu = async {
            let endpoint = resp.sfu_endpoint.clone().unwrap_or_default();
            let token = resp.sfu_token.clone().unwrap_or_default();
            let result = join_stream_session(
                endpoint,
                token,
                resp.session_id(),
                StreamPeerRole::Host,
                ctx,
                ice_servers,
                limits,
            )
            .await
            .map(|(conn, _members)| conn);
            send(HostMsg::StartSfu { generation, result });
        };
        tokio::join!(native, sfu);
    })
}

/// One rejoin attempt of the host: a new token first when the kept one
/// expired, then a new connection to the same session.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_host_rejoin(
    generation: u64,
    grant: StreamGrant,
    refresh: Option<RpcFuture>,
    session_id: String,
    ctx: usize,
    ice_servers: Vec<String>,
    limits: StreamJoinLimits,
    tx: UnboundedSender<StreamTaskMsg>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = async {
            let (grant, fresh) = match refresh {
                Some(rpc) => {
                    let new = refresh_grant(rpc).await?;
                    (new.clone(), Some(new))
                }
                None => (grant, None),
            };
            let (conn, members) = join_stream_session(
                grant.endpoint.clone(),
                grant.token.clone(),
                session_id,
                StreamPeerRole::Host,
                ctx,
                ice_servers,
                limits,
            )
            .await?;
            let viewers = members
                .into_iter()
                .filter(|m| m.role == "viewer")
                .map(|m| m.user_id)
                .collect();
            Ok::<_, String>((conn, viewers, fresh))
        }
        .await;
        let _ = tx.send(StreamTaskMsg::Host(HostMsg::Rejoin { generation, result }));
    })
}

/// A `refresh_stream_token` answer as a grant.
pub(super) async fn refresh_grant(
    rpc: impl std::future::Future<Output = crate::error::Result<String>>,
) -> Result<StreamGrant, String> {
    let body = match tokio::time::timeout(TOKEN_RPC_LIMIT, rpc).await {
        Ok(Ok(body)) => body,
        Ok(Err(e)) => return Err(format!("refresh_stream_token: {e}")),
        Err(_) => {
            return Err(format!(
                "refresh_stream_token took longer than {:?}",
                TOKEN_RPC_LIMIT
            ))
        }
    };
    #[derive(serde::Deserialize)]
    struct Answer {
        sfu_endpoint: String,
        sfu_token: String,
    }
    let answer: Answer =
        serde_json::from_str(&body).map_err(|e| format!("refresh_stream_token answer: {e}"))?;
    Ok(StreamGrant::new(answer.sfu_endpoint, answer.sfu_token))
}

impl super::Client {
    fn next_stream_generation(&mut self) -> u64 {
        self.stream_generation = self.stream_generation.wrapping_add(1);
        self.stream_generation
    }

    /// StartStream: check the request, then run the start off the loop.
    /// Returns at once; the host card shows "starting".
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
        exe: &str,
        igdb_id: u32,
    ) {
        if self.stream_session.is_some() || self.host_start.is_some() {
            let _ = self.event_tx.send(Event::StreamError {
                message: "Already streaming".to_string(),
            });
            return;
        }
        let clicked_at = Instant::now();

        let quality_preset = match preset_idx {
            0 => crate::stream::config::QualityPreset::Ultra,
            1 => crate::stream::config::QualityPreset::High,
            3 => crate::stream::config::QualityPreset::Low,
            4 => crate::stream::config::QualityPreset::Potato,
            _ => crate::stream::config::QualityPreset::Medium,
        };
        log::info!("Starting stream with preset: {:?}", quality_preset);

        // Resolve the capture target before anything else, and refuse to
        // guess: a stream of the wrong thing looks like a stream of nothing.
        let target = match crate::stream::config::CaptureTarget::resolve(
            capture_mode,
            monitor_index,
            hwnd,
            pid,
        ) {
            Ok(t) => t,
            Err(e) => {
                log::error!("Stream start rejected: {}", e);
                let _ = self.event_tx.send(Event::StreamFailed {
                    role: StreamRole::Host,
                    reason: e,
                });
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

        let mut config =
            StreamConfig::from_preset(quality_preset, crate::stream::config::Codec::H264);
        config.capture_desc = target.describe();
        let configured_bitrate_kbps = config.bitrate_kbps;

        let mut payload = match serde_json::to_value(crate::stream::host::StartStreamRequest {
            crew_id: crew_id.to_string(),
            title: title.to_string(),
            supports_av1: false,
            width: config.width,
            height: config.height,
            bitrate_kbps: config.bitrate_kbps,
            exe: exe.to_string(),
            igdb_id,
            preferred_region: crate::transport::sfu_region::preferred_region(&self.sfu_regions),
        }) {
            Ok(v) => v,
            Err(e) => {
                log::error!("start_stream payload: {}", e);
                return;
            }
        };
        if let Some(map) = payload.as_object_mut() {
            // The crew learns of the stream from stream_live, when the track
            // is open; an older backend ignores the field and advertises now.
            map.insert("advertise".into(), "on_live".into());
            map.insert("fps".into(), config.fps.into());
        }
        let rpc = match self.nakama.rpc_detached("start_stream", &payload) {
            Ok(rpc) => rpc,
            Err(e) => {
                log::error!("start_stream RPC failed: {}", e);
                let _ = self.event_tx.send(Event::StreamFailed {
                    role: StreamRole::Host,
                    reason: "You are not signed in.".to_string(),
                });
                return;
            }
        };

        let ctx = self.voice.mello_ctx();
        let (mode, monitor_index, hwnd_addr, pid, is_process) = match target {
            crate::stream::config::CaptureTarget::Window { hwnd } => (
                mello_sys::MelloCaptureMode_MELLO_CAPTURE_WINDOW,
                0,
                hwnd as usize,
                0,
                false,
            ),
            crate::stream::config::CaptureTarget::Process { pid } => (
                mello_sys::MelloCaptureMode_MELLO_CAPTURE_PROCESS,
                0,
                0,
                pid,
                true,
            ),
            crate::stream::config::CaptureTarget::Monitor { index } => (
                mello_sys::MelloCaptureMode_MELLO_CAPTURE_MONITOR,
                index,
                0,
                0,
                false,
            ),
        };
        let plan = NativeStartPlan {
            params: NativeStartParams {
                ctx: ctx as usize,
                mode,
                monitor_index,
                hwnd: hwnd_addr,
                pid,
                allow_hook: false,
                width: config.width,
                height: config.height,
                fps: config.fps,
                bitrate_kbps: config.bitrate_kbps,
            },
            igdb_id,
            exe: exe.to_string(),
            is_process,
        };

        let generation = self.next_stream_generation();
        let task = spawn_host_start(
            generation,
            rpc,
            plan,
            ctx as usize,
            self.ice_servers.clone(),
            self.stream_join_limits,
            self.stream_tasks.sender(),
        );
        log::info!(
            "Stream start {} for crew {} runs off the command loop",
            generation,
            crew_id
        );
        self.host_start = Some(HostStart {
            generation,
            crew_id: crew_id.to_string(),
            clicked_at,
            config,
            configured_bitrate_kbps,
            task,
            resp: None,
            advertise_on_live: false,
            rpc_ms: None,
            sink: Arc::new(SfuSink::detached()),
            session: None,
            native_ms: None,
            encode_size: (0, 0),
            conn: None,
            sfu_ms: None,
        });
        let _ = self.event_tx.send(Event::StreamStarting {
            crew_id: crew_id.to_string(),
        });
    }

    /// A step of a host start or of the host link ended.
    pub(super) async fn on_host_msg(&mut self, msg: HostMsg) {
        match msg {
            HostMsg::StartRpc {
                generation,
                result,
                advertise_on_live,
            } => {
                let Some(start) = self.host_start_for(generation) else {
                    return;
                };
                match result {
                    Ok(resp) => {
                        let ms = start.clicked_at.elapsed().as_millis();
                        start.rpc_ms = Some(ms);
                        start.advertise_on_live = advertise_on_live;
                        log::info!(
                            "Stream start: start_stream answered in {} ms (session {}, advertise on live: {})",
                            ms,
                            resp.session_id(),
                            advertise_on_live
                        );
                        start.resp = Some(resp);
                    }
                    Err(e) => self.fail_host_start("rpc", &e).await,
                }
            }
            HostMsg::StartNative { generation, result } => {
                if self.host_start_for(generation).is_none() {
                    // A cancelled start: dropping the outcome stops the
                    // host through its teardown guard.
                    drop(result);
                    return;
                }
                match result {
                    Ok(outcome) => self.on_host_native_started(outcome).await,
                    Err(e) => self.fail_host_start("native", &e).await,
                }
            }
            HostMsg::StartSfu { generation, result } => {
                let Some(start) = self.host_start_for(generation) else {
                    if let Ok(conn) = result {
                        let conn = Arc::new(conn);
                        tokio::spawn(async move { leave_sfu_bounded(&conn, "host").await });
                    }
                    return;
                };
                match result {
                    Ok(conn) => {
                        let ms = start.clicked_at.elapsed().as_millis();
                        start.sfu_ms = Some(ms);
                        log::info!("Stream start: SFU track open {} ms after the click", ms);
                        let conn = Arc::new(conn);
                        start.sink.attach(Arc::clone(&conn));
                        start.conn = Some(conn);
                        self.finish_host_start_if_ready();
                    }
                    Err(e) => self.fail_host_start("sfu", &e).await,
                }
            }
            HostMsg::Rejoin { generation, result } => {
                self.on_host_rejoin(generation, result).await;
            }
            HostMsg::Token { generation, result } => {
                let Some(state) = self
                    .host_link
                    .as_mut()
                    .filter(|s| s.generation == generation)
                else {
                    return;
                };
                state.token_task = None;
                match result {
                    Ok(grant) => {
                        log::info!("Stream host token refreshed");
                        state.grant = grant;
                    }
                    Err(e) => log::warn!("Stream host token refresh failed: {}", e),
                }
            }
        }
    }

    fn host_start_for(&mut self, generation: u64) -> Option<&mut HostStart> {
        self.host_start
            .as_mut()
            .filter(|s| s.generation == generation)
    }

    async fn on_host_native_started(&mut self, outcome: NativeStartOutcome) {
        let ctx = self.voice.mello_ctx();
        let event_tx = self.event_tx.clone();
        let Some(start) = self.host_start.as_mut() else {
            return;
        };
        let Some(resp) = start.resp.clone() else {
            return;
        };
        let ms = start.clicked_at.elapsed().as_millis();
        start.native_ms = Some(ms);
        let NativeStartOutcome {
            host,
            video_rx,
            audio_rx,
            teardown,
            actual_w,
            actual_h,
        } = outcome;
        log::info!(
            "Stream start: capture and encoder running {} ms after the click ({}x{}); the manager drains from the first access unit",
            ms,
            actual_w,
            actual_h
        );
        start.encode_size = (actual_w, actual_h);
        let sink: Arc<dyn crate::stream::sink::PacketSink> = start.sink.clone();
        match crate::stream::host::create_stream_session(
            ctx,
            host.0,
            &resp,
            start.config.clone(),
            video_rx,
            audio_rx,
            teardown,
            sink,
            Some(event_tx),
        ) {
            Ok(session) => {
                start.session = Some(session.with_crew_id(start.crew_id.clone()));
                self.finish_host_start_if_ready();
            }
            Err(e) => {
                // The teardown guard moved into create_stream_session and
                // was dropped there: the host stopped.
                self.fail_host_start("native", &format!("Failed to create stream session: {e}"))
                    .await;
            }
        }
    }

    /// Both the encoder and the SFU track are up: the stream is live.
    fn finish_host_start_if_ready(&mut self) {
        let ready = self
            .host_start
            .as_ref()
            .is_some_and(|s| s.session.is_some() && s.conn.is_some());
        if !ready {
            return;
        }
        let Some(mut start) = self.host_start.take() else {
            return;
        };
        let (Some(session), Some(conn), Some(resp)) =
            (start.session.take(), start.conn.take(), start.resp.take())
        else {
            return;
        };
        let live_ms = start.clicked_at.elapsed().as_millis();
        let crew_id = start.crew_id.clone();
        let session_id = session.session_id.clone();
        log::info!(
            "Stream live {} ms after the click: start_stream {} ms, capture {} ms, SFU {} ms (session {})",
            live_ms,
            start.rpc_ms.unwrap_or_default(),
            start.native_ms.unwrap_or_default(),
            start.sfu_ms.unwrap_or_default(),
            session_id
        );
        let _ = self.event_tx.send(Event::StreamStarted {
            crew_id: crew_id.clone(),
            session_id: session_id.clone(),
            mode: session.mode.clone(),
        });
        let (w, h) = start.encode_size;
        self.stream_encode_width = w;
        self.stream_encode_height = h;
        self.stream_bitrate_kbps = start.configured_bitrate_kbps;
        self.host_pacing_last = None;
        self.host_pacing_last_at = Instant::now();
        let sink: Arc<dyn crate::stream::sink::PacketSink> = start.sink.clone();
        self.stream_host_sink = Some(sink);
        self.stream_host_sfu_sink = Some(Arc::clone(&start.sink));
        self.stream_sfu_connection = Some(conn);
        self.stream_session = Some(session);

        let grant = StreamGrant::new(
            resp.sfu_endpoint.clone().unwrap_or_default(),
            resp.sfu_token.clone().unwrap_or_default(),
        );
        let generation = start.generation;
        self.host_link = Some(HostLinkState {
            generation,
            link: StreamLink::new(StreamRole::Host),
            grant,
            session_id: session_id.clone(),
            connected_at: Instant::now(),
            attempt: None,
            token_task: None,
            token_tried_at: None,
            ticks: 0,
            reported_viewers: Default::default(),
            start_timing: Some(StartTiming {
                clicked_at: start.clicked_at,
                rpc_ms: start.rpc_ms.unwrap_or_default(),
                native_ms: start.native_ms.unwrap_or_default(),
                sfu_ms: start.sfu_ms.unwrap_or_default(),
                live_ms,
            }),
        });

        if start.advertise_on_live {
            let payload = serde_json::json!({
                "crew_id": crew_id,
                "session_id": session_id,
                "width": w,
                "height": h,
                "fps": start.config.fps,
                "bitrate_kbps": start.configured_bitrate_kbps,
            });
            self.spawn_rpc_logged("stream_live", payload);
        } else {
            // An older backend advertised at the start; it learns the real
            // encode size here, as before.
            let payload = serde_json::json!({ "crew_id": crew_id, "width": w, "height": h });
            self.spawn_rpc_logged("update_stream_resolution", payload);
        }
    }

    /// Send an RPC on its own task and log a failure. The loop never waits
    /// on it.
    pub(super) fn spawn_rpc_logged(&self, name: &'static str, payload: serde_json::Value) {
        match self.nakama.rpc_detached(name, &payload) {
            Ok(request) => {
                tokio::spawn(async move {
                    match request.await {
                        Ok(_) => log::info!("{} RPC done", name),
                        Err(e) => log::warn!("{} RPC failed: {}", name, e),
                    }
                });
            }
            Err(e) => log::warn!("{} RPC not sent: {}", name, e),
        }
    }

    /// End a start that cannot complete: cancel the other steps, stop the
    /// native host, take the advertised session back, and say why.
    pub(super) async fn fail_host_start(&mut self, step: &str, detail: &str) {
        let Some(start) = self.host_start.take() else {
            return;
        };
        log::error!(
            "Stream start failed at the {} step after {} ms: {}",
            step,
            start.clicked_at.elapsed().as_millis(),
            detail
        );
        self.cancel_host_start(start);
        let _ = self.event_tx.send(Event::StreamFailed {
            role: StreamRole::Host,
            reason: start_failure_reason(step, detail),
        });
    }

    /// Stop a start in flight: its task, the manager and native host (the
    /// session drop stops them through the teardown guard), the SFU
    /// connection (bounded leave on its own task), and the advertised
    /// session (`stop_stream` on its own task).
    pub(super) fn cancel_host_start(&mut self, mut start: HostStart) {
        start.task.abort();
        start.sink.detach();
        drop(start.session.take());
        if let Some(conn) = start.conn.take() {
            tokio::spawn(async move { leave_sfu_bounded(&conn, "host").await });
        }
        if let Some(resp) = start.resp.as_ref() {
            log::info!(
                "Stream host did not start; retracting the session for crew {}",
                start.crew_id
            );
            let payload = serde_json::json!({
                "crew_id": start.crew_id,
                "session_id": resp.session_id(),
            });
            self.spawn_rpc_logged("stop_stream", payload);
        }
    }

    /// The host's link on one stream tick: ping, watch for a drop or a
    /// silence, rejoin on the schedule, refresh the token, log the start
    /// timing once.
    pub(super) async fn host_link_tick(&mut self) {
        self.report_host_viewer_changes();
        let Some(conn) = self.stream_sfu_connection.clone() else {
            return;
        };
        let now = Instant::now();
        let Some(state) = self.host_link.as_mut() else {
            return;
        };
        state.ticks = state.ticks.wrapping_add(1);
        if state.ticks.is_multiple_of(LINK_PING_EVERY_TICKS) {
            conn.send_ping();
        }

        if let Some(timing) = state.start_timing {
            if let Some((_, first)) = self
                .stream_host_sfu_sink
                .as_ref()
                .and_then(|s| s.first_video_sent())
            {
                log::info!(
                    "Stream start timing: click_to_first_rtp_ms={} click_to_live_ms={} rpc_ms={} capture_ms={} sfu_ms={}",
                    first.saturating_duration_since(timing.clicked_at).as_millis(),
                    timing.live_ms,
                    timing.rpc_ms,
                    timing.native_ms,
                    timing.sfu_ms
                );
                state.start_timing = None;
            }
        }

        let mut actions = Vec::new();
        if conn.is_ice_dead() && !state.link.is_rebuilding() {
            log::warn!("Stream host: ICE failed; rejoining the SFU");
            actions.extend(state.link.on_dropped(now));
        }
        let own_silent = super::stream_link::own_link_silence(&conn, state.connected_at, now);
        actions.extend(state.link.observe(
            now,
            LinkObservation {
                own_silent,
                host_silent: None,
                new_frame: false,
            },
        ));
        self.apply_host_link_actions(actions).await;
        self.refresh_host_token_if_due();
    }

    /// The host's signaling socket closed or the SFU sent a fatal error.
    pub(super) async fn on_host_link_dropped(&mut self, reason: &str) {
        let Some(state) = self.host_link.as_mut() else {
            return;
        };
        log::warn!(
            "Stream SFU host disconnected: {}; rejoining the same session",
            reason
        );
        let actions = state.link.on_dropped(Instant::now());
        self.apply_host_link_actions(actions).await;
    }

    /// Wake from sleep: the host's connection is dead; rejoin at once.
    pub(super) async fn on_host_wake(&mut self) {
        if self.host_link.is_some() {
            self.on_host_link_dropped("sleep_wake").await;
        }
    }

    pub(super) async fn apply_host_link_actions(&mut self, actions: Vec<LinkAction>) {
        for action in actions {
            match action {
                LinkAction::ShowReconnecting => {
                    log::warn!("Stream host link silent: the host card shows reconnecting");
                    let _ = self.event_tx.send(Event::StreamReconnecting {
                        role: StreamRole::Host,
                    });
                }
                LinkAction::ShowResumed => {
                    log::info!("Stream host link back");
                    let _ = self.event_tx.send(Event::StreamResumed {
                        role: StreamRole::Host,
                    });
                }
                LinkAction::Rejoin => self.start_host_rejoin(),
                LinkAction::GiveUp => {
                    log::error!(
                        "Stream host: no SFU rejoin within {} s; ending the stream",
                        super::voice::SFU_RECONNECT_GIVE_UP_AFTER.as_secs()
                    );
                    self.end_hosted_stream("connection_lost").await;
                    return;
                }
            }
        }
    }

    fn start_host_rejoin(&mut self) {
        let ctx = self.voice.mello_ctx() as usize;
        let ice = self.ice_servers.clone();
        let limits = self.stream_join_limits.for_rejoin();
        let tx = self.stream_tasks.sender();
        let now = tokio::time::Instant::now();
        let Some(state) = self.host_link.as_mut() else {
            return;
        };
        let refresh = if state.grant.is_fresh(now) {
            None
        } else {
            let payload = serde_json::json!({ "session_id": state.session_id });
            match self.nakama.rpc_detached("refresh_stream_token", &payload) {
                Ok(rpc) => Some(Box::pin(rpc) as RpcFuture),
                Err(e) => {
                    log::warn!(
                        "Stream host rejoin: no token refresh ({}); the old token goes",
                        e
                    );
                    None
                }
            }
        };
        log::info!(
            "Stream host rejoin attempt {} for session {} (token {})",
            state.link.attempt_number(),
            state.session_id,
            if refresh.is_some() {
                "refreshed first"
            } else {
                "kept"
            }
        );
        state.attempt = Some(spawn_host_rejoin(
            state.generation,
            state.grant.clone(),
            refresh,
            state.session_id.clone(),
            ctx,
            ice,
            limits,
            tx,
        ));
    }

    async fn on_host_rejoin(
        &mut self,
        generation: u64,
        result: Result<(SfuConnection, Vec<String>, Option<StreamGrant>), String>,
    ) {
        let now = Instant::now();
        let Some(state) = self
            .host_link
            .as_mut()
            .filter(|s| s.generation == generation)
        else {
            // The stream ended while the attempt ran: drop the connection
            // without a leave would hold the session; leave it.
            if let Ok((conn, _, _)) = result {
                let conn = Arc::new(conn);
                tokio::spawn(async move { leave_sfu_bounded(&conn, "host").await });
            }
            return;
        };
        state.attempt = None;
        match result {
            Ok((conn, viewers, grant)) => {
                if let Some(grant) = grant {
                    state.grant = grant;
                }
                state.connected_at = now;
                let actions = state.link.on_rejoined(now);
                let conn = Arc::new(conn);
                // The old connection drops without a leave: the SFU already
                // replaced the old host peer of this user.
                if let Some(sink) = self.stream_host_sfu_sink.as_ref() {
                    sink.attach(Arc::clone(&conn));
                }
                self.stream_sfu_connection = Some(conn);
                log::info!(
                    "Stream host rejoined session {} with {} viewer(s)",
                    self.host_link
                        .as_ref()
                        .map(|s| s.session_id.as_str())
                        .unwrap_or_default(),
                    viewers.len()
                );
                self.reconcile_host_viewers(&viewers).await;
                self.apply_host_link_actions(actions).await;
            }
            Err(e) => {
                log::warn!("Stream host rejoin failed: {}; the next attempt follows", e);
                state.link.on_attempt_failed(now);
            }
        }
    }

    /// A viewer that left while the host was away sent its `member_left`
    /// to the old connection: take it out of the count now.
    pub(super) async fn reconcile_host_viewers(&mut self, viewers: &[String]) {
        let Some(sink) = self.stream_host_sfu_sink.clone() else {
            return;
        };
        for viewer in sink.viewer_ids() {
            if !viewers.contains(&viewer) {
                use crate::stream::sink::PacketSink as _;
                sink.on_viewer_left(&viewer).await;
                self.report_stream_viewer_left(&viewer).await;
            }
        }
    }

    // --- stage 9 (streaming-quality): the hosted card's viewer count ---

    /// Tell the UI which viewers joined and left since the last tick. The
    /// sink's viewer set is the source: it takes `member_joined` and
    /// `member_left` once each (`sfu_membership_dedup_source_gate`), and a
    /// rejoin's reconcile takes a viewer that left while the host was away
    /// out of it. One tick after the change, before the next membership poll.
    pub(super) fn report_host_viewer_changes(&mut self) {
        let Some(sink) = self.stream_host_sfu_sink.clone() else {
            return;
        };
        let Some(state) = self.host_link.as_mut() else {
            return;
        };
        for event in host_viewer_events(&mut state.reported_viewers, sink.viewer_ids()) {
            let _ = self.event_tx.send(event);
        }
    }

    // --- end stage 9 ---

    /// Refresh the host's SFU token in the background every 4 minutes, so
    /// a rejoin after a drop needs no backend round trip.
    fn refresh_host_token_if_due(&mut self) {
        let now = tokio::time::Instant::now();
        let tx = self.stream_tasks.sender();
        let Some(state) = self.host_link.as_mut() else {
            return;
        };
        if state.token_task.is_some()
            || !state.grant.needs_refresh(now)
            || state
                .token_tried_at
                .is_some_and(|t| now.duration_since(t) < Duration::from_secs(60))
        {
            return;
        }
        state.token_tried_at = Some(now);
        let payload = serde_json::json!({ "session_id": state.session_id });
        let rpc = match self.nakama.rpc_detached("refresh_stream_token", &payload) {
            Ok(rpc) => rpc,
            Err(e) => {
                log::debug!("Stream host token refresh not sent: {}", e);
                return;
            }
        };
        let generation = state.generation;
        state.token_task = Some(tokio::spawn(async move {
            let result = refresh_grant(rpc).await;
            let _ = tx.send(StreamTaskMsg::Host(HostMsg::Token { generation, result }));
        }));
    }
}

/// Stage 9: the join and leave events that bring `reported` to `now`, in
/// id order; `reported` is `now` after.
fn host_viewer_events(
    reported: &mut std::collections::BTreeSet<String>,
    now: Vec<String>,
) -> Vec<Event> {
    let now: std::collections::BTreeSet<String> = now.into_iter().collect();
    let mut events = Vec::new();
    for viewer_id in reported.difference(&now) {
        log::info!("Stream host: viewer {viewer_id} left the count");
        events.push(Event::StreamViewerLeft {
            viewer_id: viewer_id.clone(),
        });
    }
    for viewer_id in now.difference(reported) {
        log::info!("Stream host: viewer {viewer_id} joined the count");
        events.push(Event::StreamViewerJoined {
            viewer_id: viewer_id.clone(),
        });
    }
    *reported = now;
    events
}

#[cfg(test)]
mod tests {
    use super::start_failure_reason;

    #[test]
    fn a_failed_start_names_the_cause() {
        assert_eq!(
            start_failure_reason(
                "rpc",
                r#"start_stream: {"error":"SFU_UNAVAILABLE","code":14}"#
            ),
            "Streaming is not available on the server right now."
        );
        assert_eq!(
            start_failure_reason("rpc", "start_stream took longer than 10s"),
            "The server did not answer the stream start."
        );
        assert_eq!(
            start_failure_reason("sfu", "connect took longer than 5s"),
            "Could not reach the stream server."
        );
        let native = "Streaming requires a hardware encoder (NVIDIA, AMD, or Intel).";
        assert_eq!(start_failure_reason("native", native), native);
    }
}
