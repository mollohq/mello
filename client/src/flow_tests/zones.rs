//! The zone feed (prototype, `handlers/zones.rs`): the feed the client already
//! has folds into Stage, Board, Clips, People and Games.

use super::*;

fn zones_on(h: &mut Harness, members: i32) {
    app_with_crew_of(h, members);
    h.app().set_feed_zones_on(true);
}

/// A big crew with eight clips: the board holds the belts, the clip row shows
/// three tiles (nothing on stage, so the row runs under the board) and counts
/// the rest, and the game sessions are a list.
#[test]
fn a_big_crew_folds_into_board_clips_and_games() {
    let mut h = Harness::new();
    zones_on(&mut h, 50);
    set_roster(
        &h,
        &[("user-1", "Test User"), ("u2", "Jonas"), ("u3", "Max")],
    );
    h.emit(Event::FeedLoaded {
        response: big_crew_feed(8),
    });
    let z = h.app().get_feed_zones();
    // No recap and nothing new: the quiet rule holds, so the board may be the
    // quiet row. Either way it shows and carries the belts.
    assert!(
        matches!(z.board.as_str(), "tabs" | "quiet"),
        "board: {}",
        z.board
    );
    assert!(z.titles.row_count() >= 1, "the belts are on the board");
    assert_eq!(
        z.clips.row_count(),
        8,
        "the carousel holds every clip of the week"
    );
    assert!(z.sessions.row_count() >= 1, "the sessions are a list");
}

/// A crew of one: the first five (four open seats) and the solo board. The
/// zone feed is the only feed drawn.
#[test]
fn a_crew_of_one_shows_the_first_five() {
    let mut h = Harness::new();
    zones_on(&mut h, 1);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(1, vec![], vec![], None)]),
    });
    let z = h.app().get_feed_zones();
    assert!(z.people, "the people zone shows");
    assert_eq!(z.open_seats, 4);
    assert_eq!(z.board.as_str(), "solo");
}

/// With the switch off nothing is built: the old feed is untouched.
#[test]
fn the_zone_feed_is_off_by_default() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 50);
    h.emit(Event::FeedLoaded {
        response: big_crew_feed(3),
    });
    assert_eq!(h.app().get_feed_zones().board.as_str(), "");
}

/// No card is out: the centre view of a sealed card does not exist. A centre
/// view made with no card sat at rest with an empty belt, and its scrim took
/// every click on the feed (production, 2026-11-05).
#[test]
fn the_zone_feed_has_no_centre_view_until_a_card_is_opened() {
    let mut h = Harness::new();
    zones_on(&mut h, 50);
    h.emit(Event::FeedLoaded {
        response: big_crew_feed(3),
    });
    assert!(
        h.controls_labelled("Close").is_empty(),
        "the centre view's scrim is on screen with no card in it"
    );
}

/// A clip older than 7 days is Crew history, not the carousel. Without
/// m3llo+ its week shows sealed, with the clip as a locked teaser.
#[test]
fn an_old_clip_is_a_sealed_week_of_crew_history() {
    let mut h = Harness::new();
    zones_on(&mut h, 50);
    let mut response = big_crew_feed(2);
    let mut old = clip_entry("old-clip");
    old.ts -= 20 * 24 * 60 * 60 * 1000;
    response.sections.push(FeedSection {
        id: "memory".into(),
        entries: vec![old],
    });
    h.emit(Event::FeedLoaded { response });
    let z = h.app().get_feed_zones();
    assert_eq!(
        z.clips.row_count(),
        2,
        "the old clip is not in the carousel"
    );
    assert_eq!(z.history.row_count(), 1, "one past week");
    let week = z.history.row_data(0).expect("a week");
    assert!(week.locked, "without m3llo+ the week is sealed");
    assert!(week.seal.sealed);
    assert_eq!(week.teasers.row_count(), 1, "the clip is a locked teaser");
    assert_eq!(week.clips.row_count(), 0, "no clip plays without m3llo+");
}

// ── The zones payload (crew_feed with zones: true) ─────────────────────────

fn zones_response(zones: serde_json::Value) -> FeedResponse {
    let mut r = feed_of(vec![]);
    r.zones = Some(serde_json::from_value(zones).expect("a zones payload"));
    r
}

fn clip_json(id: &str, kind: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id, "type": "clip", "ts": 1_700_000_000_000_i64,
        "data": {"event_id": id, "clip_id": id, "clip_type": kind, "clipper_name": "Max",
                 "duration_seconds": 30.0, "media_url": format!("https://media.example/{id}.mp4")}
    })
}

