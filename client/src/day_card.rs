//! The day card: a daily recap with the sessions of its day listed beside its
//! numbers (spec 19 section 3.5a, mockup next-gen-feed.html frame H). A voice
//! session is a row here, never a card. Pure: the handler maps the rows to
//! Slint.

use serde_json::Value;

/// How many session rows the wide day card draws.
pub const ROWS: usize = 2;
/// How many octagons a row draws before "+N".
pub const ROW_PEOPLE: usize = 3;

/// One session of the day, as the server sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayRow {
    pub kind: String,
    pub title: String,
    pub member_ids: Vec<String>,
    pub duration_min: i64,
    pub peak: i64,
    pub end_ms: i64,
}

/// The sessions of a daily recap entry, in the server's order (the session
/// that ended last first). Empty for an older server.
pub fn rows(data: &Value) -> Vec<DayRow> {
    data.get("sessions")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .map(|s| DayRow {
            kind: str_of(s, "kind"),
            title: str_of(s, "title"),
            member_ids: s
                .get("member_ids")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            duration_min: int_of(s, "duration_min"),
            peak: int_of(s, "peak"),
            end_ms: int_of(s, "end_ms"),
        })
        .collect()
}

/// The day card is two columns wide when it has a session to list. An older
/// server sends no sessions: the card stays a cell.
pub fn is_wide(data: &Value) -> bool {
    !rows(data).is_empty()
}

/// A voice session card from an older server. The server leaves voice
/// sessions out of `this_week`; the client draws none either way.
pub fn is_voice_session_card(entry_type: &str, backend_type: &str) -> bool {
    entry_type == "session" && backend_type == "voice_session"
}

/// The line under a row's title. Voice: "Voice, 3h 58m, 5 in the call".
/// Stream: "Stream, 54m, 3 watching". Game: "2h 40m, stromvap". A row with no
/// minutes has no line.
pub fn meta_line(kind: &str, duration_min: i64, peak: i64, first_name: &str) -> String {
    let d = duration(duration_min);
    match kind {
        "voice" => match peak {
            n if n > 1 => format!("Voice, {d}, {n} in the call"),
            _ => format!("Voice, {d}"),
        },
        "stream" => match peak {
            1 => format!("Stream, {d}, 1 watching"),
            n if n > 1 => format!("Stream, {d}, {n} watching"),
            _ => format!("Stream, {d}"),
        },
        _ if first_name.is_empty() => d,
        _ => format!("{d}, {first_name}"),
    }
}

/// The row's title: the channel, the game, or a plain word for a stream of
/// no known game. Never a window title.
pub fn title(kind: &str, title: &str) -> String {
    let t = title.trim();
    if !t.is_empty() {
        return t.to_string();
    }
    match kind {
        "voice" => "Voice".to_string(),
        "stream" => "Screen".to_string(),
        _ => "Game".to_string(),
    }
}

/// When the session started, on the viewer's clock: "20:12".
pub fn clock(end_ms: i64, duration_min: i64, tz_offset_min: i32) -> String {
    if end_ms <= 0 {
        return String::new();
    }
    const DAY_MS: i64 = 86_400_000;
    let start = end_ms - duration_min.max(0) * 60_000 + i64::from(tz_offset_min) * 60_000;
    let in_day = start.rem_euclid(DAY_MS) / 60_000;
    format!("{:02}:{:02}", in_day / 60, in_day % 60)
}

/// "3h 58m", "54m". Empty for no minutes.
fn duration(mins: i64) -> String {
    match mins {
        m if m <= 0 => String::new(),
        m if m >= 60 => format!("{}h {}m", m / 60, m % 60),
        m => format!("{m}m"),
    }
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

fn int_of(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DAY: i64 = 86_400_000;

    fn day(sessions: Value) -> Value {
        json!({"day_start": 10 * DAY, "day_end": 11 * DAY, "sessions": sessions})
    }

    #[test]
    fn a_day_with_sessions_is_wide_today_included() {
        let d = day(
            json!([{"kind": "voice", "title": "General", "member_ids": ["a"], "duration_min": 30, "peak": 2, "end_ms": 10 * DAY + 1000}]),
        );
        assert!(is_wide(&d));
        assert!(!is_wide(&day(json!([]))), "nothing to list");
        assert!(!is_wide(&json!({"day_start": 1})), "an older server");
    }

    #[test]
    fn a_voice_session_is_never_a_card() {
        assert!(is_voice_session_card("session", "voice_session"));
        assert!(!is_voice_session_card("session", "stream_session"));
        assert!(!is_voice_session_card("session", "game_session"));
        assert!(!is_voice_session_card("clip", "voice_session"));
    }

    #[test]
    fn rows_read_the_server_shape_and_survive_an_older_one() {
        let d = day(json!([
            {"kind": "stream", "title": "Counter-Strike 2", "member_ids": ["h", "v"], "duration_min": 54, "peak": 3, "end_ms": 5},
            {"kind": "game", "title": "Valorant", "member_ids": [], "duration_min": 45, "end_ms": 4},
        ]));
        let r = rows(&d);
        assert_eq!(r.len(), 2);
        assert_eq!(
            (r[0].kind.as_str(), r[0].peak, r[0].member_ids.len()),
            ("stream", 3, 2)
        );
        assert_eq!((r[1].kind.as_str(), r[1].peak), ("game", 0));
        assert!(rows(&json!({"day_start": 1})).is_empty());
    }

    #[test]
    fn the_meta_line_names_the_kind_the_length_and_the_people() {
        assert_eq!(
            meta_line("voice", 238, 5, ""),
            "Voice, 3h 58m, 5 in the call"
        );
        assert_eq!(meta_line("voice", 20, 1, "bob"), "Voice, 20m");
        assert_eq!(meta_line("stream", 54, 3, "bob"), "Stream, 54m, 3 watching");
        assert_eq!(meta_line("stream", 54, 1, ""), "Stream, 54m, 1 watching");
        assert_eq!(meta_line("game", 160, 0, "stromvap"), "2h 40m, stromvap");
        assert_eq!(meta_line("game", 45, 0, ""), "45m");
    }

    #[test]
    fn a_stream_row_never_shows_a_window_title() {
        assert_eq!(title("stream", ""), "Screen");
        assert_eq!(title("stream", "Counter-Strike 2"), "Counter-Strike 2");
        assert_eq!(title("voice", "  "), "Voice");
    }

    #[test]
    fn the_clock_is_the_start_on_the_viewers_clock() {
        // Ended 23:50 UTC after 3h 58m: started 19:52 UTC, 20:52 at UTC+1.
        let end = 10 * DAY + (23 * 60 + 50) * 60_000;
        assert_eq!(clock(end, 238, 60), "20:52");
        assert_eq!(clock(end, 238, 0), "19:52");
        // Starts the day before on the viewer's clock: still a clock, no date.
        assert_eq!(clock(10 * DAY + 30 * 60_000, 60, 0), "23:30");
        assert_eq!(clock(0, 10, 0), "");
    }
}
