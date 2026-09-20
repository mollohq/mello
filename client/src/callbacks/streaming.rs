use mello_core::Command;
use slint::{ComponentHandle, Model};

use crate::app_context::AppContext;
use crate::converters::parse_capture_source_id;

fn source_name_from_model(
    model: slint::ModelRc<crate::CaptureSourceData>,
    source_id: &str,
) -> Option<String> {
    for row in 0..model.row_count() {
        if let Some(entry) = model.row_data(row) {
            if entry.id == source_id {
                let name = entry.name.to_string();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }
    None
}

fn source_exe_from_model(
    model: slint::ModelRc<crate::CaptureSourceData>,
    source_id: &str,
) -> Option<String> {
    for row in 0..model.row_count() {
        if let Some(entry) = model.row_data(row) {
            if entry.id == source_id {
                let exe = entry.exe.to_string();
                if !exe.is_empty() {
                    return Some(exe);
                }
            }
        }
    }
    None
}

fn resolve_stream_source_exe(
    app: &crate::MainWindow,
    source_id: &str,
    source_mode: &str,
) -> String {
    let by_mode = match source_mode {
        "monitor" => source_exe_from_model(app.get_stream_monitors(), source_id),
        "process" | "game" => source_exe_from_model(app.get_stream_games(), source_id),
        "window" => source_exe_from_model(app.get_stream_windows(), source_id),
        _ => None,
    };
    by_mode
        .or_else(|| source_exe_from_model(app.get_stream_monitors(), source_id))
        .or_else(|| source_exe_from_model(app.get_stream_games(), source_id))
        .or_else(|| source_exe_from_model(app.get_stream_windows(), source_id))
        .unwrap_or_default()
}

fn resolve_stream_source_name(
    app: &crate::MainWindow,
    source_id: &str,
    source_mode: &str,
) -> Option<String> {
    // "process" is what the source list actually tags games with; "game" was the
    // only spelling matched here, so every game fell through to the fallback
    // chain below — which tries monitors first and would mislabel a game as a
    // display if their ids ever collided.
    let by_mode = match source_mode {
        "monitor" => source_name_from_model(app.get_stream_monitors(), source_id),
        "process" | "game" => source_name_from_model(app.get_stream_games(), source_id),
        "window" => source_name_from_model(app.get_stream_windows(), source_id),
        _ => None,
    };

    if by_mode.is_some() {
        return by_mode;
    }

    source_name_from_model(app.get_stream_monitors(), source_id)
        .or_else(|| source_name_from_model(app.get_stream_games(), source_id))
        .or_else(|| source_name_from_model(app.get_stream_windows(), source_id))
}

pub fn wire(ctx: &AppContext) {
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_list_capture_sources(move || {
            let _ = cmd.send(Command::ListCaptureSources);
            let _ = cmd.send(Command::StartThumbnailRefresh);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app
            .on_start_stream(move |source_id, source_mode, preset_idx| {
                let mode = source_mode.to_string();
                let id = source_id.to_string();
                let crew_id = if let Some(app) = app_weak.upgrade() {
                    let source_title =
                        resolve_stream_source_name(&app, &id, &mode).unwrap_or("STREAMING".into());
                    app.set_stream_label(source_title.into());
                    app.get_active_crew_id().to_string()
                } else {
                    return;
                };
                if crew_id.is_empty() {
                    return;
                }

                let stream_title = if let Some(app) = app_weak.upgrade() {
                    app.get_stream_label().to_string()
                } else {
                    "STREAMING".to_string()
                };

                let (monitor_index, hwnd, pid) = parse_capture_source_id(&id, &mode);
                let exe = app_weak
                    .upgrade()
                    .map(|app| resolve_stream_source_exe(&app, &id, &mode))
                    .unwrap_or_default();

                let _ = cmd.send(Command::StopThumbnailRefresh);
                let _ = cmd.send(Command::StartStream {
                    crew_id,
                    title: stream_title,
                    capture_mode: mode,
                    monitor_index,
                    hwnd,
                    pid,
                    preset: preset_idx as u32,
                    exe,
                });
            });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_stop_stream(move || {
            // Show the result of the click at once. The core confirms with
            // StreamEnded, which clears the rest of the stream state. If the
            // core is busy, the button must still respond (2026-09-15 beta).
            if let Some(app) = app_weak.upgrade() {
                app.set_is_hosting(false);
            }
            let _ = cmd.send(Command::StopStream);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_stop_thumbnail_refresh(move || {
            let _ = cmd.send(Command::StopThumbnailRefresh);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_stop_watching(move || {
            let _ = cmd.send(Command::StopWatching);
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
                let _ = cmd.send(Command::WatchStream {
                    host_id: host_id.to_string(),
                    session_id: session_id.to_string(),
                    width: width as u32,
                    height: height as u32,
                });
            });
    }
}
