use mello_core::{Command, Event};
use slint::ComponentHandle;

use crate::app_context::AppContext;
use crate::MainWindow;

const POST_GAME_MIN_DURATION: u32 = 5;
/// Post-game prompt auto-dismisses after this long without interaction (spec 17 §7.2).
const POST_GAME_TIMEOUT_SECS: u64 = 30;
/// Games where linking a Riot account unlocks server-verified results — the
/// post-game "connect" CTA is offered after sessions of these.
const RIOT_LINK_GAMES: &[&str] = &["league-of-legends"];

pub fn handle(ctx: &AppContext, event: Event) {
    match event {
        Event::GameDetected {
            game_id,
            game_name,
            short_name,
            color,
            ..
        } => {
            log::info!("[ui] game detected: {}", game_name);
            ctx.app.set_game_active(true);
            ctx.app.set_game_id(game_id.into());
            ctx.app.set_game_name(game_name.into());
            ctx.app.set_game_short_name(short_name.into());
            let parsed = slint::Color::from_argb_encoded(parse_hex_color(&color));
            ctx.app.set_game_color(parsed);
            // Clear any stale summary/hint/CTA from a previous session.
            ctx.app.set_game_summary("".into());
            ctx.app.set_telemetry_hint("".into());
            ctx.app.set_riot_cta_visible(false);
            ctx.app.set_can_stream(true);
            ctx.app.set_bar_state(1);
        }
        Event::GameEnded {
            game_id,
            game_name,
            short_name: _,
            duration_min,
        } => {
            log::info!(
                "[ui] game ended: {} (duration={}min)",
                game_name,
                duration_min
            );
            ctx.app.set_can_stream(false);
            ctx.app.set_telemetry_hint("".into());
            if duration_min >= POST_GAME_MIN_DURATION {
                ctx.app.set_bar_state(2);
                start_post_game_timeout(ctx);
                maybe_offer_riot_link(ctx, &game_id);
            } else {
                ctx.app.set_game_active(false);
                ctx.app.set_bar_state(0);
            }
        }
        Event::PostGameTimeout => {
            log::info!("[ui] post-game timeout");
            ctx.app.set_game_active(false);
            ctx.app.set_can_stream(false);
            ctx.app.set_game_summary("".into());
            ctx.app.set_riot_cta_visible(false);
            ctx.app.set_bar_state(0);
        }
        Event::TelemetrySetupHint { game_id, hint } => {
            log::info!("[ui] telemetry setup hint for {}: {}", game_id, hint);
            // Shown under the game name in the "now playing" card.
            if ctx.app.get_bar_state() == 1 {
                ctx.app.set_telemetry_hint(hint.into());
            }
        }
        Event::MatchEnded {
            result,
            own_score,
            opp_score,
            map,
        } => {
            // Live match outcome; logged for now (HUD score is future work).
            log::info!(
                "[ui] match ended: {} {}-{} on {}",
                result,
                own_score,
                opp_score,
                map
            );
        }
        Event::SessionSummary {
            wins,
            losses,
            streak_after,
            ..
        } => {
            let summary = format_session_summary(wins, losses, streak_after);
            log::info!("[ui] session summary: {}", summary);
            // Pre-fill the visible post-game card with the auto-detected record
            // so the user can confirm/share instead of cold-tapping win/loss.
            // Only enrich — never (re)open the card: the session may have been
            // under the post-game threshold, the user may have already
            // dismissed it, or a new game may have started while the RPC ran.
            if ctx.app.get_bar_state() == 2 {
                ctx.app.set_game_summary(summary.into());
            }
        }
        _ => {}
    }
}

/// After a session of a Riot-linkable game, ask core for the link state; the
/// RiotStatus handler shows the post-game "connect" CTA if the account is
/// still unlinked. Skipped entirely once the user dismissed the CTA.
fn maybe_offer_riot_link(ctx: &AppContext, game_id: &str) {
    if !RIOT_LINK_GAMES.contains(&game_id) {
        return;
    }
    if ctx.settings.borrow().riot_prompt_dismissed || ctx.app.get_riot_linked() {
        return;
    }
    ctx.riot_cta_pending.set(true);
    let _ = ctx.cmd_tx.send(Command::LoadRiotStatus);
}

/// Arm the 30 s auto-dismiss for the post-game prompt. Storing the timer in
/// the context cancels any previous one; user interaction (reaction tap,
/// text submit, dismiss) also cancels it via `cancel_post_game_timeout`.
fn start_post_game_timeout(ctx: &AppContext) {
    let app_weak: slint::Weak<MainWindow> = ctx.app.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::SingleShot,
        std::time::Duration::from_secs(POST_GAME_TIMEOUT_SECS),
        move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            // Dismiss only if the prompt is still idle; leave text input,
            // confirmations, or a newly started game alone.
            if app.get_bar_state() == 2 {
                log::info!("[ui] post-game timeout");
                app.set_game_active(false);
                app.set_game_summary("".into());
                app.set_riot_cta_visible(false);
                app.set_bar_state(0);
            }
        },
    );
    *ctx.post_game_timer.borrow_mut() = Some(timer);
}

/// Build the pre-filled post-game record line, e.g. "5W–3L · 2-win streak".
fn format_session_summary(wins: u32, losses: u32, streak_after: i32) -> String {
    let streak = match streak_after.cmp(&0) {
        std::cmp::Ordering::Greater => format!(" · {}-win streak", streak_after),
        std::cmp::Ordering::Less => format!(" · {}-loss streak", streak_after.abs()),
        std::cmp::Ordering::Equal => String::new(),
    };
    format!("{}W\u{2013}{}L{}", wins, losses, streak)
}

pub(crate) fn parse_hex_color(hex: &str) -> u32 {
    let hex = hex.trim_start_matches('#');
    let rgb = u32::from_str_radix(hex, 16).unwrap_or(0x2a2a30);
    0xFF000000 | rgb
}
