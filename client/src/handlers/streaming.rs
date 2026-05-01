use std::rc::Rc;

use mello_core::Event;
use slint::{ComponentHandle, Model};

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
        Event::StreamEnded { crew_id } => {
            log::info!("Stream ended: crew={}", crew_id);
            ctx.app.set_is_hosting(false);
            ctx.app.set_is_watching(false);
            ctx.app.set_streamer_name("".into());
            ctx.app.set_stream_label("".into());
            ctx.app.set_stream_frame(slint::Image::default());
            #[cfg(target_os = "windows")]
            {
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
        Event::StreamViewerJoined { viewer_id } => {
            log::info!("Stream viewer joined: {}", viewer_id);
        }
        Event::StreamViewerLeft { viewer_id } => {
            log::info!("Stream viewer left: {}", viewer_id);
        }
        Event::StreamWatching {
            host_id,
            width,
            height,
        } => {
            log::info!("Watching stream from {} ({}x{})", host_id, width, height);
            ctx.app.set_is_watching(true);
            ctx.app.set_streamer_name(host_id.into());
            #[cfg(target_os = "windows")]
            {
                use crate::dcomp_presenter::DCompPresenter;
                use i_slint_backend_winit::WinitWindowAccessor;
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
                    match DCompPresenter::new(hwnd, width, height, 0.0, 0.0) {
                        Ok(p) => {
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
            ctx.app.set_streamer_name("".into());
            ctx.app.set_stream_label("".into());
            ctx.app.set_stream_frame(slint::Image::default());
            #[cfg(target_os = "windows")]
            {
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
        _ => {}
    }
}
