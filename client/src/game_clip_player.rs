//! The game clip player in the client (plans/GAME-CLIPS.md 6.2): the view
//! over the feed area (`ClipPlayerUi`), its controls, and the native
//! presenter that shows the clip. There is one presenter: a watched stream
//! and a clip never show at the same time.

use std::cell::Cell;

use mello_core::{Command, Event};
use slint::ComponentHandle;

use crate::app_context::AppContext;
use crate::{ClipPlayerUi, FeedCardData};

thread_local! {
    /// The clip player made the presenter, so it closes it too.
    static OWNS_PRESENTER: Cell<bool> = const { Cell::new(false) };
}

/// The play button of a game clip card.
pub(crate) fn open(ctx: &AppContext, card: &FeedCardData) {
    let ui = ctx.app.global::<ClipPlayerUi>();
    ui.set_clipper_name(card.actor_name.clone());
    ui.set_clipper_initials(card.actor_initials.clone());
    ui.set_clipper_hue_index(card.actor_hue_index);
    ui.set_game_name(card.game_name.clone());
    ui.set_position_ms(0);
    ui.set_duration_ms(card.duration_ms);
    ui.set_paused(false);
    ui.set_ended(false);
    if ctx.app.get_is_watching() {
        ui.set_buffering(false);
        ui.set_error("Stop watching the stream to play a clip".into());
        ui.set_open(true);
        return;
    }
    ui.set_buffering(true);
    ui.set_error("".into());
    if !OWNS_PRESENTER.get() {
        // Before the view opens: its first geometry must reach the presenter.
        // The swap chain takes the clip's size at its first frame.
        crate::handlers::streaming::open_native_presenter(ctx, 1920, 1080, "a game clip");
        OWNS_PRESENTER.set(true);
    }
    ui.set_open(true);
    log::info!("UI: play game clip {}", card.clip_path);
    let _ = ctx.cmd_tx.send(Command::PlayGameClip {
        source: card.clip_path.to_string(),
    });
}

fn close_presenter(ctx: &AppContext) {
    if OWNS_PRESENTER.replace(false) {
        crate::handlers::streaming::close_native_presenter(ctx, "the game clip closed");
    }
}

pub(crate) fn wire(ctx: &AppContext) {
    let ui = ctx.app.global::<ClipPlayerUi>();

    let c = ctx.clone();
    ui.on_toggle_pause(move || {
        let ui = c.app.global::<ClipPlayerUi>();
        // An ended clip plays again from the start (core seeks to 0).
        let paused = !(ui.get_paused() || ui.get_ended());
        let _ = c.cmd_tx.send(Command::SetGameClipPaused { paused });
    });

    let c = ctx.clone();
    ui.on_seek_ms(move |ms| {
        let _ = c.cmd_tx.send(Command::SeekGameClip {
            position_ms: ms.max(0) as u64,
        });
    });

    let c = ctx.clone();
    ui.on_close(move || {
        log::info!("UI: close the game clip");
        c.app.global::<ClipPlayerUi>().set_open(false);
        close_presenter(&c);
        let _ = c.cmd_tx.send(Command::CloseGameClip);
    });
}

pub(crate) fn handle(ctx: &AppContext, event: Event) {
    let ui = ctx.app.global::<ClipPlayerUi>();
    match event {
        Event::GameClipPlayback {
            position_ms,
            duration_ms,
            buffering,
            paused,
            ended,
            ..
        } => {
            ui.set_position_ms(position_ms.min(i32::MAX as u64) as i32);
            if duration_ms > 0 {
                ui.set_duration_ms(duration_ms.min(i32::MAX as u64) as i32);
            }
            ui.set_buffering(buffering);
            ui.set_paused(paused);
            ui.set_ended(ended);
            // The first frame resized the swap chain to the clip's size: fit
            // it in the view again (a watched stream does this on its timer).
            #[cfg(target_os = "windows")]
            if OWNS_PRESENTER.get() {
                if let Some(presenter) = ctx.dcomp_presenter.borrow().as_ref() {
                    presenter.refresh_geometry_if_resized();
                }
            }
        }
        Event::GameClipPlaybackClosed { reason } => {
            close_presenter(ctx);
            if reason.is_empty() {
                ui.set_open(false);
            } else {
                log::info!("UI: the game clip closed: {reason}");
                ui.set_buffering(false);
                ui.set_error(reason.into());
            }
        }
        _ => {}
    }
}
