use mello_core::crew_events::UserGameStats;
use mello_core::game_db::GameDatabase;
use mello_core::Event;
use slint::{Color, ModelRc, SharedString, VecModel};

use crate::app_context::AppContext;
use crate::YouStripData;

pub fn handle(ctx: &AppContext, event: Event) {
    if let Event::UserGameStatsLoaded { games } = event {
        log::info!("[ui] user game stats loaded ({} games)", games.len());
        ctx.app.set_you_strip(build_you_strip(&games));
    }
}

fn empty_strip() -> YouStripData {
    YouStripData {
        has_stats: false,
        game_name: SharedString::new(),
        short_name: SharedString::new(),
        game_color: Color::default(),
        streak_text: SharedString::new(),
        streak_positive: true,
        record_text: SharedString::new(),
        win_rate_text: SharedString::new(),
        recent_form: ModelRc::new(VecModel::from(Vec::<SharedString>::new())),
    }
}

fn build_you_strip(games: &[UserGameStats]) -> YouStripData {
    // Backend returns games newest-played first; pick the first with a record.
    let Some(g) = games.iter().find(|g| g.wins + g.losses + g.draws > 0) else {
        return empty_strip();
    };

    let db = GameDatabase::load_bundled();
    let (name, short, color) = match db.lookup_by_id(&g.game_id) {
        Some(e) => (
            e.name.clone(),
            e.short_name.clone(),
            parse_hex_color(e.color.as_deref().unwrap_or("#888888")),
        ),
        None => (
            g.game_id.clone(),
            g.game_id.clone(),
            parse_hex_color("#888888"),
        ),
    };

    let streak = g.current_streak;
    let streak_text = if streak > 0 {
        format!("W{streak}")
    } else if streak < 0 {
        format!("L{}", -streak)
    } else {
        "—".to_string()
    };

    let decided = g.wins + g.losses;
    let win_rate_text = if decided > 0 {
        format!("{}%", (g.wins * 100) / decided)
    } else {
        "—".to_string()
    };

    // Newest-last form; show the most recent six in chronological order.
    let form: Vec<SharedString> = g
        .recent_form
        .iter()
        .rev()
        .take(6)
        .rev()
        .map(|s| SharedString::from(s.as_str()))
        .collect();

    YouStripData {
        has_stats: true,
        game_name: name.into(),
        short_name: short.into(),
        game_color: color,
        streak_text: streak_text.into(),
        streak_positive: streak >= 0,
        record_text: format!("{}W {}L", g.wins, g.losses).into(),
        win_rate_text: win_rate_text.into(),
        recent_form: ModelRc::new(VecModel::from(form)),
    }
}

fn parse_hex_color(hex: &str) -> Color {
    let hex = hex.trim_start_matches('#');
    let rgb = u32::from_str_radix(hex, 16).unwrap_or(0x888888);
    Color::from_argb_encoded(0xFF000000 | rgb)
}
