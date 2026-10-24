//! The quiet row and the dormant strip of the crew feed (CLIPS 7.1, mockup
//! frames E and F).
//!
//! A feed with nothing new must not look broken. Two rules say what it shows:
//!
//! - **Quiet** (frame E). A crew had time together recently, but there is no
//!   live stream, no stream preview and no belt move in the last 24 hours, and
//!   no recap this week. A slim row takes the place of the recap and says so
//!   in one line, with the day the recap comes.
//! - **Dormant** (frame F). Nothing happened for two weeks. The feed keeps the
//!   last real cards and a catch-up strip says since when, and who holds the
//!   first belt.
//!
//! Pure functions over a few facts, so the rules need no window.

use crate::feed_age::{date_label, days_between};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// Nothing for this many days makes a crew dormant.
pub const DORMANT_DAYS: i64 = 14;
/// The day a weekly recap comes out.
pub const RECAP_DAY: &str = "Monday";

/// What the rules read from the feed.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub member_count: i32,
    pub now_ms: i64,
    pub live_stream: bool,
    /// Time of the newest stream preview, if the feed has one.
    pub newest_stream_ms: Option<i64>,
    /// Time of the newest belt move of any age, if a title ever moved.
    pub newest_belt_move_ms: Option<i64>,
    /// Time of the newest recap, if the feed has one.
    pub newest_recap_ms: Option<i64>,
    /// Who played a game in the last seven days, newest first.
    pub players_last_week: Vec<String>,
    /// Time of the newest session, clip or recap: the last thing that happened.
    pub last_activity_ms: Option<i64>,
    /// The first held belt in display order: the holder and the title.
    pub top_belt: Option<(String, String)>,
}

impl Facts {
    fn within_day(&self, t: Option<i64>) -> bool {
        t.is_some_and(|t| self.now_ms - t < DAY_MS)
    }

    fn days_since_activity(&self) -> Option<i64> {
        self.last_activity_ms.map(|t| days_between(t, self.now_ms))
    }

    /// No recap in the last seven days.
    fn no_recap_this_week(&self) -> bool {
        !self
            .newest_recap_ms
            .is_some_and(|t| days_between(t, self.now_ms) < 7)
    }
}

/// The quiet row: the line and the day the recap comes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuietRow {
    pub line: String,
    pub when: String,
}

/// "Bob", "Bob and Sara", "Bob, Sara and Max", "Bob, Sara and 2 more".
pub fn names_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a} and {b}"),
        [a, b, c] => format!("{a}, {b} and {c}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more", rest.len()),
    }
}

/// The quiet row, or `None` when the feed has something to say.
///
/// A crew of one is a cold start, which has its own skeletons. A dormant crew
/// has the strip instead.
pub fn quiet_row(f: &Facts) -> Option<QuietRow> {
    if f.member_count < 2 || f.live_stream {
        return None;
    }
    if f.within_day(f.newest_stream_ms) || f.within_day(f.newest_belt_move_ms) {
        return None;
    }
    if !f.no_recap_this_week() {
        return None;
    }
    // Something happened lately, or the crew is dormant.
    if f.days_since_activity()? >= DORMANT_DAYS {
        return None;
    }
    let mut line = "No crew time last week.".to_string();
    if !f.players_last_week.is_empty() {
        line.push_str(&format!(
            " {} still played.",
            names_list(&f.players_last_week)
        ));
    }
    Some(QuietRow {
        line,
        when: RECAP_DAY.to_string(),
    })
}

/// The dormant strip: "Nothing new since 2 October. Leo holds The Carry." and
/// how long ago that was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DormantStrip {
    pub text: String,
    pub age: String,
}

/// The dormant strip, or `None` while the crew is active.
pub fn dormant_strip(f: &Facts) -> Option<DormantStrip> {
    if f.member_count < 2 || f.live_stream {
        return None;
    }
    let last = f.last_activity_ms?;
    if days_between(last, f.now_ms) < DORMANT_DAYS {
        return None;
    }
    let mut text = format!("Nothing new since {}.", date_label(last));
    if let Some((holder, title)) = &f.top_belt {
        text.push_str(&format!(" {holder} holds {title}."));
    }
    Some(DormantStrip {
        text,
        age: weeks_ago(days_between(last, f.now_ms)),
    })
}

