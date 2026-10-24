//! Watch a stream off the command loop, and keep the viewer on the SFU
//! (streaming-quality stage 6).
//!
//! | Step | Runs on | Limit | Work |
//! |---|---|---|---|
//! | Decoder | blocking thread | | `mello_stream_start_viewer` at the size from crew state (`stream_live` writes the real encode size); the decoder follows the SPS anyway |
//! | RPC | task | 10 s | `watch_stream`: SFU endpoint and viewer token |
//! | SFU | task | 5 s connect, 10 s join | WebSocket, viewer peer, `join_stream`, track and control channel open |
//!
//! The decoder starts at the click, at the same time as the RPC and the SFU
//! join, so its init (and the presenter the UI builds on `StreamWatching`)
//! overlaps ICE and DTLS. When both ended, libmello takes the video from
//! the peer (`mello_stream_viewer_attach_peer`); the SFU replays its cached
//! IDR when the viewer leg connects.
//!
//! After the join, [`ViewerLinkState`] keeps the viewer on the stream. A
//! silent own link or a silent host shows "reconnecting" over the last
//! frame; `host_reconnecting` from the SFU does the same and keeps the leg
//! (stage 5 holds the session 30 s). A lost connection, ICE failure or
//! `PEER_SLOW` rejoins the same session with a new connection; the decoder
//! and the presenter stay, so the last frame stays on screen, and libmello
//! takes the video from the new peer. `session_ended` or 30 s without a
//! frame ends the watch with a message.

use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

use crate::events::{Event, StreamRole};
use crate::nakama::WatchStreamResponse;
use crate::stream::config::Codec;
use crate::stream::congestion::ViewerCongestionController;
use crate::stream::StreamMembers;
use crate::transport::sfu_connection::{MemberInfo, SfuEvent, StreamPeerRole};
use crate::transport::SfuConnection;

use super::stream_ffi::{
    attach_viewer_peer, on_viewer_native_frame, register_pause_callback, unregister_pause_callback,
    FrameCallbackData, PauseSlot, ViewerState,
};
use super::stream_host_start::refresh_grant;
use super::stream_link::{LinkAction, LinkObservation, StreamLink, LINK_PING_EVERY_TICKS};
use super::streaming::{
    join_stream_session, leave_sfu_bounded, RpcFuture, StreamGrant, StreamTaskMsg,
};
use super::FRAME_STATE_PRESENTED;

/// The longest the `watch_stream` RPC may take.
pub(super) const WATCH_RPC_LIMIT: Duration = Duration::from_secs(10);
/// The viewer reads its receive stats for the host-silence check every 4
/// stream ticks (64 ms).
const HOST_MEDIA_CHECK_TICKS: u64 = 4;

/// A step of a watch or of the viewer link that ended.
pub(super) enum ViewerMsg {
    Decoder {
        generation: u64,
        /// The viewer handle and the frame callback data, as addresses.
        result: Result<(usize, usize), String>,
    },
    Sfu {
        generation: u64,
        result: Result<(SfuConnection, Vec<MemberInfo>, WatchStreamResponse), String>,
    },
    Rejoin {
        generation: u64,
        result: Result<(SfuConnection, Option<StreamGrant>), String>,
    },
    Token {
        generation: u64,
        result: Result<StreamGrant, String>,
    },
}

/// A watch in flight. Owned by the command loop.
pub(super) struct WatchJoin {
    pub generation: u64,
    pub host_id: String,
    pub session_id: String,
    pub clicked_at: Instant,
    pub size: (u32, u32),
    pub task: JoinHandle<()>,
    pub delivered_frames: Arc<std::sync::atomic::AtomicU64>,
    /// The started decoder and its callback data, as addresses.
    pub decoder: Option<(usize, usize)>,
    pub decoder_ms: Option<u128>,
    pub sfu: Option<(SfuConnection, Vec<MemberInfo>, WatchStreamResponse)>,
    pub sfu_ms: Option<u128>,
}

