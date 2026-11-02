//! The game clip key (F8) in the client (plans/GAME-CLIPS.md 4.3): the replay
//! state that core reports, the key, and the HUD toast. The Settings row, the
//! clip card and the upload come with the todos `client-key-settings`,
//! `feed-card` and `upload`.

use mello_core::events::GameClipState;
use mello_core::{Command, Event};

use crate::app_context::AppContext;
use crate::hud_manager::HudClipToast;

/// The replay as core last reported it.
#[derive(Debug, Clone, Default)]
pub struct GameClipUi {
    pub state: GameClipState,
    pub game_name: String,
    pub reason: String,
}

/// F8: a clip while the replay runs. A paused or waiting replay still has
/// its ring, so it clips too. Otherwise the HUD toast says why (plan 4.3).
pub(crate) fn game_clip_key_pressed(ctx: &AppContext) {
    let ui = ctx.game_clip.borrow().clone();
    match ui.state {
        GameClipState::Recording | GameClipState::Waiting | GameClipState::Paused => {
            log::info!("[game clip] key: clip {}", ui.game_name);
            let _ = ctx.cmd_tx.send(Command::CaptureGameClip);
        }
        GameClipState::Off => {
            let why = if ctx.app.get_in_voice() {
                "No game to clip"
            } else {
                "Join voice to clip your game"
            };
            log::info!("[game clip] key ignored: {why}");
            toast(ctx, why);
        }
        GameClipState::Unavailable => {
            log::info!(
                "[game clip] key ignored: not available for {} ({})",
                ui.game_name,
                ui.reason
            );
            toast(ctx, "Clips are not available for this game");
        }
    }
}

pub(crate) fn handle(ctx: &AppContext, event: Event) {
    match event {
        Event::GameClipStateChanged {
            state,
            game_name,
            reason,
        } => {
            log::info!("[game clip] replay {:?} {} {}", state, game_name, reason);
            *ctx.game_clip.borrow_mut() = GameClipUi {
                state,
                game_name,
                reason,
            };
        }
        Event::GameClipCaptured {
            path,
            game_name,
            duration_seconds,
            width,
            height,
            ..
        } => {
            log::info!(
                "[game clip] saved {} ({}, {:.1} s, {}x{})",
                path,
                game_name,
                duration_seconds,
                width,
                height
            );
            toast(ctx, "Game clip saved");
        }
        Event::GameClipFailed { reason } => {
            log::warn!("[game clip] no clip: {reason}");
            toast(ctx, "No game clip");
        }
        _ => {}
    }
}

/// The HUD clip toast over the game (4 s, the overlay's own timer). With
/// the HUD off or hidden, the log line is all.
fn toast(ctx: &AppContext, label: &str) {
    if !ctx.hud_manager.is_enabled() {
        return;
    }
    let mode = ctx.fg_monitor.borrow().current_mode();
    let mut state = crate::hud_state_builder::build_hud_state(ctx, mode);
    state.clip_toast = Some(HudClipToast {
        label: label.to_string(),
    });
    ctx.hud_manager.push_state(state);
}
