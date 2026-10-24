//! Recent games card: the text and the art of each tile. Plan
//! `next-gen-feed.md`, spec 22 section 3.4, mockup frame A.
//!
//! The pure functions here have no window, so tests need none. The handler in
//! `handlers/clip.rs` calls them and fills the Slint structs.

use std::collections::HashSet;

use crate::head2head::grouped;

/// Hours as the mockup writes them, uppercase: "4,167 H" from 100 hours up,
/// one decimal below ("12.6 H"). No minutes gives the launcher name
/// ("EPIC"), or nothing when even that is unknown.
pub fn hours_label(minutes: i64, launcher: &str) -> String {
    if minutes <= 0 {
        return launcher.trim().to_uppercase();
    }
    let hours = minutes as f64 / 60.0;
    if hours >= 100.0 {
        format!("{} H", grouped(hours.round() as i64))
    } else {
        format!("{:.1} H", hours.max(0.1))
    }
}

/// When a game was last played, as the poster grid writes it under the name
/// (frames C and D): "Today", "Yesterday", "3 days ago", "Last week",
/// "3 weeks ago". Days are UTC days, as the feed timestamps are. Empty when the
/// server has no time.
pub fn when_label(last_played_ms: i64, now_ms: i64) -> String {
    if last_played_ms <= 0 {
        return String::new();
    }
    let day = |ms: i64| ms.div_euclid(86_400_000);
    let days = (day(now_ms) - day(last_played_ms)).max(0);
    match days {
        0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        2..=6 => format!("{days} days ago"),
        7..=13 => "Last week".to_string(),
        _ => format!("{} weeks ago", days / 7),
    }
}

/// The launcher of a tile: the server's field, else the prefix of a launcher
/// game id (`epic-fortnite`). A catalogue id such as `valorant` has none.
pub fn launcher_of(game_id: &str, launcher: &str) -> String {
    if !launcher.trim().is_empty() {
        return launcher.trim().to_string();
    }
    match game_id.split_once('-') {
        Some((p @ ("steam" | "epic" | "gog"), _)) => p.to_string(),
        _ => String::new(),
    }
}

/// Two letters for the fallback tile: the first letters of the first two words
/// ("Counter-Strike 2" gives "CS"), or the first two letters of a single
/// word. Letters and digits only. Never empty for a non-empty name.
pub fn monogram(name: &str) -> String {
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let letters: String = match words.as_slice() {
        [] => String::new(),
        [one] => one.chars().take(2).collect(),
        [a, b, ..] => a.chars().take(1).chain(b.chars().take(1)).collect(),
    };
    letters.to_uppercase()
}

/// One of the five identity hues for a game, stable for the same id.
pub fn hue_index(game_id: &str, name: &str) -> i32 {
    let key = if game_id.is_empty() { name } else { game_id };
    // FNV-1a: the hue must not change between runs, so no std hasher.
    let mut h: u32 = 0x811c_9dc5;
    for b in key.to_lowercase().bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    (h % 5) as i32
}

/// Whether a tile is installed on this device: its id is in the viewer's own
/// library, or its name matches an installed game (sessions carry catalogue
/// ids, the library carries launcher ids).
pub fn is_installed(
    game_id: &str,
    name: &str,
    installed_ids: &HashSet<String>,
    installed_names: &HashSet<String>,
) -> bool {
    (!game_id.is_empty() && installed_ids.contains(game_id))
        || (!name.is_empty() && installed_names.contains(&name.trim().to_lowercase()))
}

/// How many tiles the poster grid shows in its one row.
pub const GRID_TILES: usize = 5;

/// Whether the card is the poster grid: two or more members own the games.
/// With one PC the card is the list (mockup frame A).
pub fn is_grid(pc_count: i64) -> bool {
    pc_count >= 2
}

