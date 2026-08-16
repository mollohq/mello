use mello_core::Command;
use slint::ComponentHandle;

use crate::app_context::AppContext;

pub fn wire(ctx: &AppContext) {
    // Per-game integration consent toggle (Games settings tab).
    {
        let cmd = ctx.cmd_tx.clone();
        let s = ctx.settings.clone();
        ctx.app
            .on_games_integration_toggled(move |game_id, enabled| {
                let game_id = game_id.to_string();
                let mut settings = s.borrow_mut();
                settings
                    .disabled_game_integrations
                    .retain(|id| id != &game_id);
                if !enabled {
                    settings.disabled_game_integrations.push(game_id);
                }
                settings.save();
                let _ = cmd.send(Command::SetGameIntegrations {
                    disabled: settings.disabled_game_integrations.clone(),
                });
            });
    }

    // Riot link dialog submit (from settings tab or post-game CTA).
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_riot_link_submitted(move |riot_id, region| {
            let _ = cmd.send(Command::RiotLink {
                riot_id: riot_id.trim().to_string(),
                region: region.to_string(),
            });
        });
    }

    // Disconnect from the Games settings tab.
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_riot_disconnect_clicked(move || {
            let _ = cmd.send(Command::RiotUnlink);
        });
    }

    // Post-game CTA clicked: the dialog opens in slint; just stop the 30 s
    // post-game auto-dismiss so the card doesn't vanish mid-linking.
    {
        let post_game_timer = ctx.post_game_timer.clone();
        ctx.app.on_riot_cta_clicked(move || {
            post_game_timer.borrow_mut().take();
        });
    }

    // Post-game CTA dismissed ("×"): never re-ask.
    {
        let s = ctx.settings.clone();
        ctx.app.on_riot_cta_dismissed(move || {
            let mut settings = s.borrow_mut();
            settings.riot_prompt_dismissed = true;
            settings.save();
        });
    }

    // Unknown-game prompt: "TRACK" confirms the candidate as a custom game.
    // Only now does the game enter the DB (presence/sessions/ledger follow via
    // the normal sensing machinery on the next scan).
    {
        let cmd = ctx.cmd_tx.clone();
        let s = ctx.settings.clone();
        let pending = ctx.pending_unknown_game.clone();
        let app_weak = ctx.app.as_weak();
        let icon_cache = ctx.game_icon_cache.clone();
        let rt = ctx.rt.clone();
        ctx.app.on_unknown_game_track_clicked(move || {
            let Some((exe, path, display_name)) = pending.borrow_mut().take() else {
                return;
            };
            let game = mello_core::user_games::CustomGame {
                id: mello_core::user_games::custom_game_id(&exe),
                name: display_name.clone(),
                short_name: crate::converters::make_initials(&display_name),
                exe: exe.clone(),
            };
            log::info!("[ui] tracking custom game: {} ({})", game.name, game.id);
            {
                let mut settings = s.borrow_mut();
                settings
                    .custom_games
                    .retain(|g| !g.exe.eq_ignore_ascii_case(&exe));
                settings
                    .custom_games
                    .push(crate::settings::CustomGameSetting {
                        id: game.id.clone(),
                        name: game.name.clone(),
                        short_name: game.short_name.clone(),
                        exe: game.exe.clone(),
                    });
                settings.save();
            }
            let _ = cmd.send(Command::AddCustomGame { game: game.clone() });
            // Grab the exe's icon for cards/badges and share it with the crew.
            crate::game_icons::extract_and_cache(
                icon_cache.clone(),
                cmd.clone(),
                rt.clone(),
                game.id.clone(),
                path,
            );
            if let Some(app) = app_weak.upgrade() {
                app.set_unknown_game_visible(false);
                app.set_unknown_game_name("".into());
            }
        });
    }

    // Unknown-game prompt: "Not a game" permanently dismisses this exe.
    {
        let s = ctx.settings.clone();
        let pending = ctx.pending_unknown_game.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_unknown_game_dismiss_clicked(move || {
            let Some((exe, _path, _name)) = pending.borrow_mut().take() else {
                return;
            };
            let mut settings = s.borrow_mut();
            let key = exe.to_lowercase();
            if !settings.unknown_game_dismissed.contains(&key) {
                settings.unknown_game_dismissed.push(key);
            }
            settings.save();
            if let Some(app) = app_weak.upgrade() {
                app.set_unknown_game_visible(false);
                app.set_unknown_game_name("".into());
            }
        });
    }
}
