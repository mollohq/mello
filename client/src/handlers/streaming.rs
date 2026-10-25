use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use mello_core::events::StreamRole;
use mello_core::{Command, Event};
use slint::{ComponentHandle as _, Model};

use super::stream_cards::sync_active_stream_cards;
use crate::app_context::AppContext;
use crate::CaptureSourceData;

pub fn handle(ctx: &AppContext, event: Event) {
    match event {
        Event::CaptureSourcesListed {
            monitors,
            games,
            windows,
        } => {
            log::info!(
                "Capture sources: {} monitors, {} games, {} windows",
                monitors.len(),
                games.len(),
                windows.len()
            );
            let mon: Vec<CaptureSourceData> = monitors
                .into_iter()
                .map(|s| CaptureSourceData {
                    id: s.id.into(),
                    name: s.name.into(),
                    mode: s.mode.into(),
                    pid: s.pid.unwrap_or(0) as i32,
                    exe: s.exe.into(),
                    igdb_id: s.igdb_id as i32,
                    is_fullscreen: s.is_fullscreen,
                    resolution: s.resolution.into(),
                    ..Default::default()
                })
                .collect();
            let gam: Vec<CaptureSourceData> = games
                .into_iter()
                .map(|s| CaptureSourceData {
                    id: s.id.into(),
                    name: s.name.into(),
                    mode: s.mode.into(),
                    pid: s.pid.unwrap_or(0) as i32,
                    exe: s.exe.into(),
                    igdb_id: s.igdb_id as i32,
                    is_fullscreen: s.is_fullscreen,
                    resolution: s.resolution.into(),
                    ..Default::default()
                })
                .collect();
            let win: Vec<CaptureSourceData> = windows
                .into_iter()
                .map(|s| CaptureSourceData {
                    id: s.id.into(),
                    name: s.name.into(),
                    mode: s.mode.into(),
                    pid: s.pid.unwrap_or(0) as i32,
                    exe: s.exe.into(),
                    igdb_id: s.igdb_id as i32,
                    is_fullscreen: s.is_fullscreen,
                    resolution: s.resolution.into(),
                    ..Default::default()
                })
                .collect();
            ctx.app
                .set_stream_monitors(Rc::new(slint::VecModel::from(mon)).into());
            ctx.app
                .set_stream_games(Rc::new(slint::VecModel::from(gam)).into());
            ctx.app
                .set_stream_windows(Rc::new(slint::VecModel::from(win)).into());
        }
        Event::WindowThumbnailsUpdated { thumbnails } => {
            let model = ctx.app.get_stream_windows();
            for (id, rgba, w, h) in thumbnails {
                for row in 0..model.row_count() {
                    if let Some(mut entry) = model.row_data(row) {
                        if entry.id == id.as_str() {
                            let mut pixel_buf =
                                slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
                            pixel_buf.make_mut_bytes().copy_from_slice(&rgba);
                            entry.thumbnail = slint::Image::from_rgba8(pixel_buf);
                            entry.has_thumbnail = true;
                            model.set_row_data(row, entry);
                            break;
                        }
                    }
                }
            }
        }
        Event::StreamStarted {
            crew_id,
            session_id,
            mode,
        } => {
            log::info!(
                "Stream started: crew={} session={} mode={}",
                crew_id,
                session_id,
                mode
            );
            ctx.app.set_is_hosting(true);
            ctx.app.set_stream_host_state("live".into());
            ctx.app.set_streamer_name(ctx.app.get_user_name());
            if ctx.app.get_stream_label().is_empty() {
                ctx.app.set_stream_label("STREAMING".into());
            }
            ctx.app.set_dbg_stream_mode(mode.into());
            ctx.app.set_dbg_host_pacing_mode("idle".into());
            ctx.app.set_dbg_host_pacing_target_kbps(0);
            ctx.app.set_dbg_host_pacing_out_kbps(0.0);
            ctx.app.set_dbg_host_pacing_mb(0.0);
            ctx.app.set_dbg_host_pacing_sleep_count(0);
            ctx.app.set_dbg_host_pacing_sleep_ms(0);
            ctx.app.set_dbg_host_pacing_sleep_delta_count(0);
            ctx.app.set_dbg_host_pacing_sleep_delta_ms(0);
            set_host_quality(ctx);
            sync_active_stream_cards(ctx);
        }
        // A watched stream ended: the card says why. StreamWatchingStopped
        // follows and stops the viewer.
        Event::StreamEnded {
            role: StreamRole::Viewer,
            reason,
            ..
        } => {
            log::info!("Watched stream ended: {}", reason);
            end_watched_card(ctx, "the watched stream ended");
            let name = ctx.app.get_active_streamer_name().to_string();
            show_stream_notice(
                ctx,
                "ended",
                "STREAM ENDED",
                &name,
                ended_reason_text(&reason),
            );
        }
        Event::StreamEnded {
            crew_id, reason, ..
        } => {
            log::info!("Stream ended: crew={} reason={}", crew_id, reason);
            // Stage 9: the fullscreen window goes before the presenter.
            end_watched_card(ctx, "the stream ended");
            end_hosted_card(ctx);
            ctx.app.set_stream_host_state("".into());
            if !reason.is_empty() && reason != "stopped" {
                let name = ctx.app.get_user_name().to_string();
                show_stream_notice(
                    ctx,
                    "ended",
                    "STREAM ENDED",
                    &name,
                    ended_reason_text(&reason),
                );
            }
            ctx.app.set_is_hosting(false);
            ctx.app.set_is_watching(false);
            ctx.app.set_stream_paused(false);
            ctx.app.set_stream_paused_reason("".into());
            ctx.app.set_paused_streamer_name("".into());
            ctx.app.set_stream_host_paused(false);
            ctx.stream_frame_timer.set_watching(false);
            ctx.app.set_streamer_name("".into());
            ctx.app.set_stream_label("".into());
            ctx.app.set_stream_frame(slint::Image::default());
            #[cfg(target_os = "windows")]
            {
                // No present may reach the swap chain once it goes.
                mello_core::set_native_presenter(None);
                let _ = ctx.dcomp_presenter.borrow_mut().take();
                log::info!("DComp presenter destroyed (stream ended)");
            }
            ctx.app.set_active_streamer_id("".into());
            ctx.app.set_active_streamer_name("".into());
            ctx.app.set_active_stream_title("".into());
            ctx.app.set_active_stream_session_id("".into());
            ctx.app.set_active_stream_width(0);
            ctx.app.set_active_stream_height(0);
            ctx.app.set_active_stream_viewer_count(0);
            ctx.app.set_dbg_stream_mode("idle".into());
            ctx.app.set_dbg_stream_frames(0);
            ctx.app.set_dbg_stream_packets(0);
            ctx.app.set_dbg_stream_mb(0.0);
            ctx.app.set_dbg_stream_kbps(0.0);
            ctx.app.set_dbg_stream_fps(0.0);
            ctx.app.set_dbg_stream_ui_render_fps(0.0);
            ctx.app.set_dbg_stream_truncations(0);
            ctx.app.set_dbg_host_pacing_mode("idle".into());
            ctx.app.set_dbg_host_pacing_target_kbps(0);
            ctx.app.set_dbg_host_pacing_out_kbps(0.0);
            ctx.app.set_dbg_host_pacing_mb(0.0);
            ctx.app.set_dbg_host_pacing_sleep_count(0);
            ctx.app.set_dbg_host_pacing_sleep_ms(0);
            ctx.app.set_dbg_host_pacing_sleep_delta_count(0);
            ctx.app.set_dbg_host_pacing_sleep_delta_ms(0);
            sync_active_stream_cards(ctx);
        }
        Event::StreamTargetExited => {
            // The captured game quit. Stop hosting through the normal path so
            // the session tears down and the backend tells the viewers.
            log::info!("Stream target exited — stopping hosted stream");
            let _ = ctx.cmd_tx.send(Command::StopStream);
        }
        // Stage 9: the hosted card counts its viewers.
        Event::StreamViewerJoined { viewer_id } => {
            log::info!("Stream viewer joined: {}", viewer_id);
            if ctx.app.get_is_hosting() {
                ctx.stream_card.borrow_mut().host_viewers.insert(viewer_id);
                sync_active_stream_cards(ctx);
            }
        }
        Event::StreamViewerLeft { viewer_id } => {
            log::info!("Stream viewer left: {}", viewer_id);
            if ctx.stream_card.borrow_mut().host_viewers.remove(&viewer_id) {
                sync_active_stream_cards(ctx);
            }
        }
        Event::StreamMembersChanged { members } => {
            // The e2e state port reports the list (e2e_state.rs). Stage 9:
            // the watched card counts the viewers: the others, and you.
            log::debug!("Stream members: {}", members.len());
            let others = members.iter().filter(|m| m.role == "viewer").count();
            ctx.stream_card.borrow_mut().watched_viewers = Some(others + 1);
            sync_active_stream_cards(ctx);
        }
        Event::StreamWatching {
            host_id,
            width,
            height,
        } => {
            log::info!("Watching stream from {} ({}x{})", host_id, width, height);
            start_watched_card(ctx, &host_id);
            ctx.app.set_is_watching(true);
            ctx.app.set_stream_viewer_state("".into());
            ctx.app.set_stream_paused(false);
            ctx.app.set_stream_paused_reason("".into());
            ctx.app.set_paused_streamer_name("".into());
            ctx.stream_frame_timer.set_watching(true);
            ctx.app.set_streamer_name(host_id.into());
            #[cfg(target_os = "windows")]
            {
                use crate::dcomp_presenter::DCompPresenter;
                use i_slint_backend_winit::WinitWindowAccessor;
                use slint::ComponentHandle as _;
                let hwnd = ctx
                    .app
                    .window()
                    .with_winit_window(|w: &i_slint_backend_winit::winit::window::Window| {
                        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                        match w.window_handle().map(|h| h.as_raw()) {
                            Ok(RawWindowHandle::Win32(h)) => Some(h.hwnd.get()),
                            _ => None,
                        }
                    })
                    .flatten();
                if let Some(hwnd) = hwnd {
                    // The presenter opens libmello's shared textures, so its
                    // device has to sit on the same GPU libmello decodes on.
                    let adapter_luid = mello_core::video_adapter_luid();
                    match DCompPresenter::new(hwnd, width, height, 0.0, 0.0, adapter_luid) {
                        Ok(p) => {
                            // libmello's present thread shows each frame
                            // through this swap chain (stage 3).
                            mello_core::set_native_presenter(Some(p.frame_presenter()));
                            *ctx.dcomp_presenter.borrow_mut() = Some(p);
                            log::info!("DComp presenter created for stream watching");
                        }
                        Err(e) => {
                            log::error!("Failed to create DComp presenter: {}", e);
                        }
                    }
                } else {
                    log::error!("Could not obtain HWND for DComp presenter");
                }
            }
            ctx.app.set_dbg_stream_ui_render_fps(0.0);
            sync_active_stream_cards(ctx);
        }
        Event::StreamWatchingStopped => {
            log::info!("Stopped watching stream");
            // Stage 9: the fullscreen window goes before the presenter.
            end_watched_card(ctx, "stopped watching");
            ctx.app.set_is_watching(false);
            ctx.app.set_stream_viewer_state("".into());
            ctx.app.set_stream_paused(false);
            ctx.app.set_stream_paused_reason("".into());
            ctx.app.set_paused_streamer_name("".into());
            ctx.stream_frame_timer.set_watching(false);
            ctx.app.set_streamer_name("".into());
            ctx.app.set_stream_label("".into());
            ctx.app.set_stream_frame(slint::Image::default());
            #[cfg(target_os = "windows")]
            {
                // No present may reach the swap chain once it goes.
                mello_core::set_native_presenter(None);
                let _ = ctx.dcomp_presenter.borrow_mut().take();
                log::info!("DComp presenter destroyed (stopped watching)");
            }
            ctx.app.set_dbg_stream_mode("idle".into());
            ctx.app.set_dbg_stream_frames(0);
            ctx.app.set_dbg_stream_packets(0);
            ctx.app.set_dbg_stream_mb(0.0);
            ctx.app.set_dbg_stream_kbps(0.0);
            ctx.app.set_dbg_stream_fps(0.0);
            ctx.app.set_dbg_stream_ui_render_fps(0.0);
            ctx.app.set_dbg_stream_truncations(0);
            ctx.app.set_dbg_host_pacing_mode("idle".into());
            ctx.app.set_dbg_host_pacing_target_kbps(0);
            ctx.app.set_dbg_host_pacing_out_kbps(0.0);
            ctx.app.set_dbg_host_pacing_mb(0.0);
            ctx.app.set_dbg_host_pacing_sleep_count(0);
            ctx.app.set_dbg_host_pacing_sleep_ms(0);
            ctx.app.set_dbg_host_pacing_sleep_delta_count(0);
            ctx.app.set_dbg_host_pacing_sleep_delta_ms(0);
            sync_active_stream_cards(ctx);
        }
        Event::StreamFrame { .. } => {}
        Event::StreamPaused {
            host_id,
            paused,
            reason,
        } => {
            log::info!(
                "Stream pause state: host={} paused={} reason={}",
                host_id,
                paused,
                reason
            );
            if paused {
                // Stage 9: the card's pause card says why; the fullscreen
                // window cannot. A stream that joins paused shows no frame,
                // so it is not "starting" any more either.
                crate::stream_fullscreen::exit(ctx, "the stream paused");
                ctx.app
                    .global::<crate::StreamCardUi>()
                    .set_viewer_starting(false);
            }
            ctx.app.set_stream_paused(paused);
            ctx.app.set_stream_paused_reason(reason.into());
            ctx.app
                .set_paused_streamer_name(if paused { host_id.into() } else { "".into() });
            #[cfg(target_os = "windows")]
            {
                // The DComp video layer composites above Slint content: hide
                // it so the Slint pause card shows, restore on resume.
                if let Some(p) = ctx.dcomp_presenter.borrow().as_ref() {
                    // set_content_visible takes &self; borrow scope ends here.
                    p.set_content_visible(!paused);
                }
            }
            sync_active_stream_cards(ctx);
        }
        Event::StreamHostPaused { paused } => {
            log::info!("Host stream pause state: paused={}", paused);
            ctx.app.set_stream_host_paused(paused);
        }
        Event::StreamDebugStats {
            mode,
            transport_packets,
            transport_bytes,
            transport_truncations,
            frames_presented,
            present_fps,
            ingress_kbps,
        } => {
            ctx.app.set_dbg_stream_mode(mode.into());
            ctx.app.set_dbg_stream_frames(frames_presented as i32);
            ctx.app.set_dbg_stream_packets(transport_packets as i32);
            ctx.app
                .set_dbg_stream_mb((transport_bytes as f32) / (1024.0 * 1024.0));
            ctx.app.set_dbg_stream_kbps(ingress_kbps);
            ctx.app.set_dbg_stream_fps(present_fps);
            ctx.app
                .set_dbg_stream_truncations(transport_truncations as i32);
        }
        // The viewer timeline (stage 3): mello-core logs it once a second
        // ("Stream timeline"). Stage 9: the card's stats strip shows it, and
        // a released frame ends "starting".
        Event::StreamViewerTimeline {
            target_delay_ms,
            jitter_ms,
            decode_ms_p95,
            frames_released,
            frames_late,
            audio_delay_ms,
            av_offset_ms,
            ..
        } => {
            let ui = ctx.app.global::<crate::StreamCardUi>();
            ui.set_stats(stream_stats(
                target_delay_ms,
                jitter_ms,
                decode_ms_p95,
                frames_late,
                audio_delay_ms,
                av_offset_ms,
            ));
            if frames_released > 0 {
                mark_first_frame(&ctx.app);
            }
        }
        Event::StreamHostPacingStats {
            mode,
            target_kbps,
            out_kbps,
            paced_bytes,
            sleep_count,
            sleep_ms_total,
            sleep_count_delta,
            sleep_ms_delta,
        } => {
            ctx.app
                .global::<crate::StreamCardUi>()
                .set_host_sending(sending_text(out_kbps).into());
            ctx.app.set_dbg_host_pacing_mode(mode.into());
            ctx.app.set_dbg_host_pacing_target_kbps(target_kbps as i32);
            ctx.app.set_dbg_host_pacing_out_kbps(out_kbps);
            ctx.app
                .set_dbg_host_pacing_mb((paced_bytes as f32) / (1024.0 * 1024.0));
            ctx.app.set_dbg_host_pacing_sleep_count(sleep_count as i32);
            ctx.app.set_dbg_host_pacing_sleep_ms(sleep_ms_total as i32);
            ctx.app
                .set_dbg_host_pacing_sleep_delta_count(sleep_count_delta as i32);
            ctx.app
                .set_dbg_host_pacing_sleep_delta_ms(sleep_ms_delta as i32);
        }
        Event::StreamError { message } => {
            log::error!("Stream error: {}", message);
        }
        // --- stage 6 (streaming-quality): stream session states ---
        Event::StreamStarting { crew_id } => {
            log::info!("Stream starting: crew={}", crew_id);
            clear_stream_notice(ctx);
            ctx.stream_card.borrow_mut().host_viewers.clear();
            set_host_quality(ctx);
            ctx.app.set_is_hosting(true);
            ctx.app.set_stream_host_state("starting".into());
            ctx.app.set_streamer_name(ctx.app.get_user_name());
            if ctx.app.get_stream_label().is_empty() {
                ctx.app.set_stream_label("STREAMING".into());
            }
            sync_active_stream_cards(ctx);
        }
        Event::StreamReconnecting { role } => {
            log::warn!("Stream reconnecting: {:?}", role);
            match role {
                StreamRole::Host => ctx.app.set_stream_host_state("reconnecting".into()),
                StreamRole::Viewer => {
                    ctx.app.set_stream_viewer_state("reconnecting".into());
                    #[cfg(target_os = "windows")]
                    if let Some(p) = ctx.dcomp_presenter.borrow_mut().as_mut() {
                        p.set_reconnecting_wash(true);
                    }
                }
            }
            sync_active_stream_cards(ctx);
        }
        Event::StreamResumed { role } => {
            log::info!("Stream resumed: {:?}", role);
            match role {
                StreamRole::Host => ctx.app.set_stream_host_state("live".into()),
                StreamRole::Viewer => {
                    ctx.app.set_stream_viewer_state("".into());
                    #[cfg(target_os = "windows")]
                    if let Some(p) = ctx.dcomp_presenter.borrow_mut().as_mut() {
                        p.set_reconnecting_wash(false);
                    }
                }
            }
            sync_active_stream_cards(ctx);
        }
        Event::StreamFailed { role, reason } => {
            log::error!("Stream failed ({:?}): {}", role, reason);
            match role {
                StreamRole::Host => {
                    end_hosted_card(ctx);
                    ctx.app.set_is_hosting(false);
                    ctx.app.set_stream_host_state("".into());
                    ctx.app.set_stream_label("".into());
                    let name = ctx.app.get_user_name().to_string();
                    show_stream_notice(ctx, "failed", "STREAM DID NOT START", &name, &reason);
                }
                StreamRole::Viewer => {
                    end_watched_card(ctx, "the watch failed");
                    let name = ctx.app.get_active_streamer_name().to_string();
                    show_stream_notice(ctx, "failed", "COULD NOT WATCH", &name, &reason);
                }
            }
        }
        // --- end stage 6 ---
        _ => {}
    }
}