/// The viewer's link to the SFU after the join.
pub(super) struct ViewerLinkState {
    pub generation: u64,
    pub link: StreamLink,
    pub grant: StreamGrant,
    pub session_id: String,
    pub connected_at: Instant,
    pub attempt: Option<JoinHandle<()>>,
    pub token_task: Option<JoinHandle<()>>,
    pub token_tried_at: Option<tokio::time::Instant>,
    pub ticks: u64,
    /// Host media: the receive byte count and when it last grew.
    pub rx_bytes: u64,
    pub rx_changed_at: Instant,
    /// Logged once, at the first frame.
    pub clicked_at: Option<Instant>,
}

impl Drop for ViewerLinkState {
    fn drop(&mut self) {
        if let Some(t) = self.attempt.take() {
            t.abort();
        }
        if let Some(t) = self.token_task.take() {
            t.abort();
        }
    }
}

/// Stop a started decoder and free its callback data, on the teardown
/// thread (the stop joins the decode and present threads).
fn stop_decoder(viewer: usize, frame_cb: usize) {
    use crate::stream::teardown::TeardownPtr;
    let viewer = TeardownPtr(viewer as *mut mello_sys::MelloStreamView);
    let frame_cb = TeardownPtr(frame_cb as *mut FrameCallbackData);
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
}

/// What a viewer does with one SFU event of its own connection.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ViewerSfuStep {
    /// The stream ended for good (`session_ended`): stop watching.
    End(&'static str),
    /// The connection is gone: rejoin the same session.
    Rejoin(String),
    /// The host dropped; the SFU holds the session and the leg.
    HostReconnecting,
    Continue,
}

/// The viewer's step for `event`. Before stage 6 the viewer stopped on
/// every disconnect and on `host_reconnecting`.
pub(super) fn viewer_sfu_step(event: &SfuEvent) -> ViewerSfuStep {
    match event {
        SfuEvent::Disconnected { reason } if reason == "session_ended" => {
            ViewerSfuStep::End("host_left")
        }
        SfuEvent::Disconnected { reason } => ViewerSfuStep::Rejoin(reason.clone()),
        SfuEvent::HostReconnecting { .. } => ViewerSfuStep::HostReconnecting,
        _ => ViewerSfuStep::Continue,
    }
}

