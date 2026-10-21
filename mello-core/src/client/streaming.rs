use std::sync::Arc;
use std::time::Instant;

use crate::events::Event;
use crate::stream::config::Codec;
use crate::stream::congestion::ViewerCongestionController;
use crate::stream::StreamMembers;
use crate::transport::sfu_connection::{SfuConnection, SfuEvent, StreamPeerRole};

#[cfg(not(target_os = "windows"))]
use super::stream_ffi::on_viewer_frame;

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
    feed_viewer_audio_rtp, log_viewer_native_stats, on_viewer_native_frame,
    poll_sfu_viewer_access_units, register_pause_callback, tick_viewer_congestion_sfu,
    FrameCallbackData, PauseSlot, StreamHostHandle, ViewerState,
};
use super::FRAME_STATE_PRESENTED;

const STREAM_DEBUG_EVENT_INTERVAL_SECS: f32 = 1.0;
/// Longest wait for the SFU leave message when a viewer stops watching.
const SFU_LEAVE_DEADLINE: std::time::Duration = std::time::Duration::from_millis(1500);
/// Viewer report cadence to the SFU. Above the server's 5s rate limit so a
/// message is never dropped for arriving too soon.
const VIEWER_STATS_INTERVAL_SECS: u64 = 10;
const HOST_PACING_DEBUG_EVENT_INTERVAL_SECS: f32 = 1.0;

// ---------------------------------------------------------------------------
// Async stream start: the native host starts off the command loop
// ---------------------------------------------------------------------------
//
// `mello_stream_start_host` blocks for about 2 s with the hook in the path
// (injection wait plus first-frame deadline). The command loop also drives
// the 20 ms voice tick and every other command, so awaiting it there stalls
// all three. The start therefore runs on its own OS thread; the loop polls
// the join handle on the stream tick and finishes the start when it lands.
//
// Stop-during-start is safe: dropping the join handle detaches the thread,
// and when it finishes its outcome drops, which stops the host through the
// bounded teardown guard. The advertised session is retracted either way.

/// What the background start thread hands back: everything the loop needs
/// to finish the start without touching native code again.
pub(super) struct NativeStartOutcome {
    host: StreamHostHandle,
    video_rx: tokio::sync::mpsc::Receiver<crate::stream::manager::VideoPacket>,
    audio_rx: tokio::sync::mpsc::Receiver<crate::stream::manager::AudioPacket>,
    teardown: crate::stream::teardown::NativeTeardownGuard,
    actual_w: u32,
    actual_h: u32,
}

/// A native start in flight. Owned by the command loop; polled on the
/// stream tick, never awaited.
pub(super) struct PendingStreamStart {
    crew_id: String,
    config: crate::stream::StreamConfig,
    configured_bitrate_kbps: u32,
    resp: crate::stream::host::StartStreamResponse,
    thread: std::thread::JoinHandle<Result<NativeStartOutcome, String>>,
}

/// The blocking half of stream start. Runs on its own thread: capture
/// backend init (including hook injection), resolution query, audio start.
/// Touches no `Client` state; every parameter is a plain value. The HWND
/// travels as an address because a raw pointer is not `Send`; the capture
/// source struct is rebuilt on the thread.
struct NativeStartParams {
    ctx: usize,
    mode: mello_sys::MelloCaptureMode,
    monitor_index: u32,
    hwnd: usize,
    pid: u32,
    allow_hook: bool,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
}

