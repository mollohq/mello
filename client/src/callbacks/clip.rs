use mello_core::Command;
use slint::ComponentHandle;

use crate::app_context::AppContext;
use crate::snapshot_loader;
use crate::MainWindow;

pub fn wire(ctx: &AppContext) {
    ctx.app.on_feed_show_more_clicked(|| {
        log::debug!("UI: feed show more (premium pagination — not implemented)");
    });

    let settings = ctx.settings.clone();
    ctx.app.on_session_seen(move |session_id| {
        let id = session_id.to_string();
        if !id.is_empty() {
            log::info!("UI: session seen {}", id);
            let mut s = settings.borrow_mut();
            if !s.seen_session_ids.contains(&id) {
                s.seen_session_ids.push(id);
                s.save();
            }
        }
    });

    let loader = ctx.snapshot_loader.clone();
    let app_weak: slint::Weak<MainWindow> = ctx.app.as_weak();
    ctx.app
        .on_session_preview_request_frame(move |session_id, index| {
            let sid = session_id.to_string();
            let idx = index;
            log::debug!("[snapshot] playback frame request sid={} idx={}", sid, idx);

            if let Some(app) = app_weak.upgrade() {
                let Some(url) = snapshot_loader::snapshot_url_for_card(&app, &sid, idx as usize)
                else {
                    log::warn!("[snapshot] no URL for sid={} idx={}", sid, idx);
                    return;
                };
                let gen = loader.current_generation();
                loader.request_playback_frame(app_weak.clone(), sid, url, idx, gen);
            }
        });

    let cmd = ctx.cmd_tx.clone();
    ctx.app.on_clip_captured(move || {
        log::info!("UI: clip button pressed");
        let _ = cmd.send(Command::CaptureClip { seconds: 30.0 });
    });

    let cmd = ctx.cmd_tx.clone();
    ctx.app.on_play_clip(move |path| {
        let path = path.to_string();
        if !path.is_empty() {
            log::info!("UI: play clip {}", path);
            let _ = cmd.send(Command::PlayClip { path });
        }
    });

    let cmd = ctx.cmd_tx.clone();
    ctx.app.on_pause_clip(move || {
        log::info!("UI: pause clip");
        let _ = cmd.send(Command::PauseClip);
    });

    let cmd = ctx.cmd_tx.clone();
    ctx.app.on_resume_clip(move || {
        log::info!("UI: resume clip");
        let _ = cmd.send(Command::ResumeClip);
    });

    let cmd = ctx.cmd_tx.clone();
    let app_weak: slint::Weak<MainWindow> = ctx.app.as_weak();
    ctx.app.on_seek_clip(move |normalized| {
        if let Some(app) = app_weak.upgrade() {
            let dur_text = app.get_clip_duration_text().to_string();
            let parts: Vec<&str> = dur_text.split(':').collect();
            let duration_ms = if parts.len() == 2 {
                let mins: u32 = parts[0].parse().unwrap_or(0);
                let secs: u32 = parts[1].parse().unwrap_or(0);
                (mins * 60 + secs) * 1000
            } else {
                0
            };
            let position_ms = (normalized * duration_ms as f32) as u32;
            log::info!("UI: seek clip to {}ms", position_ms);
            let _ = cmd.send(Command::SeekClip { position_ms });
        }
    });
}