/// "3 weeks ago" for 21 to 27 days. Two weeks is the least a strip shows.
fn weeks_ago(days: i64) -> String {
    format!("{} weeks ago", (days / 7).max(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = DAY_MS;
    // Sunday 18 October 2026 noon UTC.
    const SUN_18_OCT: i64 = 20_744 * DAY + 12 * 3_600_000;

    fn quiet_facts() -> Facts {
        Facts {
            member_count: 4,
            now_ms: SUN_18_OCT,
            newest_belt_move_ms: Some(SUN_18_OCT - 2 * DAY),
            players_last_week: vec!["Bob".into(), "Sara".into()],
            last_activity_ms: Some(SUN_18_OCT - 2 * DAY),
            ..Default::default()
        }
    }

    #[test]
    fn frame_e_has_a_quiet_row() {
        let row = quiet_row(&quiet_facts()).expect("quiet");
        assert_eq!(
            row.line,
            "No crew time last week. Bob and Sara still played."
        );
        assert_eq!(row.when, "Monday");
    }

    #[test]
    fn nobody_playing_leaves_the_second_sentence_out() {
        let mut f = quiet_facts();
        f.players_last_week.clear();
        assert_eq!(
            quiet_row(&f).expect("quiet").line,
            "No crew time last week."
        );
    }

    #[test]
    fn each_of_the_four_things_ends_the_quiet() {
        let mut f = quiet_facts();
        f.live_stream = true;
        assert!(quiet_row(&f).is_none(), "a live stream");
        let mut f = quiet_facts();
        f.newest_stream_ms = Some(SUN_18_OCT - 3_600_000);
        assert!(quiet_row(&f).is_none(), "a stream in 24 h");
        let mut f = quiet_facts();
        f.newest_belt_move_ms = Some(SUN_18_OCT - 3_600_000);
        assert!(quiet_row(&f).is_none(), "a belt in 24 h");
        let mut f = quiet_facts();
        f.newest_recap_ms = Some(SUN_18_OCT - 4 * DAY);
        assert!(quiet_row(&f).is_none(), "a recap this week");
    }

    #[test]
    fn an_old_stream_or_belt_does_not_end_the_quiet() {
        let mut f = quiet_facts();
        f.newest_stream_ms = Some(SUN_18_OCT - 2 * DAY);
        f.newest_recap_ms = Some(SUN_18_OCT - 9 * DAY);
        assert!(quiet_row(&f).is_some());
    }

    #[test]
    fn a_crew_of_one_has_no_quiet_row() {
        let mut f = quiet_facts();
        f.member_count = 1;
        assert!(quiet_row(&f).is_none());
    }

    #[test]
    fn names_stop_at_two_and_a_count() {
        let n = |v: &[&str]| names_list(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(n(&["Bob"]), "Bob");
        assert_eq!(n(&["Bob", "Sara", "Max"]), "Bob, Sara and Max");
        assert_eq!(
            n(&["Bob", "Sara", "Max", "Leo", "Jo"]),
            "Bob, Sara and 3 more"
        );
    }

    fn dormant_facts() -> Facts {
        // Monday 26 October 2026. The last session was on 2 October.
        let now = 20_752 * DAY + 9 * 3_600_000;
        Facts {
            member_count: 4,
            now_ms: now,
            last_activity_ms: Some(now - 24 * DAY),
            newest_recap_ms: Some(now - 21 * DAY),
            top_belt: Some(("Leo".into(), "The Carry".into())),
            ..Default::default()
        }
    }

    #[test]
    fn frame_f_has_a_dormant_strip_and_no_quiet_row() {
        let f = dormant_facts();
        let strip = dormant_strip(&f).expect("dormant");
        assert_eq!(
            strip.text,
            "Nothing new since 2 October. Leo holds The Carry."
        );
        assert_eq!(strip.age, "3 weeks ago");
        assert!(quiet_row(&f).is_none(), "the strip stands in for the row");
    }

    #[test]
    fn a_crew_that_played_last_week_is_not_dormant() {
        assert!(dormant_strip(&quiet_facts()).is_none());
    }

    #[test]
    fn the_strip_needs_the_two_weeks() {
        let mut f = dormant_facts();
        f.last_activity_ms = Some(f.now_ms - 13 * DAY);
        assert!(dormant_strip(&f).is_none());
        f.last_activity_ms = Some(f.now_ms - 14 * DAY);
        assert_eq!(dormant_strip(&f).expect("dormant").age, "2 weeks ago");
    }
}
