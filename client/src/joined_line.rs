//! The joined line at the top of the crew feed (CLIPS 7, mockup frame B2): one
//! slim row for each member who joined in the last 24 hours, newest first, at
//! most three. The rows come from the `joined` list of the feed response, which
//! the server reads from the `member_joined` events of the crew ledger.
//!
//! Pure functions, so the rule needs no window.

use mello_core::crew_events::FeedJoin;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// The most rows the line shows.
pub const MAX_ROWS: usize = 3;

/// One row as the UI draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub user_id: String,
    pub line: String,
    pub when: String,
}

/// "Jonas joined, invited by b0bben", or "Jonas joined" when nobody invited.
pub fn line(name: &str, invited_by: &str) -> String {
    let name = if name.trim().is_empty() {
        "A new member"
    } else {
        name.trim()
    };
    if invited_by.trim().is_empty() {
        format!("{name} joined")
    } else {
        format!("{name} joined, invited by {}", invited_by.trim())
    }
}

/// The time of the join on the viewer's clock: "Today 23:14" or "Yesterday
/// 23:14". `tz_offset_min` is the local offset, east positive. A join inside
/// the 24 h window is on one of those two days.
pub fn when_label(ts_ms: i64, now_ms: i64, tz_offset_min: i32) -> String {
    let local = |ms: i64| ms + i64::from(tz_offset_min) * 60_000;
    let day = |ms: i64| local(ms).div_euclid(DAY_MS);
    let in_day = local(ts_ms).rem_euclid(DAY_MS) / 60_000;
    let clock = format!("{:02}:{:02}", in_day / 60, in_day % 60);
    let word = if day(ts_ms) >= day(now_ms) {
        "Today"
    } else {
        "Yesterday"
    };
    format!("{word} {clock}")
}

/// The rows to draw. Joins older than 24 h, the viewer's own join and repeat
/// joiners are dropped, then the list is sorted newest first and cut to three.
/// `name_of` gives the current name of a member from the roster; the name the
/// event stored is the fallback.
pub fn rows(
    joins: &[FeedJoin],
    viewer_id: &str,
    now_ms: i64,
    tz_offset_min: i32,
    name_of: &dyn Fn(&str) -> Option<String>,
) -> Vec<Row> {
    let mut sorted: Vec<&FeedJoin> = joins
        .iter()
        .filter(|j| !j.user_id.is_empty() && j.user_id != viewer_id)
        .filter(|j| j.ts <= now_ms && now_ms - j.ts < DAY_MS)
        .collect();
    sorted.sort_by_key(|j| std::cmp::Reverse(j.ts));
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for j in sorted {
        if seen.contains(&j.user_id.as_str()) {
            continue;
        }
        seen.push(&j.user_id);
        let name = name_of(&j.user_id).unwrap_or_else(|| j.name.clone());
        let inviter = if j.invited_by.is_empty() {
            String::new()
        } else {
            name_of(&j.invited_by).unwrap_or_else(|| j.invited_by_name.clone())
        };
        out.push(Row {
            user_id: j.user_id.clone(),
            line: line(&name, &inviter),
            when: when_label(j.ts, now_ms, tz_offset_min),
        });
        if out.len() == MAX_ROWS {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600_000;
    // 2026-10-24 23:30 UTC.
    const NOW: i64 = 1_792_884_600_000;

    fn join(id: &str, name: &str, by: &str, ago: i64) -> FeedJoin {
        FeedJoin {
            user_id: id.into(),
            name: name.into(),
            invited_by: if by.is_empty() {
                String::new()
            } else {
                "bob".into()
            },
            invited_by_name: by.into(),
            ts: NOW - ago,
        }
    }
    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn the_line_names_the_inviter_when_there_is_one() {
        assert_eq!(line("Jonas", "b0bben"), "Jonas joined, invited by b0bben");
        assert_eq!(line("Jonas", ""), "Jonas joined");
        assert_eq!(line("", ""), "A new member joined");
    }

    #[test]
    fn the_time_reads_on_the_viewers_clock() {
        // 23:14 UTC, an hour ago in UTC.
        let ts = NOW - 16 * 60_000;
        assert_eq!(when_label(ts, NOW, 0), "Today 23:14");
        // Sweden in winter: 00:14 the next day, and now is 00:30, same day.
        assert_eq!(when_label(ts, NOW, 60), "Today 00:14");
        // Five hours ago on a UTC clock is the afternoon of the same day.
        assert_eq!(when_label(NOW - 5 * HOUR, NOW, 0), "Today 18:30");
        // At 01:30, three hours ago was 22:30 the evening before.
        let early = NOW - 22 * HOUR;
        assert_eq!(when_label(early - 3 * HOUR, early, 0), "Yesterday 22:30");
    }

    #[test]
    fn a_join_shows_for_24_hours_newest_first_and_three_at_most() {
        let joins = vec![
            join("a", "Ann", "", 5 * HOUR),
            join("b", "Jonas", "b0bben", HOUR),
            join("c", "Cy", "", 23 * HOUR),
            join("d", "Old", "", 25 * HOUR),
            join("e", "Eli", "", 2 * HOUR),
        ];
        let r = rows(&joins, "me", NOW, 0, &none);
        let ids: Vec<&str> = r.iter().map(|r| r.user_id.as_str()).collect();
        assert_eq!(
            ids,
            ["b", "e", "a"],
            "three, newest first, none older than 24 h"
        );
        assert_eq!(r[0].line, "Jonas joined, invited by b0bben");
    }

    #[test]
    fn the_viewer_and_repeat_joiners_do_not_show() {
        let joins = vec![
            join("me", "Me", "", HOUR),
            join("a", "Ann", "", 2 * HOUR),
            join("a", "Ann", "", 3 * HOUR),
        ];
        let r = rows(&joins, "me", NOW, 0, &none);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].user_id, "a");
        assert!(rows(&[], "me", NOW, 0, &none).is_empty());
    }

    #[test]
    fn the_roster_name_wins_over_the_stored_one() {
        let joins = vec![join("a", "old name", "b0bben", HOUR)];
        let roster = |id: &str| match id {
            "a" => Some("Jonas".to_string()),
            "bob" => Some("Bob".to_string()),
            _ => None,
        };
        assert_eq!(
            rows(&joins, "me", NOW, 0, &roster)[0].line,
            "Jonas joined, invited by Bob"
        );
    }
}
