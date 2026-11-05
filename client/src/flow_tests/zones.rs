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
