use mello_core::crew_events::UserGameStats;
use mello_core::game_db::GameDatabase;
use mello_core::Event;
use slint::{Color, ModelRc, SharedString, VecModel};

use crate::app_context::AppContext;
use crate::{GameStatsCardData, YouStripData};

pub fn handle(ctx: &AppContext, event: Event) {
    if let Event::UserGameStatsLoaded { games } = event {
        log::info!("[ui] user game stats loaded ({} games)", games.len());
        ctx.app.set_you_strip(build_you_strip(&games));
        ctx.app
            .set_stats_profile_games(ModelRc::new(VecModel::from(build_profile_cards(&games))));
    }
}

/// Resolve display name / short name / badge color from the bundled game DB,
/// falling back to the raw id for unknown games.
fn display_info(db: &GameDatabase, game_id: &str) -> (String, String, Color) {
    match db.lookup_by_id(game_id) {
        Some(e) => (
            e.name.clone(),
            e.short_name.clone(),
            parse_hex_color(e.color.as_deref().unwrap_or("#888888")),
        ),
        None => (
            game_id.to_string(),
            game_id.to_string(),
            parse_hex_color("#888888"),
        ),
    }
}

fn streak_text(streak: i32) -> String {
    if streak > 0 {
        format!("W{streak}")
    } else if streak < 0 {
        format!("L{}", -streak)
    } else {
        "—".to_string()
    }
}

fn win_rate_text(wins: u32, losses: u32) -> String {
    let decided = wins + losses;
    if decided > 0 {
        format!("{}%", (wins * 100) / decided)
    } else {
        "—".to_string()
    }
}

fn record_text(g: &UserGameStats) -> String {
    if g.draws > 0 {
        format!("{}W {}L {}D", g.wins, g.losses, g.draws)
    } else {
        format!("{}W {}L", g.wins, g.losses)
    }
}

/// Newest-last recent form, trimmed to the most recent `max` entries.
fn form_pips(g: &UserGameStats, max: usize) -> Vec<SharedString> {
    g.recent_form
        .iter()
        .rev()
        .take(max)
        .rev()
        .map(|s| SharedString::from(s.as_str()))
        .collect()
}

fn empty_strip() -> YouStripData {
    YouStripData {
        has_stats: false,
        game_id: SharedString::new(),
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
    let (name, short, color) = display_info(&db, &g.game_id);

    YouStripData {
        has_stats: true,
        game_id: g.game_id.as_str().into(),
        game_name: name.into(),
        short_name: short.into(),
        game_color: color,
        streak_text: streak_text(g.current_streak).into(),
        streak_positive: g.current_streak >= 0,
        record_text: format!("{}W {}L", g.wins, g.losses).into(),
        win_rate_text: win_rate_text(g.wins, g.losses).into(),
        recent_form: ModelRc::new(VecModel::from(form_pips(g, 6))),
    }
}

/// Per-game cards for the stats profile (spec 19 A2), one per tracked game,
/// in the backend's newest-played-first order.
fn build_profile_cards(games: &[UserGameStats]) -> Vec<GameStatsCardData> {
    let db = GameDatabase::load_bundled();
    games
        .iter()
        .filter(|g| g.wins + g.losses + g.draws > 0)
        .map(|g| {
            let (name, short, color) = display_info(&db, &g.game_id);

            let best = match (g.longest_win_streak, g.longest_loss_streak) {
                (0, 0) => String::new(),
                (w, 0) => format!("W{w} best"),
                (0, l) => format!("L{l} worst"),
                (w, l) => format!("W{w} best · L{l} worst"),
            };

            GameStatsCardData {
                game_id: g.game_id.as_str().into(),
                game_name: name.into(),
                short_name: short.into(),
                game_color: color,
                record_text: record_text(g).into(),
                win_rate_text: win_rate_text(g.wins, g.losses).into(),
                streak_text: streak_text(g.current_streak).into(),
                streak_positive: g.current_streak >= 0,
                best_streak_text: best.into(),
                recent_form: ModelRc::new(VecModel::from(form_pips(g, 10))),
                last_played_text: relative_time(g.last_played).into(),
            }
        })
        .collect()
}

/// Coarse relative timestamp for "last played" (ms since epoch; "" if unset).
fn relative_time(ts_ms: i64) -> String {
    if ts_ms <= 0 {
        return String::new();
    }
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let ago = now_secs - ts_ms / 1000;
    if ago < 60 {
        "just now".to_string()
    } else if ago < 3600 {
        format!("{}m ago", ago / 60)
    } else if ago < 86400 {
        format!("{}h ago", ago / 3600)
    } else if ago < 7 * 86400 {
        format!("{}d ago", ago / 86400)
    } else {
        format!("{}w ago", ago / (7 * 86400))
    }
}

fn parse_hex_color(hex: &str) -> Color {
    let hex = hex.trim_start_matches('#');
    let rgb = u32::from_str_radix(hex, 16).unwrap_or(0x888888);
    Color::from_argb_encoded(0xFF000000 | rgb)
}
