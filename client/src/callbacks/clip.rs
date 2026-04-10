use mello_core::Command;
use slint::ComponentHandle;

use crate::app_context::AppContext;
use crate::MainWindow;

pub fn wire(ctx: &AppContext) {
    let cmd = ctx.cmd_tx.clone();
    ctx.app.on_clip_captured(move || {
        log::info!("UI: clip button pressed");
        let _ = cmd.try_send(Command::CaptureClip { seconds: 30.0 });
    });

    let cmd = ctx.cmd_tx.clone();
    ctx.app.on_play_clip(move |path| {
        let path = path.to_string();
        if !path.is_empty() {
            log::info!("UI: play clip {}", path);
            let _ = cmd.try_send(Command::PlayClip { path });
        }
    });

    let cmd = ctx.cmd_tx.clone();
    ctx.app.on_pause_clip(move || {
        log::info!("UI: pause clip");
        let _ = cmd.try_send(Command::PauseClip);
    });

    let cmd = ctx.cmd_tx.clone();
    ctx.app.on_resume_clip(move || {
        log::info!("UI: resume clip");
        let _ = cmd.try_send(Command::ResumeClip);
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
            let _ = cmd.try_send(Command::SeekClip { position_ms });
        }
    });
}