impl super::Client {
    /// WatchStream: start the decoder and the join at the same time, off the
    /// loop. Returns at once.
    pub(super) async fn handle_watch_stream(
        &mut self,
        host_id: &str,
        session_id: &str,
        stream_width: u32,
        stream_height: u32,
    ) {
        if self.viewer_state.is_some() || self.watch_join.is_some() {
            log::warn!("Already watching a stream, ignoring WatchStream");
            return;
        }
        let clicked_at = Instant::now();
        log::info!("Starting stream viewer for host {}", host_id);
        let ctx = self.voice.mello_ctx();
        if ctx.is_null() {
            let _ = self.event_tx.send(Event::StreamFailed {
                role: StreamRole::Viewer,
                reason: "The media engine is not running.".to_string(),
            });
            return;
        }
        // A stream without a session cannot be joined on the SFU.
        if session_id.is_empty() {
            log::error!("WatchStream for host {} has no session_id", host_id);
            let _ = self.event_tx.send(Event::StreamFailed {
                role: StreamRole::Viewer,
                reason: "The stream has no session to join.".to_string(),
            });
            return;
        }
        let rpc = match self.nakama.rpc_detached(
            "watch_stream",
            &serde_json::json!({ "session_id": session_id }),
        ) {
            Ok(rpc) => rpc,
            Err(e) => {
                log::error!("watch_stream RPC failed: {}", e);
                let _ = self.event_tx.send(Event::StreamFailed {
                    role: StreamRole::Viewer,
                    reason: "You are not signed in.".to_string(),
                });
                return;
            }
        };

        // The size from crew state; `stream_live` writes the real encode
        // size there. The decoder and the presenter follow the SPS.
        let (w, h) = if stream_width > 0 && stream_height > 0 {
            (stream_width, stream_height)
        } else {
            let config = crate::stream::StreamConfig::default();
            log::warn!(
                "Stream viewer: no size in crew state, the decoder starts at {}x{}",
                config.width,
                config.height
            );
            (config.width, config.height)
        };

        let delivered_frames = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let frame_cb_data = Box::into_raw(Box::new(FrameCallbackData {
            frame_slot: self.frame_slot.clone(),
            native_frame_slot: self.native_frame_slot.clone(),
            frame_consumed: self.frame_consumed.clone(),
            frame_lifecycle: self.frame_lifecycle.clone(),
            surface_frame_seq: self.surface_frame_seq.clone(),
            delivered_frames: Arc::clone(&delivered_frames),
        }));
        self.frame_lifecycle
            .store(FRAME_STATE_PRESENTED, Ordering::Release);

        self.stream_generation = self.stream_generation.wrapping_add(1);
        let generation = self.stream_generation;
        let tx = self.stream_tasks.sender();

        // The decoder on a blocking thread: it creates a device and a
        // decoder session.
        let ctx_addr = ctx as usize;
        let cb_addr = frame_cb_data as usize;
        let decoder_tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            let config = mello_sys::MelloStreamConfig {
                width: w,
                height: h,
                fps: crate::stream::StreamConfig::default().fps,
                bitrate_kbps: 0,
            };
            #[cfg(target_os = "windows")]
            let frame_callback = None;
            #[cfg(not(target_os = "windows"))]
            let frame_callback = Some(super::stream_ffi::on_viewer_frame as _);
            // SAFETY: the context lives as long as the client; the callback
            // data lives until the viewer stopped (stop_decoder, or the
            // ViewerState drop).
            let viewer = unsafe {
                mello_sys::mello_stream_start_viewer(
                    ctx_addr as *mut mello_sys::MelloContext,
                    &config,
                    frame_callback,
                    cb_addr as *mut std::ffi::c_void,
                )
            };
            let result = if viewer.is_null() {
                Err(format!("Failed to start the video decoder at {w}x{h}"))
            } else {
                // SAFETY: viewer was just started.
                unsafe {
                    mello_sys::mello_stream_set_native_frame_callback(
                        viewer,
                        Some(on_viewer_native_frame as _),
                        cb_addr as *mut std::ffi::c_void,
                    );
                }
                Ok((viewer as usize, cb_addr))
            };
            let _ = decoder_tx.send(StreamTaskMsg::Viewer(ViewerMsg::Decoder {
                generation,
                result,
            }));
        });

        let limits = self.stream_join_limits;
        let ice = self.ice_servers.clone();
        let session = session_id.to_string();
        let task = tokio::spawn(async move {
            let result = async {
                let body = match tokio::time::timeout(WATCH_RPC_LIMIT, rpc).await {
                    Ok(Ok(body)) => body,
                    Ok(Err(e)) => return Err(format!("watch_stream: {e}")),
                    Err(_) => {
                        return Err(format!(
                            "watch_stream took longer than {:?}",
                            WATCH_RPC_LIMIT
                        ))
                    }
                };
                let resp: WatchStreamResponse =
                    serde_json::from_str(&body).map_err(|e| format!("watch_stream answer: {e}"))?;
                let (conn, members) = join_stream_session(
                    resp.sfu_endpoint.clone().unwrap_or_default(),
                    resp.sfu_token.clone().unwrap_or_default(),
                    session,
                    StreamPeerRole::Viewer,
                    ctx_addr,
                    ice,
                    limits,
                )
                .await?;
                Ok((conn, members, resp))
            }
            .await;
            let _ = tx.send(StreamTaskMsg::Viewer(ViewerMsg::Sfu { generation, result }));
        });

        self.watch_join = Some(WatchJoin {
            generation,
            host_id: host_id.to_string(),
            session_id: session_id.to_string(),
            clicked_at,
            size: (w, h),
            task,
            delivered_frames,
            decoder: None,
            decoder_ms: None,
            sfu: None,
            sfu_ms: None,
        });
    }

    /// A step of a watch or of the viewer link ended.
    pub(super) async fn on_viewer_msg(&mut self, msg: ViewerMsg) {
        match msg {
            ViewerMsg::Decoder { generation, result } => {
                let Some(join) = self
                    .watch_join
                    .as_mut()
                    .filter(|j| j.generation == generation)
                else {
                    // A cancelled watch: stop the decoder it started.
                    // Without a viewer the callback data was never used.
                    if let Ok((viewer, cb)) = result {
                        stop_decoder(viewer, cb);
                    }
                    return;
                };
                match result {
                    Ok(decoder) => {
                        let ms = join.clicked_at.elapsed().as_millis();
                        join.decoder_ms = Some(ms);
                        join.decoder = Some(decoder);
                        log::info!(
                            "Stream watch: decoder ready {} ms after the click at {}x{}",
                            ms,
                            join.size.0,
                            join.size.1
                        );
                        // The UI builds its presenter now, while ICE runs.
                        let _ = self.event_tx.send(Event::StreamWatching {
                            host_id: join.host_id.clone(),
                            width: join.size.0,
                            height: join.size.1,
                        });
                        self.finish_watch_if_ready();
                    }
                    Err(e) => self.fail_watch(&e, "The video decoder did not start."),
                }
            }
            ViewerMsg::Sfu { generation, result } => {
                let Some(join) = self
                    .watch_join
                    .as_mut()
                    .filter(|j| j.generation == generation)
                else {
                    if let Ok((conn, _, _)) = result {
                        let conn = Arc::new(conn);
                        tokio::spawn(async move { leave_sfu_bounded(&conn, "viewer").await });
                    }
                    return;
                };
                match result {
                    Ok(joined) => {
                        let ms = join.clicked_at.elapsed().as_millis();
                        join.sfu_ms = Some(ms);
                        log::info!("Stream watch: SFU track open {} ms after the click", ms);
                        join.sfu = Some(joined);
                        self.finish_watch_if_ready();
                    }
                    Err(e) => {
                        let reason = if e.starts_with("watch_stream") {
                            "The stream is not available."
                        } else {
                            "Could not reach the stream server."
                        };
                        self.fail_watch(&e, reason);
                    }
                }
            }
            ViewerMsg::Rejoin { generation, result } => {
                self.on_viewer_rejoin(generation, result);
            }
            ViewerMsg::Token { generation, result } => {
                let Some(state) = self
                    .viewer_link
                    .as_mut()
                    .filter(|s| s.generation == generation)
                else {
                    return;
                };
                state.token_task = None;
                match result {
                    Ok(grant) => {
                        log::info!("Stream viewer token refreshed");
                        state.grant = grant;
                    }
                    Err(e) => log::warn!("Stream viewer token refresh failed: {}", e),
                }
            }
        }
    }

    /// End a watch in flight with `reason` for the user.
    fn fail_watch(&mut self, detail: &str, reason: &str) {
        let Some(join) = self.watch_join.take() else {
            return;
        };
        log::error!(
            "Stream watch failed after {} ms: {}",
            join.clicked_at.elapsed().as_millis(),
            detail
        );
        let shown = join.decoder.is_some();
        self.cancel_watch(join);
        let _ = self.event_tx.send(Event::StreamFailed {
            role: StreamRole::Viewer,
            reason: reason.to_string(),
        });
        if shown {
            let _ = self.event_tx.send(Event::StreamWatchingStopped);
        }
    }

    /// Stop a watch in flight: its task, its decoder (or the decoder still
    /// starting: its report finds no watch and stops it), its connection.
    pub(super) fn cancel_watch(&mut self, mut join: WatchJoin) {
        join.task.abort();
        if let Some((viewer, cb)) = join.decoder.take() {
            stop_decoder(viewer, cb);
        }
        if let Some((conn, _, _)) = join.sfu.take() {
            let conn = Arc::new(conn);
            tokio::spawn(async move { leave_sfu_bounded(&conn, "viewer").await });
        }
        self.frame_lifecycle
            .store(FRAME_STATE_PRESENTED, Ordering::Release);
    }

    /// The decoder and the SFU leg are both up: libmello takes the video
    /// from the peer.
    fn finish_watch_if_ready(&mut self) {
        let ready = self
            .watch_join
            .as_ref()
            .is_some_and(|j| j.decoder.is_some() && j.sfu.is_some());
        if !ready {
            return;
        }
        let Some(mut join) = self.watch_join.take() else {
            return;
        };
        let (Some((viewer_addr, cb_addr)), Some((conn, members, resp))) =
            (join.decoder.take(), join.sfu.take())
        else {
            return;
        };
        let viewer = viewer_addr as *mut mello_sys::MelloStreamView;
        let conn = Arc::new(conn);
        let members = StreamMembers::from_joined(&members);

        // Pause state arrives on the reliable control channel.
        let pause_slot = Arc::new(PauseSlot::new());
        let mut pause_cb_data: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut pause_cb_peer: *mut mello_sys::MelloPeerConnection = std::ptr::null_mut();
        match conn.peer_nonnull() {
            Ok(peer) => {
                pause_cb_peer = peer.as_ptr();
                // SAFETY: the peer lives as long as `conn`, which the
                // ViewerState keeps past the unregister in its drop.
                pause_cb_data = unsafe { register_pause_callback(peer, &pause_slot) };
                // SAFETY: `viewer` is live; libmello holds the peer weakly.
                if !unsafe { attach_viewer_peer(viewer, peer) } {
                    log::error!("SFU viewer: libmello refused the viewer peer");
                }
            }
            Err(e) => log::error!("SFU viewer: no peer for the viewer pipeline: {}", e),
        }

        log::info!(
            "Stream watch: viewer attached {} ms after the click (decoder {} ms, SFU {} ms)",
            join.clicked_at.elapsed().as_millis(),
            join.decoder_ms.unwrap_or_default(),
            join.sfu_ms.unwrap_or_default()
        );
        log::info!(
            "Stream members at join: {} (host in session: {})",
            members.members().len(),
            members.host().is_some()
        );
        let _ = self.event_tx.send(Event::StreamMembersChanged {
            members: members.members().to_vec(),
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
        let now = Instant::now();
        self.viewer_link = Some(ViewerLinkState {
            generation: join.generation,
            link: StreamLink::new(StreamRole::Viewer),
            grant: StreamGrant::new(
                resp.sfu_endpoint.clone().unwrap_or_default(),
                resp.sfu_token.clone().unwrap_or_default(),
            ),
            session_id: join.session_id.clone(),
            connected_at: now,
            attempt: None,
            token_task: None,
            token_tried_at: None,
            ticks: 0,
            rx_bytes: 0,
            rx_changed_at: now,
            clicked_at: Some(join.clicked_at),
        });
        self.viewer_state = Some(ViewerState {
            viewer: Some(viewer),
            sfu_connection: Some(conn),
            host_id: join.host_id.clone(),
            members,
            _frame_cb_data: cb_addr as *mut FrameCallbackData,
            pause_slot,
            last_reported_paused: false,
            last_reported_pause_reason: String::new(),
            _pause_cb_data: pause_cb_data,
            pause_cb_peer,
            frames_presented: 0,
            stream_tick_count: 0,
            congestion: ViewerCongestionController::new(receive_bitrate_kbps, Codec::H264),
            debug_last_emit: now,
            debug_last_tick_count: 0,
            debug_last_bytes: 0,
            debug_last_frames_presented: 0,
            debug_last_frames_decoded: 0,
            audio_packets_received: 0,
            last_new_frame_at: now,
            freeze_count: 0,
            freeze_ms_total: 0,
            in_freeze: false,
            freeze_accounted_ms: 0,
            stats_last_emit: now,
            stats_last_frames_presented: 0,
            stats_last_freeze_count: 0,
            stats_last_freeze_ms: 0,
            stats_last_frames_decoded: 0,
            delivered_frames: join.delivered_frames,
            last_delivered_frames: 0,
            audio_muted: false,
        });
    }

    /// The viewer's link on one stream tick, after the frame count: ping,
    /// watch the own link and the host's media, rejoin on the schedule.
    /// `new_frames` are the frames shown since the last tick; `held` is
    /// true while the host is paused or idle.
    pub(super) async fn viewer_link_tick(&mut self, new_frames: u64, held: bool) {
        let now = Instant::now();
        let Some(conn) = self
            .viewer_state
            .as_ref()
            .and_then(|vs| vs.sfu_connection.clone())
        else {
            return;
        };
        let frames_presented = self
            .viewer_state
            .as_ref()
            .map_or(0, |vs| vs.frames_presented);
        let Some(state) = self.viewer_link.as_mut() else {
            return;
        };
        state.ticks = state.ticks.wrapping_add(1);
        if state.ticks.is_multiple_of(LINK_PING_EVERY_TICKS) {
            conn.send_ping();
        }
        if new_frames > 0 {
            if let Some(clicked) = state.clicked_at.take() {
                log::info!(
                    "Stream watch timing: click_to_first_frame_ms={}",
                    clicked.elapsed().as_millis()
                );
            }
        }
        if state.ticks.is_multiple_of(HOST_MEDIA_CHECK_TICKS) {
            if let Ok(stats) = conn.video_stats() {
                if stats.rx_ingress_bytes != state.rx_bytes {
                    state.rx_bytes = stats.rx_ingress_bytes;
                    state.rx_changed_at = now;
                }
            }
        }
        if new_frames > 0 || held {
            // Frames came, or the host stopped on purpose: not a silence.
            state.rx_changed_at = now;
        }

        let mut actions = Vec::new();
        if conn.is_ice_dead() && !state.link.is_rebuilding() {
            log::warn!("Stream viewer: ICE failed; rejoining the SFU");
            actions.extend(state.link.on_dropped(now));
        }
        let pong_age = conn.pong_age_ms();
        let own_silent = if pong_age < 0 {
            now.saturating_duration_since(state.connected_at)
        } else {
            Duration::from_millis(pong_age as u64)
        };
        let host_silent = (frames_presented > 0 && !held)
            .then(|| now.saturating_duration_since(state.rx_changed_at));
        actions.extend(state.link.observe(
            now,
            LinkObservation {
                own_silent,
                host_silent,
                new_frame: new_frames > 0,
            },
        ));
        self.apply_viewer_link_actions(actions).await;
        self.refresh_viewer_token_if_due();
    }

    /// One SFU event of the viewer's connection that is not media or
    /// membership. Returns false when the watch ended.
    pub(super) async fn on_viewer_sfu_event(&mut self, event: &SfuEvent) -> bool {
        match viewer_sfu_step(event) {
            ViewerSfuStep::Continue => true,
            ViewerSfuStep::End(reason) => {
                log::info!("Stream SFU session ended: the host left");
                self.end_watching(reason).await;
                false
            }
            ViewerSfuStep::Rejoin(why) => {
                log::warn!(
                    "Stream SFU viewer disconnected: {}; rejoining the same session",
                    why
                );
                if let Some(state) = self.viewer_link.as_mut() {
                    let actions = state.link.on_dropped(Instant::now());
                    self.apply_viewer_link_actions(actions).await;
                }
                self.viewer_state.is_some()
            }
            ViewerSfuStep::HostReconnecting => {
                log::info!("Stream host reconnecting; the viewer keeps its leg and the last frame");
                if let Some(state) = self.viewer_link.as_mut() {
                    let actions = state.link.on_host_reconnecting(Instant::now());
                    self.apply_viewer_link_actions(actions).await;
                }
                true
            }
        }
    }

    /// Wake from sleep: the viewer's connection is dead; rejoin at once.
    pub(super) async fn on_viewer_wake(&mut self) {
        if let Some(state) = self.viewer_link.as_mut() {
            log::warn!("Stream viewer: wake from sleep; rejoining the same session");
            let actions = state.link.on_dropped(Instant::now());
            self.apply_viewer_link_actions(actions).await;
        }
    }

    async fn apply_viewer_link_actions(&mut self, actions: Vec<LinkAction>) {
        for action in actions {
            match action {
                LinkAction::ShowReconnecting => {
                    log::warn!("Stream viewer: reconnecting; the last frame stays");
                    let _ = self.event_tx.send(Event::StreamReconnecting {
                        role: StreamRole::Viewer,
                    });
                }
                LinkAction::ShowResumed => {
                    log::info!("Stream viewer: frames again");
                    let _ = self.event_tx.send(Event::StreamResumed {
                        role: StreamRole::Viewer,
                    });
                }
                LinkAction::Rejoin => self.start_viewer_rejoin(),
                LinkAction::GiveUp => {
                    log::error!(
                        "Stream viewer: no frame within {} s; leaving the stream",
                        super::voice::SFU_RECONNECT_GIVE_UP_AFTER.as_secs()
                    );
                    self.end_watching("connection_lost").await;
                    return;
                }
            }
        }
    }

    fn start_viewer_rejoin(&mut self) {
        let ctx = self.voice.mello_ctx() as usize;
        let ice = self.ice_servers.clone();
        let limits = self.stream_join_limits;
        let tx = self.stream_tasks.sender();
        let now = tokio::time::Instant::now();
        let Some(state) = self.viewer_link.as_mut() else {
            return;
        };
        let refresh: Option<RpcFuture> = if state.grant.is_fresh(now) {
            None
        } else {
            let payload = serde_json::json!({ "session_id": state.session_id });
            match self.nakama.rpc_detached("refresh_stream_token", &payload) {
                Ok(rpc) => Some(Box::pin(rpc)),
                Err(e) => {
                    log::warn!(
                        "Stream viewer rejoin: no token refresh ({}); the old token goes",
                        e
                    );
                    None
                }
            }
        };
        log::info!(
            "Stream viewer rejoin attempt {} for session {}",
            state.link.attempt_number(),
            state.session_id
        );
        let generation = state.generation;
        let grant = state.grant.clone();
        let session_id = state.session_id.clone();
        state.attempt = Some(tokio::spawn(async move {
            let result = async {
                let (grant, fresh) = match refresh {
                    Some(rpc) => {
                        let new = refresh_grant(rpc).await?;
                        (new.clone(), Some(new))
                    }
                    None => (grant, None),
                };
                let (conn, _members) = join_stream_session(
                    grant.endpoint.clone(),
                    grant.token.clone(),
                    session_id,
                    StreamPeerRole::Viewer,
                    ctx,
                    ice,
                    limits,
                )
                .await?;
                Ok::<_, String>((conn, fresh))
            }
            .await;
            let _ = tx.send(StreamTaskMsg::Viewer(ViewerMsg::Rejoin {
                generation,
                result,
            }));
        }));
    }

    fn on_viewer_rejoin(
        &mut self,
        generation: u64,
        result: Result<(SfuConnection, Option<StreamGrant>), String>,
    ) {
        let now = Instant::now();
        let Some(state) = self
            .viewer_link
            .as_mut()
            .filter(|s| s.generation == generation)
        else {
            if let Ok((conn, _)) = result {
                let conn = Arc::new(conn);
                tokio::spawn(async move { leave_sfu_bounded(&conn, "viewer").await });
            }
            return;
        };
        state.attempt = None;
        let (conn, grant) = match result {
            Ok(v) => v,
            Err(e) => {
                log::warn!(
                    "Stream viewer rejoin failed: {}; the next attempt follows",
                    e
                );
                state.link.on_attempt_failed(now);
                return;
            }
        };
        if let Some(grant) = grant {
            state.grant = grant;
        }
        state.connected_at = now;
        state.rx_changed_at = now;
        let actions = state.link.on_rejoined(now);
        let Some(vs) = self.viewer_state.as_mut() else {
            return;
        };
        let conn = Arc::new(conn);
        // SAFETY: the old connection is still in `vs`, so its peer is alive
        // for the unregister; the viewer is live for the attach.
        unsafe { move_viewer_to_connection(vs, &conn) };
        let old = vs.sfu_connection.replace(conn);
        // No leave on the old connection: the SFU replaced this user's old
        // viewer peer on the rejoin, and a leave would count the viewer out.
        drop(old);
        log::info!("Stream viewer rejoined the session; the decoder takes the new leg");
        for action in actions {
            if action == LinkAction::ShowReconnecting {
                let _ = self.event_tx.send(Event::StreamReconnecting {
                    role: StreamRole::Viewer,
                });
            }
        }
    }

    fn refresh_viewer_token_if_due(&mut self) {
        let now = tokio::time::Instant::now();
        let tx = self.stream_tasks.sender();
        let Some(state) = self.viewer_link.as_mut() else {
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
                log::debug!("Stream viewer token refresh not sent: {}", e);
                return;
            }
        };
        let generation = state.generation;
        state.token_task = Some(tokio::spawn(async move {
            let result = refresh_grant(rpc).await;
            let _ = tx.send(StreamTaskMsg::Viewer(ViewerMsg::Token {
                generation,
                result,
            }));
        }));
    }

    /// The watched stream ended (`reason`: "host_left", "connection_lost").
    /// The card says why, then the viewer stops as on Leave.
    pub(super) async fn end_watching(&mut self, reason: &str) {
        let _ = self.event_tx.send(Event::StreamEnded {
            crew_id: String::new(),
            role: StreamRole::Viewer,
            reason: reason.to_string(),
        });
        self.viewer_link = None;
        if let Some(vs) = self.viewer_state.take() {
            let _ = self.event_tx.send(Event::StreamWatchingStopped);
            // The session is gone or unreachable: no leave.
            drop(vs);
            self.frame_lifecycle
                .store(FRAME_STATE_PRESENTED, Ordering::Release);
        }
    }

    pub(super) async fn handle_stop_watching(&mut self) {
        if let Some(join) = self.watch_join.take() {
            log::info!(
                "Stopping a stream watch in flight for host {}",
                join.host_id
            );
            let shown = join.decoder.is_some();
            self.cancel_watch(join);
            if shown {
                let _ = self.event_tx.send(Event::StreamWatchingStopped);
            }
            return;
        }
        self.viewer_link = None;
        if let Some(vs) = self.viewer_state.take() {
            log::info!("Stopping stream viewer for host {}", vs.host_id);
            // UI first; the SFU leave and the native teardown below are
            // bounded or off-loop and must not delay it.
            let _ = self.event_tx.send(Event::StreamWatchingStopped);
            if let Some(conn) = vs.sfu_connection.clone() {
                // The leave runs on its own task: a stalled socket must not
                // hold the loop.
                tokio::spawn(async move { leave_sfu_bounded(&conn, "viewer").await });
            }
            // Native stop runs on the teardown thread (ViewerState::drop).
            drop(vs);
            self.frame_lifecycle
                .store(FRAME_STATE_PRESENTED, Ordering::Release);
        }
    }
}

