use mello_core::Command;
use slint::ComponentHandle;

use crate::app_context::AppContext;
use crate::snapshot_cache::THUMB_MAX_WIDTH;
use crate::snapshot_loader;
use crate::{FeedView, MainWindow};

pub fn wire(ctx: &AppContext) {
    ctx.app.on_feed_show_more_clicked(|| {
        log::debug!("UI: feed show more (premium pagination — not implemented)");
    });

    // The zone feed scrolled near the end of Crew history.
    crate::handlers::zones::wire_history_paging(ctx);

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

    let loader = ctx.snapshot_loader.clone();
    let app_weak: slint::Weak<MainWindow> = ctx.app.as_weak();
    ctx.app
        .global::<FeedView>()
        .on_poster_wanted(move |card_id, wanted, w, h| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            // `image-fit: cover` on a 16:9 poster: the drawn width covers the
            // card width and the height at 16:9, whichever is wider.
            let scale = app.window().scale_factor();
            let drawn = (w.max(h * 16.0 / 9.0) * scale).ceil() as u32;
            let max_w = drawn.clamp(1, THUMB_MAX_WIDTH);
            log::debug!(
                "[snapshot] poster wanted={} card={} w={}",
                wanted,
                card_id,
                max_w
            );
            loader.poster_wanted(app_weak.clone(), card_id.to_string(), wanted, max_w);
        });

    let cmd = ctx.cmd_tx.clone();
    ctx.app.on_clip_captured(move || {
        log::info!("UI: clip button pressed");
        let _ = cmd.send(Command::CaptureClip { seconds: 30.0 });
    });

    crate::game_clip_player::wire(ctx);

    let cmd = ctx.cmd_tx.clone();
    // A strong `AppContext`: the clip player opens the presenter with it.
    // `Harness::drop` clears this callback, or the window would own itself.
    let player_ctx = ctx.clone();
    ctx.app.on_play_clip(move |path| {
        let path = path.to_string();
        // A game clip plays in the clip player (CLIPS.md 15.9).
        if let Some(card) = crate::handlers::clip::game_clip_card(&player_ctx.app, &path) {
            crate::game_clip_player::open(&player_ctx, &card);
            return;
        }
        if !path.is_empty() {
            log::info!("UI: play clip {}", path);
            let _ = cmd.send(Command::PlayClip { path });
        }
    });

    let cmd = ctx.cmd_tx.clone();
    let app_weak: slint::Weak<MainWindow> = ctx.app.as_weak();
    ctx.app.on_pause_clip(move || {
        log::info!("UI: pause clip");
        if let Some(app) = app_weak.upgrade() {
            app.set_clip_paused(true);
        }
        let _ = cmd.send(Command::PauseClip);
    });

    let cmd = ctx.cmd_tx.clone();
    let app_weak: slint::Weak<MainWindow> = ctx.app.as_weak();
    ctx.app.on_resume_clip(move || {
        log::info!("UI: resume clip");
        if let Some(app) = app_weak.upgrade() {
            app.set_clip_paused(false);
        }
        let _ = cmd.send(Command::ResumeClip);
    });

    let cmd = ctx.cmd_tx.clone();
    ctx.app.on_seek_clip_ms(move |position_ms| {
        // Absolute milliseconds arrive from the waveform, which knows the
        // per-card duration — the global clip-duration-ms is zeroed when
        // playback finishes and would break seeks from idle.
        if position_ms >= 0 {
            log::info!("UI: seek clip to {}ms", position_ms);
            let _ = cmd.send(Command::SeekClip {
                position_ms: position_ms as u32,
            });
        }
    });
}