// --- stage 6 (streaming-quality): the stream notice card ---

/// How long a stream notice card stays.
const STREAM_NOTICE_FOR: std::time::Duration = std::time::Duration::from_secs(8);

/// The card copy for a `StreamEnded` reason.
fn ended_reason_text(reason: &str) -> &'static str {
    match reason {
        "connection_lost" => "The connection to the stream was lost for 30 seconds.",
        "lost_on_server" => "The server lost the stream during a reconnect.",
        "host_left" => "The host ended the stream.",
        _ => "The stream ended.",
    }
}

/// Show a card that says why a stream ended or did not start, for a few
/// seconds.
fn show_stream_notice(ctx: &AppContext, state: &str, title: &str, name: &str, reason: &str) {
    ctx.app.set_stream_notice_state(state.into());
    ctx.app.set_stream_notice_title(title.into());
    ctx.app.set_stream_notice_name(name.into());
    ctx.app.set_stream_notice(reason.into());
    sync_active_stream_cards(ctx);
    let ctx = ctx.clone();
    let shown = reason.to_string();
    slint::Timer::single_shot(STREAM_NOTICE_FOR, move || {
        // A newer notice stays.
        if ctx.app.get_stream_notice().as_str() == shown {
            clear_stream_notice(&ctx);
            sync_active_stream_cards(&ctx);
        }
    });
}

