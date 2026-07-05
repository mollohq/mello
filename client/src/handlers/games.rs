use std::rc::Rc;

use mello_core::Event;

use crate::app_context::AppContext;
use crate::GameIntegrationData;

pub fn handle(ctx: &AppContext, event: Event) {
    match event {
        Event::GamesSettings { games } => {
            *ctx.games_integrations.borrow_mut() = games;
            push_games_list(ctx);
        }
        Event::RiotStatus {
            available,
            linked,
            riot_id,
            ..
        } => {
            ctx.app.set_riot_available(available);
            ctx.app.set_riot_linked(linked);
            ctx.app.set_riot_id(riot_id.into());

            if linked {
                // Successful link (or already linked): close the dialog and
                // drop any pending/visible post-game CTA.
                ctx.app.set_riot_dialog_open(false);
                ctx.app.set_riot_dialog_busy(false);
                ctx.app.set_riot_cta_visible(false);
                ctx.riot_cta_pending.set(false);
            } else if ctx.riot_cta_pending.get() {
                // A Riot-linkable session just ended; show the CTA while the
                // post-game card is still up.
                ctx.riot_cta_pending.set(false);
                if available && ctx.app.get_bar_state() == 2 {
                    ctx.app.set_riot_cta_visible(true);
                }
            }
        }
        Event::RiotLinkFailed { reason } => {
            ctx.app.set_riot_dialog_busy(false);
            ctx.app
                .set_riot_dialog_error(friendly_link_error(&reason).into());
        }
        _ => {}
    }
}

/// Rebuild the Games settings row model: core-reported adapter info merged
/// with the persisted per-game consent state.
fn push_games_list(ctx: &AppContext) {
    let settings = ctx.settings.borrow();
    let rows: Vec<GameIntegrationData> = ctx
        .games_integrations
        .borrow()
        .iter()
        .map(|g| GameIntegrationData {
            game_id: g.game_id.as_str().into(),
            name: g.name.as_str().into(),
            short_name: g.short_name.as_str().into(),
            color: slint::Color::from_argb_encoded(super::game::parse_hex_color(&g.color)),
            install_status: match g.installed {
                Some(true) => "found".into(),
                Some(false) => "not-found".into(),
                None => "".into(),
            },
            writes_files: g.writes_files,
            note: g.note.as_str().into(),
            enabled: !settings.disabled_game_integrations.contains(&g.game_id),
            has_account_link: g.account_link.is_some(),
        })
        .collect();
    ctx.app
        .set_games_list(Rc::new(slint::VecModel::from(rows)).into());
}

/// Map raw RPC errors to something a user can act on.
fn friendly_link_error(reason: &str) -> String {
    let lower = reason.to_lowercase();
    if lower.contains("riot id must") || lower.contains("not found") {
        "Riot ID not found. Check the spelling — it looks like GameName#TAG.".to_string()
    } else if lower.contains("region") {
        "Pick the region your account plays in.".to_string()
    } else {
        format!("Couldn't link right now: {reason}")
    }
}
