use std::rc::Rc;

use mello_core::events::StreamRole;
use mello_core::{Command, Event};
use slint::Model;

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
        Event::StreamViewerJoined { viewer_id } => {
            log::info!("Stream viewer joined: {}", viewer_id);
        }
        Event::StreamViewerLeft { viewer_id } => {
            log::info!("Stream viewer left: {}", viewer_id);
        }
        Event::StreamMembersChanged { members } => {
            // No UI shows who else watches a stream. The e2e state port
            // reports the list (e2e_state.rs).
            log::debug!("Stream members: {}", members.len());
        }
        Event::StreamWatching {
            host_id,
            width,
            height,
        } => {
            log::info!("Watching stream from {} ({}x{})", host_id, width, height);
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
        // ("Stream timeline"); no debug panel field shows it yet.
        Event::StreamViewerTimeline { .. } => {}
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
                    ctx.app.set_is_hosting(false);
                    ctx.app.set_stream_host_state("".into());
                    ctx.app.set_stream_label("".into());
                    let name = ctx.app.get_user_name().to_string();
                    show_stream_notice(ctx, "failed", "STREAM DID NOT START", &name, &reason);
                }
                StreamRole::Viewer => {
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