fn clear_stream_notice(ctx: &AppContext) {
    ctx.app.set_stream_notice_state("".into());
    ctx.app.set_stream_notice_title("".into());
    ctx.app.set_stream_notice_name("".into());
    ctx.app.set_stream_notice("".into());
}

// --- stage 9 (streaming-quality): the stream card's controls ---

/// The volume of one host's game audio, for this user only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamAudio {
    /// 0 to 200 %. The mute keeps it.
    pub volume: u16,
    pub muted: bool,
}

impl Default for StreamAudio {
    fn default() -> Self {
        Self {
            volume: 100,
            muted: false,
        }
    }
}

impl StreamAudio {
    /// The gain for libmello: 0.0 when muted, else the percent over 100.
    pub fn gain(self) -> f32 {
        if self.muted {
            0.0
        } else {
            f32::from(self.volume.min(200)) / 100.0
        }
    }
}

/// The stream card's values that the UI does not hold.
#[derive(Debug, Default)]
pub struct StreamCardState {
    /// The volume of each host's game audio, by host id, for the life of
    /// the app (a restart clears it, as the member volumes).
    audio: HashMap<String, StreamAudio>,
    /// The host of the watched stream: the key of `audio`.
    watched_host: String,
    /// The viewers of the watched session (the SFU membership, you
    /// included). None until the first membership snapshot.
    pub watched_viewers: Option<usize>,
    /// The viewers of the hosted stream.
    pub host_viewers: HashSet<String>,
}

