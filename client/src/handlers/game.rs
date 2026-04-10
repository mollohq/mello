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
            ctx.app.set_bar_state(0);
        }
        _ => {}
    }
}

fn parse_hex_color(hex: &str) -> u32 {
    let hex = hex.trim_start_matches('#');
    let rgb = u32::from_str_radix(hex, 16).unwrap_or(0x2a2a30);
    0xFF000000 | rgb
}
