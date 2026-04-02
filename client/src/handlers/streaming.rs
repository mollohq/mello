use std::rc::Rc;

use mello_core::Event;
use slint::Model;

use crate::CaptureSourceData;
use crate::app_context::AppContext;

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
            ctx.app.set_stream_label("STREAMING".into());
        }
        Event::StreamEnded { crew_id } => {
            log::info!("Stream ended: crew={}", crew_id);
            ctx.app.set_is_hosting(false);
            ctx.app.set_is_watching(false);
            ctx.app.set_streamer_name("".into());
            ctx.app.set_stream_label("".into());
            ctx.app.set_stream_frame(slint::Image::default());
            ctx.app.set_active_streamer_id("".into());
            ctx.app.set_active_streamer_name("".into());
            ctx.app.set_active_stream_session_id("".into());
            ctx.app.set_active_stream_width(0);
            ctx.app.set_active_stream_height(0);
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
        }
        Event::StreamWatchingStopped => {
            log::info!("Stopped watching stream");
            ctx.app.set_is_watching(false);
            ctx.app.set_streamer_name("".into());
            ctx.app.set_stream_label("".into());
            ctx.app.set_stream_frame(slint::Image::default());
        }
        Event::StreamFrame { .. } => {}
        Event::StreamError { message } => {
            log::error!("Stream error: {}", message);
        }
        _ => {}
    }
}
