use mello_core::Command;

use crate::app_context::AppContext;

pub fn wire(ctx: &AppContext) {
    // Activity-sharing consent (Games settings tab).
    {
        let cmd = ctx.cmd_tx.clone();
        let s = ctx.settings.clone();
        ctx.app
            .on_setting_changed_share_game_activity(move |enabled| {
                let mut settings = s.borrow_mut();
                settings.share_game_activity = enabled;
                settings.save();
                let _ = cmd.send(Command::SetShareGameActivity { enabled });
                log::info!("[settings] share_game_activity = {}", enabled);
            });
    }

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
}