impl StreamCardState {
    /// The volume of the watched host.
    pub fn watched_audio(&self) -> StreamAudio {
        self.audio
            .get(&self.watched_host)
            .copied()
            .unwrap_or_default()
    }
}

/// A watch starts: the card shows this host's volume, and libmello gets it
/// when it is not unity (a new view starts at unity).
fn start_watched_card(ctx: &crate::app_context::AppContext, host_id: &str) {
    let audio = {
        let mut state = ctx.stream_card.borrow_mut();
        state.watched_host = host_id.to_string();
        state.watched_viewers = None;
        state.watched_audio()
    };
    show_stream_audio(ctx, audio);
    if audio != StreamAudio::default() {
        let _ = ctx
            .cmd_tx
            .send(Command::SetStreamAudioVolume { gain: audio.gain() });
    }
}

/// The watched card goes: its fullscreen window, popover, label and stats
/// go with it.
fn end_watched_card(ctx: &crate::app_context::AppContext, why: &str) {
    crate::stream_fullscreen::exit(ctx, why);
    let ui = ctx.app.global::<crate::StreamCardUi>();
    if matches!(ui.get_popover().as_str(), "volume" | "settings") {
        ui.set_popover("".into());
    }
    ui.set_viewer_starting(false);
    ui.set_format_label("".into());
    ui.set_stats(crate::StreamStatsData::default());
    ctx.stream_card.borrow_mut().watched_viewers = None;
}