/// Move the viewer's pause callback and libmello's video source to `conn`.
///
/// # Safety
/// `vs.sfu_connection` (the old connection) must still hold its peer, and
/// `vs.viewer` must be live.
unsafe fn move_viewer_to_connection(vs: &mut ViewerState, conn: &Arc<SfuConnection>) {
    if !vs._pause_cb_data.is_null() {
        match NonNull::new(vs.pause_cb_peer) {
            Some(peer) => unregister_pause_callback(peer, vs._pause_cb_data),
            None => drop(Arc::from_raw(vs._pause_cb_data as *const PauseSlot)),
        }
        vs._pause_cb_data = std::ptr::null_mut();
        vs.pause_cb_peer = std::ptr::null_mut();
    }
    match conn.peer_nonnull() {
        Ok(peer) => {
            vs.pause_cb_peer = peer.as_ptr();
            vs._pause_cb_data = register_pause_callback(peer, &vs.pause_slot);
            if let Some(viewer) = vs.viewer {
                if !attach_viewer_peer(viewer, peer) {
                    log::error!("SFU viewer: libmello refused the new viewer peer");
                }
            }
        }
        Err(e) => log::error!("SFU viewer rejoin: no peer on the new connection: {}", e),
    }
}

#[cfg(test)]
mod tests {
    use super::{viewer_sfu_step, ViewerSfuStep};
    use crate::transport::sfu_connection::SfuEvent;

    /// Stage 6: only `session_ended` ends a watch. Before, every
    /// disconnect and `host_reconnecting` stopped the viewer.
    #[test]
    fn only_session_ended_ends_a_watch() {
        assert_eq!(
            viewer_sfu_step(&SfuEvent::Disconnected {
                reason: "session_ended".into()
            }),
            ViewerSfuStep::End("host_left")
        );
        assert_eq!(
            viewer_sfu_step(&SfuEvent::Disconnected {
                reason: "ws_closed".into()
            }),
            ViewerSfuStep::Rejoin("ws_closed".into())
        );
        assert_eq!(
            viewer_sfu_step(&SfuEvent::Disconnected {
                reason: "error:PEER_SLOW".into()
            }),
            ViewerSfuStep::Rejoin("error:PEER_SLOW".into())
        );
        assert_eq!(
            viewer_sfu_step(&SfuEvent::HostReconnecting {
                user_id: "host-1".into(),
                hold_ms: 30_000
            }),
            ViewerSfuStep::HostReconnecting
        );
        assert_eq!(
            viewer_sfu_step(&SfuEvent::HostReconnected {
                user_id: "host-1".into()
            }),
            ViewerSfuStep::Continue
        );
    }
}
