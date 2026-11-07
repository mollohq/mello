//! The game clip key (F8) in the client (CLIPS.md 15.2): the replay
//! state that core reports, the key, and the HUD toast. The Settings section
//! and the Keybinds row are in `callbacks/settings.rs`. A saved clip goes to
//! core for the post and the upload (`mello_core::clip_upload`). The clip
//! card comes with the todo `feed-card`.

use mello_core::events::GameClipState;
use mello_core::{Command, Event};

use crate::app_context::AppContext;
use crate::hud_manager::HudClipToast;

const DISK_LOW_TOAST: &str = "Game clips are off: the disk is almost full";
const NO_PERMISSION_TOAST: &str = "Allow Screen Recording in Settings to clip your game";

/// The replay as core last reported it.
#[derive(Debug, Clone, Default)]
pub struct GameClipUi {
    pub state: GameClipState,
    pub game_name: String,
    pub reason: String,
}

/// F8: a clip while the replay runs. A paused or waiting replay still has
/// its ring, so it clips too. Otherwise the HUD toast says why (CLIPS.md 15.2).
pub(crate) fn game_clip_key_pressed(ctx: &AppContext) {
    let ui = ctx.game_clip.borrow().clone();
    match ui.state {
        GameClipState::Recording | GameClipState::Waiting | GameClipState::Paused => {
            log::info!("[game clip] key: clip {}", ui.game_name);
            let _ = ctx.cmd_tx.send(Command::CaptureGameClip);
        }
        GameClipState::Off => {
            let why = if !ctx.settings.borrow().game_clips_enabled {
                "Game clips are off in Settings"
            } else if ctx.app.get_in_voice() {
                "No game to clip"
            } else {
                "Join voice to clip your game"
            };
            log::info!("[game clip] key ignored: {why}");
            toast(ctx, why);
        }
        GameClipState::DiskLow => {
            log::info!("[game clip] key ignored: the disk is almost full");
            toast(ctx, DISK_LOW_TOAST);
        }
        GameClipState::Unavailable => {
            log::info!(
                "[game clip] key ignored: not available for {} ({})",
                ui.game_name,
                ui.reason
            );
            toast(ctx, "Clips are not available for this game");
        }
        GameClipState::NoPermission => {
            log::info!("[game clip] key ignored: no screen recording permission");
            toast(ctx, NO_PERMISSION_TOAST);
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
            let was = ctx.game_clip.borrow().state;
            // macOS: the Settings row that asks for Screen Recording.
            ctx.app
                .set_game_clips_need_permission(state == GameClipState::NoPermission);
            *ctx.game_clip.borrow_mut() = GameClipUi {
                state,
                game_name,
                reason,
            };
            // CLIPS.md 15.2: the replay stops on a full disk, and says so once.
            if state == GameClipState::DiskLow && was != GameClipState::DiskLow {
                toast(ctx, DISK_LOW_TOAST);
            }
        }
        Event::GameClipCaptured {
            clip_id,
            path,
            poster_path,
            game_name,
            duration_seconds,
            width,
            height,
        } => {
            crate::sounds::cue(ctx, mello_core::sounds::Sound::ClipSaved);
            log::info!(
                "[game clip] saved {} ({}, {:.1} s, {}x{})",
                path,
                game_name,
                duration_seconds,
                width,
                height
            );
            toast(ctx, "Game clip saved");
            // The clip goes to the active crew (CLIPS.md 15.2), and core
            // uploads it in the background (CLIPS.md 15.7).
            let crew_id = ctx.app.get_active_crew_id().to_string();
            if crew_id.is_empty() {
                log::warn!("[game clip] {clip_id} stays local: no active crew");
                return;
            }
            let _ = ctx.cmd_tx.send(Command::ShareGameClip {
                crew_id,
                clip_id,
                path,
                poster_path,
                duration_seconds,
                width,
                height,
                game_name,
            });
        }
        Event::GameClipFailed { reason } => {
            log::warn!("[game clip] no clip: {reason}");
            toast(ctx, "No game clip");
        }
        Event::GameClipUploadFailed { clip_id, reason } => {
            log::warn!("[game clip] {clip_id} does not go up: {reason}");
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
