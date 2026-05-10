use mello_core::Command;
use slint::{ComponentHandle, Model};

use crate::app_context::AppContext;
use crate::snapshot_cache;
use crate::MainWindow;

pub fn wire(ctx: &AppContext) {
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

    let app_weak: slint::Weak<MainWindow> = ctx.app.as_weak();
    ctx.app.on_request_session_frame(move |session_id, index| {
        let sid = session_id.to_string();
        let idx = index as usize;
        log::debug!("[snapshot] request frame sid={} idx={}", sid, idx);

        if let Some(app) = app_weak.upgrade() {
            let cards = app.get_feed_cards();
            let card_count = cards.row_count();
            let url: Option<String> = (0..card_count)
                .filter_map(|i| cards.row_data(i))
                .find(|c| c.id == sid.as_str())
                .and_then(|c| {
                    let urls = c.snapshot_urls;
                    let url_count = urls.row_count();
                    if idx < url_count {
                        urls.row_data(idx)
                            .map(|u: slint::SharedString| u.to_string())
                    } else {
                        log::warn!("[snapshot] idx {} out of range (total={})", idx, url_count);
                        None
                    }
                });

            if url.is_none() {
                log::warn!(
                    "[snapshot] no URL found for sid={} idx={} (cards={})",
                    sid,
                    idx,
                    card_count
                );
            }

            if let Some(ref url) = url {
                match snapshot_cache::decode_snapshot(url) {
                    Some(img) => {
                        app.set_session_frame_ready(false);
                        app.set_session_frame_id(sid.clone().into());
                        app.set_session_pushed_frame(img);
                        app.set_session_frame_ready(true);
                        log::debug!("[snapshot] pushed frame sid={} idx={}", sid, idx);
                    }
                    None => {
                        log::warn!("[snapshot] decode failed for sid={} idx={}", sid, idx);
                    }
                }
            }
        }
    });

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