/// The hosted card goes.
fn end_hosted_card(ctx: &crate::app_context::AppContext) {
    let ui = ctx.app.global::<crate::StreamCardUi>();
    if ui.get_popover().as_str() == "host-settings" {
        ui.set_popover("".into());
    }
    ui.set_host_sending("".into());
    ctx.stream_card.borrow_mut().host_viewers.clear();
}

fn show_stream_audio(ctx: &crate::app_context::AppContext, audio: StreamAudio) {
    let ui = ctx.app.global::<crate::StreamCardUi>();
    ui.set_volume(i32::from(audio.volume));
    ui.set_muted(audio.muted);
}

/// The card's volume slider: store the value for the watched host and
/// apply it.
pub fn set_stream_volume(ctx: &crate::app_context::AppContext, percent: i32) {
    update_stream_audio(ctx, |a| a.volume = percent.clamp(0, 200) as u16);
}

/// The card's mute switch. The volume stays.
pub fn set_stream_muted(ctx: &crate::app_context::AppContext, muted: bool) {
    update_stream_audio(ctx, |a| a.muted = muted);
}

fn update_stream_audio(
    ctx: &crate::app_context::AppContext,
    change: impl FnOnce(&mut StreamAudio),
) {
    let audio = {
        let mut state = ctx.stream_card.borrow_mut();
        let host = state.watched_host.clone();
        if host.is_empty() {
            return;
        }
        let mut audio = state.watched_audio();
        change(&mut audio);
        if audio == StreamAudio::default() {
            state.audio.remove(&host);
        } else {
            state.audio.insert(host, audio);
        }
        audio
    };
    log::info!("Stream volume: {} % (muted: {})", audio.volume, audio.muted);
    show_stream_audio(ctx, audio);
    let _ = ctx
        .cmd_tx
        .send(Command::SetStreamAudioVolume { gain: audio.gain() });
}