fn start_host_blocking(params: NativeStartParams) -> Result<NativeStartOutcome, String> {
    let source = mello_sys::MelloCaptureSource {
        mode: params.mode,
        monitor_index: params.monitor_index,
        hwnd: params.hwnd as *mut std::ffi::c_void,
        pid: params.pid,
        allow_hook: params.allow_hook,
    };
    let mello_config = mello_sys::MelloStreamConfig {
        width: params.width,
        height: params.height,
        fps: params.fps,
        bitrate_kbps: params.bitrate_kbps,
    };
    let ctx = params.ctx as *mut mello_sys::MelloContext;
    let (host, video_rx, audio_rx, teardown) =
        unsafe { crate::stream::host::start_host(ctx, &source, &mello_config) }
            .map_err(|e| e.to_string())?;
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

        // 0b. A native start in flight on its thread. Polled, never awaited:
        // the completion below only runs fast async steps from here.
        self.poll_pending_stream_start().await;

        self.poll_sfu_host_membership_events().await;
        self.emit_host_pacing_debug_stats().await;

        // Poll viewer for incoming stream packets
        if self.viewer_state.is_none() {
            return;
        }

        let vs = self.viewer_state.as_mut().unwrap();
        vs.stream_tick_count = vs.stream_tick_count.saturating_add(1);
        let viewer = match vs.viewer {
            Some(v) => v,
            None => return, // Decoder not yet initialized
        };

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
        if paused_now {
            vs.last_new_frame_at = Instant::now();
            vs.in_freeze = false;
            vs.freeze_accounted_ms = 0;
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
                    ev @ (crate::transport::SfuEvent::MemberJoined { .. }
                    | crate::transport::SfuEvent::MemberLeft { .. }) => {
                        if let Some(changed) = viewer_membership_event(&mut vs.members, &ev) {
                            let _ = self.event_tx.send(changed);
                        }
                    }
                    crate::transport::SfuEvent::ControlPacket { .. }
                    | crate::transport::SfuEvent::SlotReleased { .. } => {}
                    crate::transport::SfuEvent::AudioRtp { rtp, .. } => {
                        if let Some(viewer) = vs.viewer {
                            // SAFETY: `vs.viewer` is set only from a successful
                            // viewer start and cleared on stop, so it is valid
                            // for the lifetime of this ViewerState.
                            if unsafe { feed_viewer_audio_rtp(viewer, &rtp) } {
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
                mode: "sfu".to_string(),
                transport_packets: vs.transport_packets,
                transport_bytes: vs.transport_bytes,
                transport_truncations: vs.au_buffer_grows,
                frames_presented: vs.frames_presented,
                present_fps,
                ingress_kbps,
            });

            if let Some(conn) = vs.sfu_connection.clone() {
                if let Ok(stats) = conn.video_stats() {
                    log_viewer_native_stats(&stats);
                }
            }

            let rtt_ms = vs.sfu_connection.as_ref().map(|conn| conn.rtt_ms());
            log::info!(
                "Stream cadence: tick_hz={:.1} present_attempt_hz={:.1} present_fps={:.1} forced={} skipped_unconsumed={} backlog_guard_drops={} rtt_ms={:.0}",
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
    /// Freeze time leads the payload because it is the only field that
    /// corresponds to what the user actually sees — the rest explain *why* a
    /// freeze happened.
    async fn report_viewer_stream_stats(&mut self) {
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
                SfuEvent::AudioRtp { .. } | SfuEvent::SlotReleased { .. } => {}
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
                igdb_id: 0,
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
            let process_name = unsafe { std::ffi::CStr::from_ptr(game.name.as_ptr()) }
                .to_string_lossy()
                .to_string();
            let Some((is_known, source)) = game_capture_source(
                head,
                game.pid,
                exe,
                &path,
                title,
                process_name,
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
        if self.stream_session.is_some() || self.pending_stream_start.is_some() {
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
            exe,
            igdb_id,
            crate::transport::sfu_region::preferred_region(&self.sfu_regions),
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

        // Step 2: native start, off the command loop. `start_host_blocking`
        // holds the capture backend init (hook injection included, about 2 s)
        // on its own thread; the loop polls it on the stream tick. The guard
        // above ran before the RPC await, so a second start queued during it
        // reaches here advertised: take it back before reporting, or a viewer
        // presses "watch" on a stream that never started (2026-09-16).
        if self.stream_session.is_some() || self.pending_stream_start.is_some() {
            log::warn!("Stream start raced a live session; retracting the duplicate");
            self.retract_advertised_stream(crew_id).await;
            let _ = self.event_tx.send(Event::StreamError {
                message: "Already streaming".to_string(),
            });
            return;
        }
        let ctx = self.voice.mello_ctx();
        if !unsafe { crate::stream::encoder_available(ctx) } {
            let message = "Streaming requires a hardware encoder \
                           (NVIDIA, AMD, or Intel). None was found on this machine.";
            log::error!("{}", message);
            self.retract_advertised_stream(crew_id).await;
            let _ = self.event_tx.send(Event::StreamError {
                message: message.to_string(),
            });
            return;
        }

        let (mode, monitor_index, hwnd_addr, pid, allow_hook) = match target {
            crate::stream::config::CaptureTarget::Window { hwnd } => (
                mello_sys::MelloCaptureMode_MELLO_CAPTURE_WINDOW,
                0,
                hwnd as usize,
                0,
                false,
            ),
            crate::stream::config::CaptureTarget::Process { pid } => {
                // Gate 1 of the hook policy (plan §8): the backend
                // `capture` block carries the kill switch and the id
                // lists. No block, or no match, means no hook. Gates 2
                // (catalogue) and 3 (runtime checks) run in libmello.
                let allow_hook =
                    crate::stream::host::hook_allowed_for_game(igdb_id, resp.capture.as_ref());
                let version = resp
                    .capture
                    .as_ref()
                    .map(|c| c.policy_version.as_str())
                    .unwrap_or("none");
                log::info!(
                    "Hook policy: exe={:?} igdb_id={} allow_hook={} policy_version={}",
                    exe,
                    igdb_id,
                    allow_hook,
                    version
                );
                (
                    mello_sys::MelloCaptureMode_MELLO_CAPTURE_PROCESS,
                    0,
                    0,
                    pid,
                    allow_hook,
                )
            }
            crate::stream::config::CaptureTarget::Monitor { index } => (
                mello_sys::MelloCaptureMode_MELLO_CAPTURE_MONITOR,
                index,
                0,
                0,
                false,
            ),
        };

        // Pointers travel as addresses; the thread touches no Client state
        // and every parameter is Send, so the closure is Send.
        let params = NativeStartParams {
            ctx: ctx as usize,
            mode,
            monitor_index,
            hwnd: hwnd_addr,
            pid,
            allow_hook,
            width: config.width,
            height: config.height,
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
        };
        let thread = match std::thread::Builder::new()
            .name("stream_start".to_string())
            .spawn(move || start_host_blocking(params))
        {
            Ok(thread) => thread,
            Err(e) => {
                log::error!("Stream start thread failed to spawn: {}", e);
                self.retract_advertised_stream(crew_id).await;
                let _ = self.event_tx.send(Event::StreamError {
                    message: "Could not start capture (thread spawn failed)".to_string(),
                });
                return;
            }
        };
        log::info!(
            "Stream native start on background thread for crew {}; loop stays responsive",
            crew_id
        );
        self.pending_stream_start = Some(PendingStreamStart {
            crew_id: crew_id.to_string(),
            config,
            configured_bitrate_kbps,
            resp,
            thread,
        });
    }

    /// Poll the in-flight native start. Runs on the stream tick; never blocks.
    /// On success the start finishes below (resolution RPC, SFU, session).
    /// On failure the advertised session is retracted so no viewer watches a
    /// stream that never started (2026-09-16).
    pub(super) async fn poll_pending_stream_start(&mut self) {
        let Some(pending) = self.pending_stream_start.take() else {
            return;
        };
        if !pending.thread.is_finished() {
            // Still running; put it back and keep the loop moving.
            self.pending_stream_start = Some(pending);
            return;
        }
        let PendingStreamStart {
            crew_id,
            config,
            configured_bitrate_kbps,
            resp,
            thread,
        } = pending;
        let joined = match thread.join() {
            Ok(outcome) => outcome,
            Err(_) => Err("Stream start thread panicked".to_string()),
        };
        match joined {
            Ok(outcome) => {
                self.finish_stream_start(&crew_id, outcome, config, configured_bitrate_kbps, resp)
                    .await;
            }
            Err(message) => {
                log::error!("Stream native start failed: {}", message);
                self.retract_advertised_stream(&crew_id).await;
                let _ = self.event_tx.send(Event::StreamError { message });
            }
        }
    }

    /// The second half of stream start: resolution RPC, sink selection, SFU
    /// connect, session creation. Runs on the loop; every await yields it.
    async fn finish_stream_start(
        &mut self,
        crew_id: &str,
        outcome: NativeStartOutcome,
        config: crate::stream::StreamConfig,
        configured_bitrate_kbps: u32,
        resp: crate::stream::host::StartStreamResponse,
    ) {
        let NativeStartOutcome {
            host,
            video_rx,
            audio_rx,
            teardown,
            actual_w,
            actual_h,
        } = outcome;
        log::info!("Host encode resolution: {}x{}", actual_w, actual_h);
        self.stream_encode_width = actual_w;
        self.stream_encode_height = actual_h;

        // Update backend with actual encode resolution (may differ from preset)
        if let Err(e) = self
            .nakama
            .update_stream_resolution(crew_id, self.stream_encode_width, self.stream_encode_height)
            .await
        {
            log::warn!("update_stream_resolution RPC failed: {}", e);
        }

        // The SFU is the only stream transport. A failed SFU setup ends the
        // start; there is no other path.
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
        let Some(sink) = sfu_sink else {
            let message = "SFU stream setup failed on host; aborting stream start";
            log::error!("{}", message);
            // Bounded native stop on the teardown thread, never inline here.
            drop(teardown);
            let _ = self.event_tx.send(Event::StreamError {
                message: message.to_string(),
            });
            self.stream_host_sink = None;
            self.stream_sfu_connection = None;
            return;
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
            teardown,
            Arc::clone(&sink),
            Some(self.event_tx.clone()),
        ) {
            Ok(session) => {
                let _ = self.event_tx.send(Event::StreamStarted {
                    crew_id: crew_id.to_string(),
                    session_id: session.session_id.clone(),
                    mode: session.mode.clone(),
                });
                self.stream_host_sink = Some(Arc::clone(&sink));
                self.stream_session = Some(session);
                self.stream_bitrate_kbps = configured_bitrate_kbps;
                self.host_pacing_last = None;
                self.host_pacing_last_at = Instant::now();
            }
            Err(e) => {
                // The teardown guard moved into create_stream_session and was
                // dropped there, which already stopped the host.
                log::error!("Failed to create stream session: {}", e);
                let _ = self.event_tx.send(Event::StreamError {
                    message: e.to_string(),
                });
                self.stream_host_sink = None;
            }
        }
    }

    /// Tells the backend to drop a stream session that never carried a frame.
    ///
    /// `start_stream` announces the stream to the crew before the host starts,
    /// which is what makes the button feel instant. When the host then fails,
    /// that announcement is wrong: viewers see a live stream and get black.
    /// This is the other half of that trade.
    pub(super) async fn retract_advertised_stream(&mut self, crew_id: &str) {
        log::info!(
            "Stream host did not start; retracting the session for crew {}",
            crew_id
        );
        let payload = serde_json::json!({ "crew_id": crew_id });
        if let Err(e) = self.nakama.rpc("stop_stream", &payload).await {
            // Nothing else to try here. The backend drops a session when the
            // host stops sending, so this is a delay, not a permanent lie.
            log::warn!("stop_stream RPC failed while retracting: {}", e);
        }
    }

    pub(super) async fn handle_stop_stream(&mut self) {
        // A native start in flight on its thread. END STREAM answers at
        // once: the UI is told first, the advertised session is retracted,
        // and the detached thread's outcome drops when it lands, which stops
        // the host through the bounded teardown guard.
        if let Some(pending) = self.pending_stream_start.take() {
            log::info!(
                "Stopping stream while native start is in flight for crew {}",
                pending.crew_id
            );
            let crew_id = pending.crew_id.clone();
            drop(pending.thread);
            let _ = self.event_tx.send(Event::StreamEnded {
                crew_id: crew_id.clone(),
            });
            self.retract_advertised_stream(&crew_id).await;
            return;
        }
        let Some(session) = self.stream_session.take() else {
            return;
        };
        log::info!("Stopping stream session {}", session.session_id);

        // The UI is told first. Nothing below may delay END STREAM: the manager
        // join is bounded, native teardown runs on its own thread, and the RPC
        // is best effort. On 2026-09-15 this event waited behind a native stop
        // that never returned, and the button looked dead.
        let crew_id = self.nakama.active_crew_id().map(String::from);
        let _ = self.event_tx.send(Event::StreamEnded {
            crew_id: crew_id.clone().unwrap_or_default(),
        });

        // Bounded. The native host stops on the teardown thread when the
        // manager task ends or is aborted.
        session.stop_and_wait().await;

        self.stream_host_sink = None;
        self.stream_sfu_connection = None;
        self.host_pacing_last = None;
        self.host_pacing_last_at = Instant::now();
        self.stream_encode_width = 0;
        self.stream_encode_height = 0;
        self.stream_bitrate_kbps = 0;

        if let Some(crew_id) = crew_id {
            let payload = serde_json::json!({ "crew_id": crew_id });
            if let Err(e) = self.nakama.rpc("stop_stream", &payload).await {
                log::warn!("stop_stream RPC failed: {}", e);
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

        // A stream without a session cannot be joined on the SFU.
        if session_id.is_empty() {
            log::error!("WatchStream for host {} has no session_id", host_id);
            let _ = self.event_tx.send(Event::StreamError {
                message: "The stream has no session to join".to_string(),
            });
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

        self.watch_stream_sfu(
            host_id,
            session_id,
            stream_width,
            stream_height,
            &watch_resp,
        )
        .await;
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

        let session = match conn.join_stream(ph, session_id, "viewer").await {
            Ok(session) => session,
            Err(e) => {
                log::error!("SFU viewer join_stream failed: {}", e);
                let _ = self.event_tx.send(Event::StreamError {
                    message: format!("SFU viewer join failed: {e}"),
                });
                return;
            }
        };
        let members = StreamMembers::from_joined(&session.members);

        if let Err(e) = conn.wait_for_datachannel_open().await {
            log::error!("SFU viewer stream transport failed: {}", e);
            let _ = self.event_tx.send(Event::StreamError {
                message: format!("SFU viewer transport failed: {e}"),
            });
            return;
        }

        log::info!("SFU viewer connected to session {}", session_id);
        let conn = Arc::new(conn);

        // Pause state arrives on the reliable control channel (host tab-out).
        // Registered late — messages before this are lost, but the host
        // re-broadcasts on viewer joins and periodically while paused.
        let pause_slot = Arc::new(PauseSlot::new());
        let mut pause_cb_data: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut pause_cb_peer: *mut mello_sys::MelloPeerConnection = std::ptr::null_mut();
        match conn.peer_nonnull() {
            Ok(peer) => {
                pause_cb_peer = peer.as_ptr();
                pause_cb_data = unsafe { register_pause_callback(peer, &pause_slot) };
            }
            Err(e) => {
                log::warn!("SFU viewer: pause callback registration failed: {}", e);
            }
        }

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
        self.viewer_state = Some(ViewerState {
            viewer: Some(viewer),
            sfu_connection: Some(conn),
            host_id: host_id.to_string(),
            members,
            _frame_cb_data: frame_cb_data,
            pause_slot,
            last_reported_paused: false,
            last_reported_pause_reason: String::new(),
            _pause_cb_data: pause_cb_data,
            pause_cb_peer,
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

    pub(super) async fn handle_stop_watching(&mut self) {
        if let Some(vs) = self.viewer_state.take() {
            log::info!("Stopping stream viewer for host {}", vs.host_id);
            // UI first; the SFU leave and the native teardown below are bounded
            // or off-loop and must not delay it.
            let _ = self.event_tx.send(Event::StreamWatchingStopped);
            if let Some(ref conn) = vs.sfu_connection {
                if tokio::time::timeout(SFU_LEAVE_DEADLINE, conn.leave())
                    .await
                    .is_err()
                {
                    log::warn!(
                        "SFU viewer leave did not finish within {} ms; dropping the connection",
                        SFU_LEAVE_DEADLINE.as_millis()
                    );
                }
            }
            // Native stop runs on the teardown thread (ViewerState::drop).
            drop(vs);
            self.frame_lifecycle
                .store(FRAME_STATE_PRESENTED, std::sync::atomic::Ordering::Release);
        }
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

    use super::viewer_membership_event;
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
            SfuConnection::create_stream_peer(std::ptr::null_mut(), StreamPeerRole::Viewer)
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
        let peer = unsafe { SfuConnection::create_stream_peer(std::ptr::null_mut(), role) }
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