/// The PCs behind the card: the server's count, else the distinct members
/// across the tiles. A server that does not send `pc_count` still gives the
/// right shape.
pub fn pc_count_of(server: Option<i64>, owners_per_tile: &[Vec<String>]) -> i64 {
    if let Some(n) = server.filter(|n| *n > 0) {
        return n;
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for tile in owners_per_tile {
        seen.extend(tile.iter().map(String::as_str));
    }
    seen.len() as i64
}

/// The right side of the card header. "From 2 PCs" for the grid of a crew of
/// two (mockup frame B2). From three PCs the week's sessions line says more
/// than a count. With one PC: "From this PC" when every tile is installed on
/// this device, else the sessions line the server built.
pub fn header_note(all_installed: bool, sessions_line: String, pc_count: i64) -> String {
    if pc_count == 2 {
        "From 2 PCs".to_string()
    } else if pc_count > 2 {
        sessions_line
    } else if all_installed {
        "From this PC".to_string()
    } else {
        sessions_line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn when_reads_as_days_then_weeks() {
        let day = 86_400_000;
        let now = 100 * day + 3_600_000;
        assert_eq!(when_label(now - 1_000, now), "Today");
        assert_eq!(when_label(99 * day, now), "Yesterday");
        assert_eq!(when_label(96 * day, now), "4 days ago");
        assert_eq!(when_label(92 * day, now), "Last week");
        assert_eq!(when_label(79 * day, now), "3 weeks ago");
        assert_eq!(when_label(0, now), "", "no time from the server");
    }

    #[test]
    fn hours_read_like_the_mockup() {
        assert_eq!(hours_label(249_008, ""), "4,150 H");
        assert_eq!(hours_label(250_020, "steam"), "4,167 H");
        assert_eq!(hours_label(756, ""), "12.6 H");
        assert_eq!(hours_label(6_000, ""), "100 H");
        assert_eq!(hours_label(5_940, ""), "99.0 H");
        assert_eq!(hours_label(2, ""), "0.1 H");
    }

    #[test]
    fn unknown_hours_show_the_launcher_in_capitals() {
        assert_eq!(hours_label(0, "epic"), "EPIC");
        assert_eq!(hours_label(0, ""), "");
        assert_eq!(
            hours_label(60, "epic"),
            "1.0 H",
            "hours win over the launcher"
        );
    }

    #[test]
    fn the_launcher_comes_from_the_field_or_the_id_prefix() {
        assert_eq!(launcher_of("epic-rocketleague", ""), "epic");
        assert_eq!(launcher_of("steam-730", ""), "steam");
        assert_eq!(launcher_of("valorant", ""), "");
        assert_eq!(launcher_of("valorant", "gog"), "gog");
        assert_eq!(launcher_of("counter-strike-2", ""), "");
    }

    #[test]
    fn the_monogram_is_two_letters_never_the_name() {
        assert_eq!(monogram("Counter-Strike 2"), "CS");
        assert_eq!(monogram("Night Stones"), "NS");
        assert_eq!(monogram("Brawlhalla"), "BR");
        assert_eq!(monogram("Rocket League"), "RL");
        assert_eq!(monogram("X"), "X");
        assert_eq!(monogram("  "), "");
        assert_eq!(monogram("ärlig Öl"), "ÄÖ");
    }

    #[test]
    fn a_game_keeps_its_hue() {
        assert_eq!(hue_index("steam-730", "x"), hue_index("steam-730", "y"));
        assert!((0..5).contains(&hue_index("steam-730", "")));
        assert!((0..5).contains(&hue_index("", "Some Game")));
        let hues: HashSet<i32> = ["a", "b", "c", "d", "e", "f", "g", "h"]
            .iter()
            .map(|k| hue_index(k, ""))
            .collect();
        assert!(hues.len() > 1, "ids spread over the hues");
    }

    #[test]
    fn installed_matches_by_id_or_by_name() {
        let ids: HashSet<String> = ["steam-730".to_string()].into();
        let names: HashSet<String> = ["counter-strike 2".to_string()].into();
        assert!(is_installed("steam-730", "", &ids, &names));
        assert!(is_installed(
            "counter-strike-2",
            "Counter-Strike 2",
            &ids,
            &names
        ));
        assert!(!is_installed("valorant", "Valorant", &ids, &names));
        assert!(!is_installed("", "", &ids, &names));
    }

    #[test]
    fn the_header_note_is_from_this_pc_only_when_all_are_installed() {
        assert_eq!(
            header_note(true, "14 sessions, 31 h this week".into(), 1),
            "From this PC"
        );
        assert_eq!(
            header_note(false, "14 sessions, 31 h this week".into(), 1),
            "14 sessions, 31 h this week"
        );
        assert_eq!(header_note(false, String::new(), 0), "");
    }

    #[test]
    fn two_pcs_read_from_2_pcs_and_three_or_more_read_the_week() {
        assert_eq!(
            header_note(true, "1 session, 1 h this week".into(), 2),
            "From 2 PCs"
        );
        assert_eq!(
            header_note(true, "5 sessions, 8 h this week".into(), 3),
            "5 sessions, 8 h this week"
        );
    }

    #[test]
    fn the_grid_needs_two_owners_and_the_count_falls_back_to_the_tiles() {
        assert!(!is_grid(0) && !is_grid(1));
        assert!(is_grid(2) && is_grid(5));
        let tiles = vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["b".to_string()],
        ];
        assert_eq!(pc_count_of(None, &tiles), 2);
        assert_eq!(pc_count_of(Some(0), &tiles), 2);
        assert_eq!(pc_count_of(Some(4), &tiles), 4);
        assert_eq!(pc_count_of(None, &[vec!["a".to_string()]]), 1);
        assert_eq!(pc_count_of(None, &[]), 0);
    }
}