/// The first frame shows: the watch is no longer "starting".
pub fn mark_first_frame(app: &crate::MainWindow) {
    let ui = app.global::<crate::StreamCardUi>();
    if ui.get_viewer_starting() {
        ui.set_viewer_starting(false);
    }
}

/// "720p60": the picture's height (from the SPS when the decoder has one)
/// and the frame rate of the host's rung. Without a rung, the height alone.
pub fn format_label(height: u32, fps: u32) -> String {
    match (height, fps) {
        (0, _) => String::new(),
        (h, 0) => format!("{h}p"),
        (h, f) => format!("{h}p{f}"),
    }
}

/// Set the card's format label from the decoded picture height (0 when not
/// known) and the host's last rung. The stream timer calls it.
pub fn refresh_stream_format(app: &crate::MainWindow, decoded_height: u32) {
    let rung = mello_core::stream::viewer_format::viewer_format().rung;
    let height = if decoded_height > 0 {
        decoded_height
    } else {
        rung.map(|r| r.height).unwrap_or(0)
    };
    let label = format_label(height, rung.map(|r| r.fps).unwrap_or(0));
    let ui = app.global::<crate::StreamCardUi>();
    if ui.get_format_label().as_str() != label {
        ui.set_format_label(label.into());
    }
}

/// The stats strip's values from the 1 s viewer timeline.
pub fn stream_stats(
    target_delay_ms: f32,
    jitter_ms: f32,
    decode_ms_p95: f32,
    frames_late: u64,
    audio_delay_ms: f32,
    av_offset_ms: f32,
) -> crate::StreamStatsData {
    let ms = |v: f32| format!("{} ms", v.round() as i64);
    // No game audio: no audio delay and no offset to show.
    let has_audio = audio_delay_ms > 0.0;
    crate::StreamStatsData {
        target_delay: ms(target_delay_ms).into(),
        jitter: ms(jitter_ms).into(),
        decode: format!("{decode_ms_p95:.1} ms").into(),
        frames_late: frames_late.to_string().into(),
        audio_delay: if has_audio {
            ms(audio_delay_ms).into()
        } else {
            "-".into()
        },
        av_offset: if has_audio {
            let v = av_offset_ms.round() as i64;
            if v > 0 {
                format!("+{v} ms").into()
            } else {
                format!("{v} ms").into()
            }
        } else {
            "-".into()
        },
    }
}

