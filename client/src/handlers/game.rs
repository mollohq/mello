use mello_core::Event;

use crate::app_context::AppContext;

const POST_GAME_MIN_DURATION: u32 = 5;

pub fn handle(ctx: &AppContext, event: Event) {
    match event {
        Event::GameDetected {
            game_name,
            short_name,
            color,
            ..
        } => {
            log::info!("[ui] game detected: {}", game_name);
            ctx.app.set_game_active(true);
            ctx.app.set_game_name(game_name.into());
            ctx.app.set_game_short_name(short_name.into());
            let parsed = slint::Color::from_argb_encoded(parse_hex_color(&color));
            ctx.app.set_game_color(parsed);
            // Clear any stale summary from a previous session.
            ctx.app.set_game_summary("".into());
            ctx.app.set_can_stream(true);
            ctx.app.set_bar_state(1);
        }
        Event::GameEnded {
            game_id: _,
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
            if duration_min >= POST_GAME_MIN_DURATION {
                ctx.app.set_bar_state(2);
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
            ctx.app.set_bar_state(0);
        }
        Event::MatchEnded {
            result,
            rounds_won,
            rounds_lost,
            map,
        } => {
            // Live match outcome; logged for now (HUD score is future work).
            log::info!(
                "[ui] match ended: {} {}-{} on {}",
                result,
                rounds_won,
                rounds_lost,
                map
            );
        }
        Event::SessionSummary {
            wins,
            losses,
            draws,
            streak_after,
            ..
        } => {
            let summary = format_session_summary(wins, losses, draws, streak_after);
            log::info!("[ui] session summary: {}", summary);
            // Pre-fill the post-game card with the auto-detected record so the
            // user can confirm/share instead of cold-tapping win/loss.
            ctx.app.set_game_summary(summary.into());
            ctx.app.set_game_active(true);
            ctx.app.set_bar_state(2);
        }
        _ => {}
    }
}

/// Build the pre-filled post-game record line, e.g. "5W–3L · 2-win streak".
fn format_session_summary(wins: u32, losses: u32, draws: u32, streak_after: i32) -> String {
    let record = if wins + losses == 0 && draws > 0 {
        // A draw-only night (e.g. a 15-15 Premier).
        if draws == 1 {
            "1 draw".to_string()
        } else {
            format!("{} draws", draws)
        }
    } else if draws > 0 {
        format!("{}W\u{2013}{}L\u{2013}{}D", wins, losses, draws)
    } else {
        format!("{}W\u{2013}{}L", wins, losses)
    };
    let streak = match streak_after.cmp(&0) {
        std::cmp::Ordering::Greater => format!(" · {}-win streak", streak_after),
        std::cmp::Ordering::Less => format!(" · {}-loss streak", streak_after.abs()),
        std::cmp::Ordering::Equal => String::new(),
    };
    format!("{}{}", record, streak)
}

fn parse_hex_color(hex: &str) -> u32 {
    let hex = hex.trim_start_matches('#');
    let rgb = u32::from_str_radix(hex, 16).unwrap_or(0x2a2a30);
    0xFF000000 | rgb
}
