//! How old a card is, in the words the feed uses (mockup frames D and F).
//!
//! Pure functions, so the rules need no window.

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Days between two instants, by UTC day. Never negative.
pub fn days_between(then_ms: i64, now_ms: i64) -> i64 {
    (now_ms.div_euclid(DAY_MS) - then_ms.div_euclid(DAY_MS)).max(0)
}

/// The day name of an instant, UTC. 1970-01-01 was a Thursday.
pub fn weekday_of(ms: i64) -> &'static str {
    match (ms.div_euclid(DAY_MS) + 4).rem_euclid(7) {
        0 => "Sunday",
        1 => "Monday",
        2 => "Tuesday",
        3 => "Wednesday",
        4 => "Thursday",
        5 => "Friday",
        _ => "Saturday",
    }
}

/// When a weekly recap came out, at the right of its label: "Today" on the
/// day, the day name for the rest of the week ("Monday" on a Thursday), then
/// "Last week" and "3 weeks ago". The feed keeps the last real recap through a
/// quiet spell, so this label is what tells a week-old recap from a new one
/// (frame F). `week_end_ms` is the end of the recap week, the Monday it came out.
/// Empty when the recap has no time.
pub fn recap_age_label(week_end_ms: i64, now_ms: i64) -> String {
    if week_end_ms <= 0 {
        return String::new();
    }
    match days_between(week_end_ms, now_ms) {
        0 => "Today".to_string(),
        1..=6 => weekday_of(week_end_ms).to_string(),
        7..=13 => "Last week".to_string(),
        d => format!("{} weeks ago", d / 7),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = DAY_MS;
    // Monday 5 October 2026, 00:00 UTC.
    const MON_5_OCT: i64 = 20_731 * DAY;

    #[test]
    fn the_weekday_is_right() {
        assert_eq!(weekday_of(MON_5_OCT), "Monday");
        assert_eq!(weekday_of(MON_5_OCT + 3 * DAY), "Thursday");
        assert_eq!(weekday_of(0), "Thursday");
    }

    #[test]
    fn a_recap_reads_as_a_day_name_then_as_weeks() {
        let came = MON_5_OCT;
        assert_eq!(recap_age_label(came, came + 3_600_000), "Today");
        // Frame D: Thursday, the recap came out on Monday.
        assert_eq!(recap_age_label(came, came + 3 * DAY), "Monday");
        assert_eq!(recap_age_label(came, came + 8 * DAY), "Last week");
        // Frame F: Monday 26 October, three weeks on.
        assert_eq!(recap_age_label(came, came + 21 * DAY), "3 weeks ago");
        assert_eq!(recap_age_label(0, came), "");
    }
}
