use mello_core::Command;
use slint::ComponentHandle;

use crate::app_context::AppContext;
use crate::converters::parse_capture_source_id;

pub fn wire(ctx: &AppContext) {
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_list_capture_sources(move || {
            let _ = cmd.try_send(Command::ListCaptureSources);
            let _ = cmd.try_send(Command::StartThumbnailRefresh);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app
            .on_start_stream(move |source_id, source_mode, preset_idx| {
                let crew_id = if let Some(app) = app_weak.upgrade() {
                    app.get_active_crew_id().to_string()
                } else {
                    return;
                };
                if crew_id.is_empty() {
                    return;
                }

                let mode = source_mode.to_string();
                let id = source_id.to_string();

                let (monitor_index, hwnd, pid) = parse_capture_source_id(&id, &mode);

                let _ = cmd.try_send(Command::StopThumbnailRefresh);
                let _ = cmd.try_send(Command::StartStream {
                    crew_id,
                    title: String::new(),
                    capture_mode: mode,
                    monitor_index,
                    hwnd,
                    pid,
                    preset: preset_idx as u32,
                });
            });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_stop_stream(move || {
            let _ = cmd.try_send(Command::StopStream);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_stop_thumbnail_refresh(move || {
            let _ = cmd.try_send(Command::StopThumbnailRefresh);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_stop_watching(move || {
            let _ = cmd.try_send(Command::StopWatching);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app
            .on_watch_stream(move |host_id, session_id, width, height| {
                log::info!(
                    "UI: watch stream from host {} session={} ({}x{})",
                    host_id,
                    session_id,
                    width,
                    height
                );
                let _ = cmd.try_send(Command::WatchStream {
                    host_id: host_id.to_string(),
                    session_id: session_id.to_string(),
                    width: width as u32,
                    height: height as u32,
                });
            });
    }
}