/// The host's quality: the preset the stream started with, as the quality
/// pills name it.
fn set_host_quality(ctx: &crate::app_context::AppContext) {
    let text = match ctx.app.get_stream_preset() {
        0 => "ULTRA \u{b7} 1080p60",
        1 => "HIGH \u{b7} 1080p30",
        2 => "MEDIUM \u{b7} 720p60",
        3 => "PRO-POTATO \u{b7} 720p30",
        _ => "POTATO \u{b7} 480p30",
    };
    ctx.app
        .global::<crate::StreamCardUi>()
        .set_host_quality(text.into());
}

/// What the host sends now, from the pacer's output rate.
pub fn sending_text(out_kbps: f32) -> String {
    if out_kbps >= 1000.0 {
        format!("{:.1} Mbps", out_kbps / 1000.0)
    } else {
        format!("{} kbps", out_kbps.round() as i64)
    }
}

#[cfg(test)]
mod stage9_tests {
    use super::*;

    #[test]
    fn the_format_label_names_the_height_and_the_rung_rate() {
        assert_eq!(format_label(720, 60), "720p60");
        assert_eq!(format_label(480, 30), "480p30");
        assert_eq!(
            format_label(1080, 0),
            "1080p",
            "no rung yet: the height alone"
        );
        assert_eq!(format_label(0, 60), "", "no picture yet: no label");
    }

