use mello_core::crew_events::UserGameStats;
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

/// Resolve display name / short name / badge colour for a stats row.
///
/// The catalogue covers curated and popular games. Anything else — a title
/// discovered in an installed library, or one nothing could name — falls back
/// to the raw id, which at least renders something legible rather than a blank
/// badge.
fn display_info(
    head: Option<&mello_core::catalogue::Head>,
    game_id: &str,
) -> (String, String, Color) {
    match head.and_then(|h| h.by_game_id(game_id)) {
        Some(e) => (
            e.name.to_string(),
            e.short_name.to_string(),
            parse_hex_color("#888888"),
        ),
        None => (
            game_id.to_string(),
            mello_core::library::derive_short_name(game_id),
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
        has_record: false,
        streak_text: SharedString::new(),
        streak_positive: true,
        record_text: SharedString::new(),
        win_rate_text: SharedString::new(),
        weekly_time_text: SharedString::new(),
        recent_form: ModelRc::new(VecModel::from(Vec::<SharedString>::new())),
    }
}

/// Days of the rolling window the strip summarises.
const WEEK_DAYS: usize = 7;

/// Minutes played this week, as (wall, active).
///
/// The backend keeps a rolling day list, oldest first, so the last seven
/// entries are the week. Both figures are returned because the copy rule picks
/// between them rather than always trusting wall time.
fn weekly_minutes(g: &UserGameStats) -> (u32, u32) {
    g.recent_days
        .iter()
        .rev()
        .take(WEEK_DAYS)
        .fold((0, 0), |(wall, active), d| {
            (wall + d.wall_min, active + d.active_min)
        })
}

/// "4h 12m this week", or empty when nothing was played.
///
/// Wall time is the headline, except when a game was clearly left running:
/// if it held the foreground for under a third of its wall time, the active
/// figure is shown instead. An inflated number a crewmate can disprove costs
/// more than the larger number gains.
fn weekly_time_text(g: &UserGameStats) -> String {
    let (wall, active) = weekly_minutes(g);
    if wall == 0 && active == 0 {
        return String::new();
    }
    let shown = if active * 3 < wall { active } else { wall };
    if shown == 0 {
        return String::new();
    }
    let (h, m) = (shown / 60, shown % 60);
    let duration = if h > 0 && m > 0 {
        format!("{h}h {m}m")
    } else if h > 0 {
        format!("{h}h")
    } else {
        format!("{m}m")
    };
    format!("{duration} this week")
}

fn build_you_strip(games: &[UserGameStats]) -> YouStripData {
    // The backend returns games newest-played first, so the first entry is
    // what the person was last doing. This used to pick the first game with a
    // win/loss record, which meant the strip stayed empty — or showed a stale
    // game — for anyone whose games have no telemetry adapter. That is most
    // games and, for most people, all of them.
    let Some(g) = games.first() else {
        return empty_strip();
    };

    let head = mello_core::catalogue::Head::bundled();
    let (name, short, color) = display_info(head.as_ref(), &g.game_id);
    let has_record = g.wins + g.losses + g.draws > 0;

    YouStripData {
        has_stats: true,
        game_id: g.game_id.as_str().into(),
        game_name: name.into(),
        short_name: short.into(),
        game_color: color,
        has_record,
        streak_text: streak_text(g.current_streak).into(),
        streak_positive: g.current_streak >= 0,
        record_text: format!("{}W {}L", g.wins, g.losses).into(),
        win_rate_text: win_rate_text(g.wins, g.losses).into(),
        weekly_time_text: weekly_time_text(g).into(),
        recent_form: ModelRc::new(VecModel::from(form_pips(g, 6))),
    }
}

/// Per-game cards for the stats profile (spec 19 A2), one per tracked game,
/// in the backend's newest-played-first order.
fn build_profile_cards(games: &[UserGameStats]) -> Vec<GameStatsCardData> {
    let head = mello_core::catalogue::Head::bundled();
    games
        .iter()
        .filter(|g| g.wins + g.losses + g.draws > 0)
        .map(|g| {
            let (name, short, color) = display_info(head.as_ref(), &g.game_id);

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

#[cfg(test)]
mod tests {
    use super::*;
    use mello_core::crew_events::RecentDayEntry;

    fn day(date: &str, wall: u32, active: u32) -> RecentDayEntry {
        RecentDayEntry {
            date: date.to_string(),
            wall_min: wall,
            active_min: active,
        }
    }

    fn game(id: &str, days: Vec<RecentDayEntry>) -> UserGameStats {
        UserGameStats {
            game_id: id.to_string(),
            recent_days: days,
            ..Default::default()
        }
    }

    #[test]
    fn weekly_time_reads_as_hours_and_minutes() {
        let g = game(
            "x",
            vec![day("2026-08-20", 180, 180), day("2026-08-21", 72, 72)],
        );
        assert_eq!(weekly_time_text(&g), "4h 12m this week");
    }

    #[test]
    fn whole_hours_and_bare_minutes_drop_the_empty_half() {
        let g = game("x", vec![day("2026-08-21", 120, 120)]);
        assert_eq!(weekly_time_text(&g), "2h this week");
        let g = game("x", vec![day("2026-08-21", 45, 45)]);
        assert_eq!(weekly_time_text(&g), "45m this week");
    }

    #[test]
    fn a_game_left_running_reports_the_active_figure() {
        // Nine hours of wall time against twenty minutes of foreground is a
        // game left open overnight. Claiming nine hours is a number a crewmate
        // can disprove, which costs more than the bigger figure gains.
        let g = game("x", vec![day("2026-08-21", 540, 20)]);
        assert_eq!(weekly_time_text(&g), "20m this week");
    }

    #[test]
    fn ordinary_play_keeps_wall_time() {
        // Foreground above a third of wall time is normal play — alt-tabbing,
        // a menu left open — and must not be trimmed.
        let g = game("x", vec![day("2026-08-21", 180, 100)]);
        assert_eq!(weekly_time_text(&g), "3h this week");
    }

    #[test]
    fn only_the_last_seven_days_count() {
        // The backend keeps eight days so the window can roll; the strip says
        // "this week" and must mean it.
        let mut days: Vec<RecentDayEntry> = (1..=8)
            .map(|i| day(&format!("2026-08-{i:02}"), 60, 60))
            .collect();
        assert_eq!(days.len(), 8);
        let g = game("x", days.clone());
        assert_eq!(weekly_time_text(&g), "7h this week");
        days.truncate(7);
        assert_eq!(weekly_time_text(&game("x", days)), "7h this week");
    }

    #[test]
    fn no_play_yields_no_copy() {
        assert_eq!(weekly_time_text(&game("x", vec![])), "");
        assert_eq!(
            weekly_time_text(&game("x", vec![day("2026-08-21", 0, 0)])),
            ""
        );
    }

    #[test]
    fn the_strip_shows_the_most_recent_game_even_without_a_record() {
        // This is the whole point of the change: picking by record left the
        // strip empty for anyone whose games have no telemetry adapter, which
        // is most games.
        let recent = game("steam-2300340", vec![day("2026-08-21", 90, 85)]);
        let older = UserGameStats {
            game_id: "counter-strike-2".to_string(),
            wins: 5,
            losses: 3,
            ..Default::default()
        };
        let strip = build_you_strip(&[recent, older]);
        assert!(strip.has_stats);
        assert_eq!(strip.game_id, "steam-2300340");
        assert!(!strip.has_record, "no W/L means the hours line carries it");
        assert_eq!(strip.weekly_time_text, "1h 30m this week");
    }

    #[test]
    fn a_game_with_a_record_still_reports_one() {
        let g = UserGameStats {
            game_id: "counter-strike-2".to_string(),
            wins: 5,
            losses: 3,
            ..Default::default()
        };
        let strip = build_you_strip(&[g]);
        assert!(strip.has_record);
        assert_eq!(strip.record_text, "5W 3L");
    }

    #[test]
    fn no_games_hides_the_strip() {
        assert!(!build_you_strip(&[]).has_stats);
    }
}