/// The server's zones drive the feed: the stage clip, the clip carousel,
/// the sessions, the board mode and the open seats come from the payload.
#[test]
fn the_zones_payload_drives_every_zone() {
    let mut h = Harness::new();
    zones_on(&mut h, 3);
    h.emit(Event::FeedLoaded {
        response: zones_response(serde_json::json!({
            "free_days": 7,
            "stage": {"kind": "game_clip", "entry": clip_json("hero", "game")},
            "board": {"mode": "tabs"},
            "clips": [clip_json("c1", "voice"), clip_json("c2", "game")],
            "people": {"member_count": 3, "open_seats": 2},
            "sessions": [{"id": "s1", "type": "session", "ts": 1_700_000_000_000_i64,
                          "data": {"game_name": "Counter-Strike 2", "duration_min": 60, "player_ids": ["user-1"]}}],
            "history": []
        })),
    });
    let z = h.app().get_feed_zones();
    assert_eq!(z.stage.as_str(), "card");
    assert_eq!(z.stage_card.id.as_str(), "hero");
    assert!(z.stage_card.is_hero, "the stage clip draws as the hero");
    assert_eq!(z.board.as_str(), "tabs");
    assert_eq!(z.clips.row_count(), 2);
    assert_eq!(z.sessions.row_count(), 1);
    assert_eq!(
        z.sessions.row_data(0).map(|c| c.card_type.to_string()),
        Some("game-session".into()),
        "a game session is a real session card"
    );
    assert_eq!(z.open_seats, 2);
    assert!(z.people);
}

/// A locked week of the payload shows sealed with its teasers, and the
/// carousel's history tile counts it.
#[test]
fn a_locked_week_of_the_payload_is_sealed_with_teasers() {
    let mut h = Harness::new();
    zones_on(&mut h, 12);
    h.emit(Event::FeedLoaded {
        response: zones_response(serde_json::json!({
            "board": {"mode": "tabs"},
            "history": [{"week_start": 1_792_368_000_000_i64, "week_number": 44, "locked": true,
                         "clip_count": 5, "stream_count": 1,
                         "teasers": [{"kind": "clip", "actor_name": "Leo", "duration_seconds": 30.0, "ts": 1_792_400_000_000_i64},
                                     {"kind": "stream", "actor_name": "Ida", "duration_seconds": 4320.0, "ts": 1_792_500_000_000_i64}]}],
            "history_next": 0
        })),
    });
    let z = h.app().get_feed_zones();
    let week = z.history.row_data(0).expect("a week");
    assert!(week.locked && week.seal.sealed);
    assert_eq!(week.label.as_str(), "Week 44");
    assert_eq!(week.teasers.row_count(), 2);
    assert_eq!(
        week.teasers.row_data(1).map(|t| t.duration.to_string()),
        Some("1 h 12".into())
    );
    assert_eq!(week.more, 4, "six items, two teasers shown");
    assert_eq!(week.clips.row_count(), 0, "nothing plays in a locked week");
    assert!(!z.history_more);
}

/// Scrolling near the end asks for the next page once; the page appends its
/// weeks and the next scroll asks for nothing more.
#[test]
fn crew_history_pages_by_week() {
    let mut h = Harness::new();
    zones_on(&mut h, 12);
    let week =
        |start: i64| serde_json::json!({"week_start": start, "locked": true, "clip_count": 1});
    h.emit(Event::FeedLoaded {
        response: zones_response(serde_json::json!({
            "board": {"mode": "tabs"},
            "history": [week(1_792_368_000_000)],
            "history_next": 1_792_368_000_000_i64
        })),
    });
    assert!(h.app().get_feed_zones().history_more);
    h.commands();
    h.app().invoke_zones_history_more();
    h.app().invoke_zones_history_more();
    h.pump();
    let asked: Vec<i64> = h
        .commands()
        .into_iter()
        .filter_map(|c| match c {
            Command::LoadCrewHistory { before, .. } => Some(before),
            _ => None,
        })
        .collect();
    assert_eq!(
        asked,
        vec![1_792_368_000_000],
        "one request for the next page"
    );

    h.emit(Event::FeedHistoryLoaded {
        response: zones_response(serde_json::json!({
            "history": [week(1_791_763_200_000)],
            "history_next": 0
        })),
    });
    let z = h.app().get_feed_zones();
    assert_eq!(z.history.row_count(), 2, "the page appends its week");
    assert!(!z.history_more, "the last page");
}

/// The m3llo+ coming soon modal closes with its Close button.
#[test]
fn the_coming_soon_modal_closes() {
    let mut h = Harness::new();
    zones_on(&mut h, 12);
    h.app().set_plus_soon_open(true);
    h.pump();
    h.click_label("CLOSE");
    assert!(!h.app().get_plus_soon_open());
}
