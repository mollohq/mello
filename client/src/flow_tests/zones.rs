//! The zone feed (`handlers/zones.rs`): the server's zones draw into Stage,
//! Board, Clips, People, Sessions and Crew history.

use super::*;

fn zones_on(h: &mut Harness, members: i32) {
    app_with_crew_of(h, members);
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

/// A crew of one: the first five (four open seats) and the solo board.
#[test]
fn a_crew_of_one_shows_the_first_five() {
    let mut h = Harness::new();
    zones_on(&mut h, 1);
    let mut feed = feed_of(vec![h2h_entry(1, vec![], vec![], None)]);
    feed.zones.board.mode = "solo".into();
    feed.zones.people.open_seats = 4;
    h.emit(Event::FeedLoaded { response: feed });
    let z = h.app().get_feed_zones();
    assert!(z.people, "the people zone shows");
    assert_eq!(z.open_seats, 4);
    assert_eq!(z.board.as_str(), "solo");
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

// ── The zones payload ───────────────────────────────────────────────────────

fn zones_response(zones: serde_json::Value) -> FeedResponse {
    FeedResponse {
        crew_id: "crew-0".into(),
        zones: serde_json::from_value(zones).expect("a zones payload"),
    }
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

/// A poster on a card stays on it when the zones are built again. The Stage
/// keeps its instance through a rebuild and does not ask for the poster again.
#[test]
fn a_loaded_poster_stays_through_a_rebuild() {
    let mut h = Harness::new();
    zones_on(&mut h, 3);
    let mut hero = clip_json("hero", "game");
    hero["data"]["poster_url"] = serde_json::json!("https://media.example/hero.jpg");
    let mut c2 = clip_json("c2", "game");
    c2["data"]["poster_url"] = serde_json::json!("https://media.example/c2.jpg");
    h.emit(Event::FeedLoaded {
        response: zones_response(serde_json::json!({
            "stage": {"kind": "game_clip", "entry": hero},
            "board": {"mode": "tabs"},
            "clips": [c2],
        })),
    });
    let img = slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(4, 4));
    for id in ["hero", "c2"] {
        crate::handlers::zones::update_cards(h.app(), id, |c| {
            c.snapshot_poster = img.clone();
            c.snapshot_poster_ready = true;
            c.snapshot_loading = false;
        });
    }
    let held = |h: &Harness| {
        let z = h.app().get_feed_zones();
        let clip = z.clips.row_data(0).expect("the clip");
        (
            z.stage_card.snapshot_poster_ready,
            z.stage_card.snapshot_poster.size().width,
            clip.snapshot_poster_ready,
        )
    };
    assert_eq!(held(&h), (true, 4, true));

    // A member's avatar or a live change builds the zones again.
    crate::handlers::zones::refresh(h.ctx());
    assert_eq!(held(&h), (true, 4, true), "the posters stay");
}
