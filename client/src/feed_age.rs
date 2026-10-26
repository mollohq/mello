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

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// The day and month of an instant, UTC: "2 October". Cards older than a
/// week carry it (frame F), where "9d ago" counted for nothing.
pub fn date_label(ms: i64) -> String {
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = ms.div_euclid(DAY_MS) + 719_468;
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{day} {}", MONTHS[(month - 1) as usize])
}

/// The day, the month and the year of an instant, UTC, as numbers.
fn civil(ms: i64) -> (i64, usize, i64) {
    let z = ms.div_euclid(DAY_MS) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (day, (month - 1) as usize, year)
}

/// The days of a recap week: "12 to 18 October", or "28 September to 4
/// October" across a month. `week_end_ms` is the Monday that ends the week, so
/// the last day is the one before it. Empty without a time.
pub fn week_range(week_start_ms: i64, week_end_ms: i64) -> String {
    if week_start_ms <= 0 || week_end_ms <= week_start_ms {
        return String::new();
    }
    let (d0, m0, _) = civil(week_start_ms);
    let (d1, m1, _) = civil(week_end_ms - 1);
    if m0 == m1 {
        format!("{d0} to {d1} {}", MONTHS[m1])
    } else {
        format!("{d0} {} to {d1} {}", MONTHS[m0], MONTHS[m1])
    }
}

/// How long ago, plainly: "Today", "Yesterday", "2 days ago", "Last week",
/// "3 weeks ago". By UTC day. Empty without a time.
pub fn ago_label(then_ms: i64, now_ms: i64) -> String {
    if then_ms <= 0 {
        return String::new();
    }
    match days_between(then_ms, now_ms) {
        0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        d @ 2..=6 => format!("{d} days ago"),
        7..=13 => "Last week".to_string(),
        d => format!("{} weeks ago", d / 7),
    }
}

/// The day of a daily recap, from the UTC start of that day: "Today so far"
/// for the day that is not over, "Yesterday", then the day name ("Tuesday").
pub fn day_label(day_start_ms: i64, now_ms: i64) -> String {
    if day_start_ms <= 0 {
        return String::new();
    }
    match days_between(day_start_ms, now_ms) {
        0 => "Today so far".to_string(),
        1 => "Yesterday".to_string(),
        _ => weekday_of(day_start_ms).to_string(),
    }
}

/// How old a weekly recap is, at the right of its label: "Last week", then
/// "3 weeks ago". The feed keeps the last real recap through a quiet spell, so
/// this label is what tells an old recap from a new one (frame F). A recap
/// from the current week has no label: the day it came out is a note, not
/// data. `week_end_ms` is the end of the recap week, the Monday it came out.
/// Empty when the recap has no time.
pub fn recap_age_label(week_end_ms: i64, now_ms: i64) -> String {
    if week_end_ms <= 0 {
        return String::new();
    }
    match days_between(week_end_ms, now_ms) {
        0..=6 => String::new(),
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
    fn a_week_reads_as_its_first_and_last_day() {
        // Monday 12 October 2026 to the Monday after.
        let start = MON_5_OCT + 7 * DAY;
        assert_eq!(week_range(start, start + 7 * DAY), "12 to 18 October");
        // Monday 28 September to Sunday 4 October.
        let start = MON_5_OCT - 7 * DAY;
        assert_eq!(
            week_range(start, start + 7 * DAY),
            "28 September to 4 October"
        );
        assert_eq!(week_range(0, 0), "");
    }

    #[test]
    fn the_weekday_is_right() {
        assert_eq!(weekday_of(MON_5_OCT), "Monday");
        assert_eq!(weekday_of(MON_5_OCT + 3 * DAY), "Thursday");
        assert_eq!(weekday_of(0), "Thursday");
    }

    #[test]
    fn ago_reads_as_days_then_weeks() {
        let now = MON_5_OCT + 3_600_000;
        assert_eq!(ago_label(now - 1_000, now), "Today");
        assert_eq!(ago_label(MON_5_OCT - DAY, now), "Yesterday");
        assert_eq!(ago_label(MON_5_OCT - 2 * DAY, now), "2 days ago");
        assert_eq!(ago_label(MON_5_OCT - 8 * DAY, now), "Last week");
        assert_eq!(ago_label(MON_5_OCT - 24 * DAY, now), "3 weeks ago");
        assert_eq!(ago_label(0, now), "");
    }

    #[test]
    fn a_date_reads_as_day_and_month() {
        assert_eq!(date_label(MON_5_OCT), "5 October");
        assert_eq!(date_label(MON_5_OCT - 3 * DAY), "2 October");
        assert_eq!(date_label(MON_5_OCT - 6 * DAY), "29 September");
        assert_eq!(date_label(0), "1 January");
        // 29 February 2028.
        assert_eq!(date_label(21_243 * DAY), "29 February");
    }

    #[test]
    fn a_day_reads_as_today_so_far_yesterday_then_the_day_name() {
        let now = MON_5_OCT + 15 * 3_600_000;
        assert_eq!(day_label(MON_5_OCT, now), "Today so far");
        assert_eq!(day_label(MON_5_OCT - DAY, now), "Yesterday");
        assert_eq!(day_label(MON_5_OCT - 2 * DAY, now), "Saturday");
        assert_eq!(day_label(0, now), "");
    }

    #[test]
    fn a_recap_has_no_label_in_its_week_then_reads_as_weeks() {
        let came = MON_5_OCT;
        assert_eq!(recap_age_label(came, came + 3_600_000), "");
        // Frame D: Thursday, the recap came out on Monday.
        assert_eq!(recap_age_label(came, came + 3 * DAY), "");
        assert_eq!(recap_age_label(came, came + 8 * DAY), "Last week");
        // Frame F: Monday 26 October, three weeks on.
        assert_eq!(recap_age_label(came, came + 21 * DAY), "3 weeks ago");
        assert_eq!(recap_age_label(0, came), "");
    }
}