    #[test]
    fn the_stats_strip_formats_the_timeline() {
        let s = stream_stats(118.4, 6.2, 2.06, 3, 141.6, 8.4);
        assert_eq!(s.target_delay, "118 ms");
        assert_eq!(s.jitter, "6 ms");
        assert_eq!(s.decode, "2.1 ms");
        assert_eq!(s.frames_late, "3");
        assert_eq!(s.audio_delay, "142 ms");
        assert_eq!(s.av_offset, "+8 ms");
        assert_eq!(
            stream_stats(1.0, 1.0, 1.0, 0, 120.0, -12.4).av_offset,
            "-12 ms"
        );
        let silent = stream_stats(100.0, 5.0, 2.0, 0, 0.0, 0.0);
        assert_eq!(
            (silent.audio_delay.as_str(), silent.av_offset.as_str()),
            ("-", "-")
        );
    }

    #[test]
    fn a_muted_stream_has_no_gain_and_keeps_its_volume() {
        let a = StreamAudio {
            volume: 70,
            muted: true,
        };
        assert_eq!(a.gain(), 0.0);
        assert_eq!(StreamAudio { muted: false, ..a }.gain(), 0.7);
        assert_eq!(StreamAudio::default().gain(), 1.0);
    }

    #[test]
    fn the_send_rate_reads_in_mbps_or_kbps() {
        assert_eq!(sending_text(6200.0), "6.2 Mbps");
        assert_eq!(sending_text(850.4), "850 kbps");
    }
}
