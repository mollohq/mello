//! Sealed belts and the weekly recap, and the centre view that opens them
//! (plan `SEALED-REVEAL.md`). The `Clock` is manual here, so a frame at a given
//! time is the same on every run.

use super::*;

const HOUR_MS: i64 = 3_600_000;

fn clock(h: &Harness) -> crate::Clock<'_> {
    h.app().global::<crate::Clock>()
}

/// Put the clock at `ms` after the launch. The launch is at 0.
fn at(h: &Harness, ms: i64) {
    clock(h).set_manual(true);
    clock(h).set_manual_now(ms);
    // `changed` handlers run when the timers and animations update.
    slint::platform::update_timers_and_animations();
}

/// A crew of five. u4 took night_owl from u5 `owl_h` hours ago. u2 took
/// the_carry from u3 `carry_h` hours ago: after 24 hours it is no unread belt.
fn feed_with_belts_moved(owl_h: i64, carry_h: i64) -> FeedResponse {
    feed_of(entries_with_belts_moved(owl_h, carry_h))
}

fn entries_with_belts_moved(owl_h: i64, carry_h: i64) -> Vec<FeedEntry> {
    let now = crate::head2head::now_ms();
    entries_with_belts_moved_at(now - owl_h * HOUR_MS, now - carry_h * HOUR_MS)
}

/// The same crew, with the move times given.
fn entries_with_belts_moved_at(owl_at: i64, carry_at: i64) -> Vec<FeedEntry> {
    let owl = serde_json::json!({
        "title_id": "night_owl", "holder_user_id": "u4", "value": "03:40", "unit": "",
        "since_ms": owl_at, "moved_at_ms": owl_at, "previous_holder_user_id": "u5",
    });
    let carry = serde_json::json!({
        "title_id": "the_carry", "holder_user_id": "u2", "value": 4201.0, "unit": "h",
        "since_ms": carry_at, "moved_at_ms": carry_at, "previous_holder_user_id": "u3",
    });
    vec![h2h_entry(
        5,
        vec![owl.clone(), carry],
        vec![
            h2h_pair_values(
                "u2",
                1,
                6,
                vec![(
                    "the_carry",
                    serde_json::json!(100.0),
                    serde_json::json!(4201.0),
                )],
            ),
            h2h_pair_values(
                "u4",
                3,
                3,
                vec![(
                    "night_owl",
                    serde_json::json!("01:00"),
                    serde_json::json!("03:40"),
                )],
            ),
        ],
        Some(owl),
    )]
}

fn open_feed(h: &mut Harness, feed: FeedResponse) {
    app_with_crew_of(h, 5);
    set_roster(
        h,
        &[
            ("user-1", "Test User"),
            ("u2", "Jonas"),
            ("u3", "Sara"),
            ("u4", "Max"),
            ("u5", "Leo"),
        ],
    );
    open_crew_feed_fresh(h);
    h.emit(Event::FeedLoaded { response: feed });
}

/// `(title, sealed, dot)` of the pair rows that carry a chip.
fn ui(h: &Harness) -> crate::SealedUi<'_> {
    h.app().global::<crate::SealedUi>()
}

/// `(title, sealed, dot)` of the pair rows that carry a chip.
fn chips(h: &Harness) -> Vec<(String, bool, bool)> {
    let pairs = first_card(h).head2head.pairs;
    (0..pairs.row_count())
        .filter_map(|i| pairs.row_data(i))
        .filter(|p| p.has_chip)
        .map(|p| (p.chip.title.to_string(), p.chip.belt.sealed, p.moved_recent))
        .collect()
}

fn stored_keys(h: &Harness) -> Vec<String> {
    h.settings()
        .borrow()
        .sealed_opened
        .get("crew-0")
        .cloned()
        .unwrap_or_default()
}

fn sealed_controls(h: &Harness) -> usize {
    h.controls_labelled("Sealed belt, open it").len()
}

/// The sealed chip, the sealed recap and the scrim of the centre view are
/// controls: a role and a label. The driver finds them by the label.
#[test]
fn the_sealed_cards_and_the_scrim_are_controls_with_a_role_and_a_label() {
    use i_slint_backend_testing::AccessibleRole::Button;

    let mut h = Harness::new();
    open_feed(&mut h, feed_with_belts_moved(2, 30));
    let chips = h.controls_labelled("Sealed belt, open it");
    assert_eq!(chips.len(), 2);
    assert!(chips.iter().all(|c| c.accessible_role() == Some(Button)));
    assert!(
        h.controls_labelled("Close").is_empty(),
        "no scrim without a card"
    );

    at(&h, 0);
    h.click_label("Sealed belt, open it");
    let close = h.controls_labelled("Close");
    assert_eq!(close.len(), 1, "one scrim");
    assert_eq!(close[0].accessible_role(), Some(Button));
    assert_eq!(
        sealed_controls(&h),
        1,
        "the belt in the centre is no control in its slot"
    );

    // The recap: sealed, then in the centre, then open with its control.
    let mut h = Harness::new();
    emit_recap(&mut h, serde_json::json!({"week_end": now_ms()}));
    let recap = h.controls_labelled("Sealed recap, open it");
    assert_eq!(recap.len(), 1);
    assert_eq!(recap[0].accessible_role(), Some(Button));
    assert!(
        h.controls_labelled("See the week").is_empty(),
        "a sealed recap shows nothing of the week"
    );
    at(&h, 0);
    h.click_label("Sealed recap, open it");
    assert_eq!(h.controls_labelled("Close").len(), 1);
    at(&h, 1500);
    h.click_label("Close");
    at(&h, 2100);
    h.pump();
    assert!(h.controls_labelled("Sealed recap, open it").is_empty());
    let see = h.controls_labelled("See the week");
    assert_eq!(see.len(), 1, "the open recap card has its control");
    assert_eq!(see[0].accessible_role(), Some(Button));
}

#[test]
fn a_belt_that_moved_is_sealed_and_not_a_title_that_did_not() {
    let mut h = Harness::new();
    open_feed(&mut h, feed_with_belts_moved(2, 30));
    // Both belts moved in the last 7 days and neither is opened.
    let c = chips(&h);
    assert_eq!(c.len(), 2, "{c:?}");
    assert!(c.iter().all(|(_, sealed, _)| *sealed), "{c:?}");
    assert_eq!(sealed_controls(&h), 2);
    assert_eq!(count_of(&h, "SealedChip"), 2);
    assert_eq!(count_of(&h, "TitleChip"), 0, "a sealed chip draws no title");
    assert_eq!(ui(&h).get_active_kind().as_str(), "");
    assert_eq!(
        count_of(&h, "SealedStage"),
        0,
        "no centre view when nothing is open"
    );
}

#[test]
fn a_move_older_than_seven_days_is_open() {
    let mut h = Harness::new();
    open_feed(&mut h, feed_with_belts_moved(8 * 24, 9 * 24));
    let c = chips(&h);
    assert!(c.iter().all(|(_, sealed, _)| !*sealed), "{c:?}");
    assert_eq!(sealed_controls(&h), 0);
    assert_eq!(count_of(&h, "TitleChip"), 2);
}

#[test]
fn a_click_sends_the_card_to_the_centre_and_a_click_on_the_scrim_brings_it_back_open() {
    let mut h = Harness::new();
    open_feed(&mut h, feed_with_belts_moved(2, 30));
    assert_eq!(
        chips(&h)[0],
        ("Night Owl".to_string(), true, true),
        "sealed and unread"
    );
    at(&h, 0);

    // The card of the Night Owl leaves its slot.
    h.click_label("Sealed belt, open it");
    assert_eq!(ui(&h).get_active_kind().as_str(), "belt");
    assert_eq!(count_of(&h, "SealedStage"), 1);
    assert_eq!(
        stored_keys(&h).len(),
        1,
        "the key is stored when the card leaves"
    );
    assert!(
        stored_keys(&h)[0].starts_with("night_owl:"),
        "{:?}",
        stored_keys(&h)
    );
    assert_eq!(sealed_controls(&h), 1, "the other belt waits");

    // The charge shows the back, the rest shows the face.
    at(&h, 700);
    assert_eq!(
        (count_of(&h, "BigBeltBack"), count_of(&h, "BigBeltFace")),
        (1, 0)
    );
    at(&h, 1400);
    assert_eq!(
        (count_of(&h, "BigBeltBack"), count_of(&h, "BigBeltFace")),
        (0, 1)
    );

    // A click before the card rests does nothing. A click at rest closes.
    at(&h, 800);
    h.click_label("Close");
    assert!(!ui(&h).get_closing());
    at(&h, 1500);
    h.click_label("Close");
    assert!(ui(&h).get_closing());
    assert_eq!(
        count_of(&h, "SealedStage"),
        1,
        "the card is still on its way back"
    );

    // It lands: the cards are built again and the centre view is gone.
    at(&h, 2100);
    h.pump();
    assert_eq!(ui(&h).get_active_kind().as_str(), "");
    assert_eq!(count_of(&h, "SealedStage"), 0);
    let c = chips(&h);
    assert_eq!(
        c.iter().filter(|(_, sealed, _)| *sealed).count(),
        1,
        "{c:?}"
    );
    assert_eq!(
        c[0],
        ("Night Owl".to_string(), false, false),
        "open, and the dot is clear"
    );
}

fn card_of_type(h: &Harness, card_type: &str) -> FeedCardData {
    let feed = h.app().get_feed_cards();
    (0..feed.row_count())
        .filter_map(|i| feed.row_data(i))
        .find(|c| c.card_type == card_type)
        .unwrap_or_else(|| panic!("no {card_type} card"))
}

/// The same belt sits in a pair row and in the recap. Opening one opens both,
/// and both slots draw the same code.
#[test]
fn opening_a_belt_opens_it_in_every_slot() {
    let mut h = Harness::new();
    // The recap is open, so its rows show. The Carry moved long ago: only the
    // Night Owl is sealed.
    let now = crate::head2head::now_ms();
    let week_start = now - 3 * 86_400_000;
    open_recap(&h, week_start);
    let owl_at = now - 2 * HOUR_MS;
    let mut entries = entries_with_belts_moved_at(owl_at, now - 8 * 24 * HOUR_MS);
    let mut moved = belt_moved("night_owl", serde_json::json!("03:40"), "");
    moved["moved_at_ms"] = owl_at.into();
    moved["holder_user_id"] = "u4".into();
    moved["previous_holder_user_id"] = "u5".into();
    entries.push(recap_entry(serde_json::json!({
        "belts_moved": [moved], "week_start": week_start, "week_end": now_ms(),
    })));
    open_feed(&mut h, feed_of(entries));
    assert_eq!(sealed_controls(&h), 2, "the pair row and the recap row");
    assert_eq!(count_of(&h, "SealedChip"), 2);

    // One key, one code.
    let in_cell = card_of_type(&h, "head2head")
        .head2head
        .pairs
        .row_data(0)
        .expect("row")
        .chip
        .belt;
    let in_recap = card_of_type(&h, "recap")
        .belts_moved
        .row_data(0)
        .expect("row")
        .belt;
    assert!(in_cell.sealed && in_recap.sealed);
    assert_eq!(in_cell.key.as_str().split(':').next(), Some("night_owl"));
    assert_eq!(
        in_cell.code, in_recap.code,
        "one seed, one code in every slot"
    );
    assert_eq!(in_cell.cells.row_count(), 90);

    // While the card is in the centre, the other slot stays sealed and cannot be
    // opened. When the card lands, both are open.
    at(&h, 0);
    h.click_label("Sealed belt, open it");
    assert_eq!(
        count_of(&h, "SealedChip"),
        1,
        "the slot of the card is empty"
    );
    at(&h, 1500);
    h.click_label("Close");
    at(&h, 2100);
    h.pump();
    assert_eq!(count_of(&h, "SealedChip"), 0);
    assert_eq!(sealed_controls(&h), 0);
    let row = card_of_type(&h, "recap")
        .belts_moved
        .row_data(0)
        .expect("row");
    assert!(!row.belt.sealed, "the recap row is open too");
    assert_eq!(
        count_of(&h, "TitleChip"),
        3,
        "two pair rows and the recap row"
    );
}

/// The recap is sealed by week: opening one week does not open another.
#[test]
fn the_recap_is_sealed_by_week() {
    let mut h = Harness::new();
    open_recap(&h, 1);
    emit_recap(
        &mut h,
        serde_json::json!({"week_start": 2, "week_end": now_ms()}),
    );
    let card = card_of_type(&h, "recap");
    assert!(card.recap_seal.sealed, "week 2 is not opened");
    assert_eq!(card.recap_seal.key.as_str(), "recap:2");

    let mut h = Harness::new();
    open_recap(&h, 1);
    emit_recap(
        &mut h,
        serde_json::json!({"week_start": 1, "week_end": now_ms()}),
    );
    assert!(
        !card_of_type(&h, "recap").recap_seal.sealed,
        "week 1 is opened"
    );

    // A recap older than seven days opens silently.
    let mut h = Harness::new();
    emit_recap(
        &mut h,
        serde_json::json!({"week_start": 3, "week_end": now_ms() - 8 * 86_400_000}),
    );
    assert!(!card_of_type(&h, "recap").recap_seal.sealed);
    assert!(stored_keys(&h).is_empty(), "a silent open stores nothing");
}

#[test]
fn opening_the_recap_stores_the_week_and_the_card_returns_open() {
    let mut h = Harness::new();
    emit_recap(
        &mut h,
        serde_json::json!({"week_start": 5, "week_end": now_ms()}),
    );
    assert_eq!(count_of(&h, "SealedRecap"), 1);
    assert_eq!(count_of(&h, "RecapCard"), 0);
    at(&h, 0);
    h.click_label("Sealed recap, open it");
    assert_eq!(ui(&h).get_active_kind().as_str(), "recap");
    assert_eq!(stored_keys(&h), vec!["recap:5".to_string()]);
    at(&h, 700);
    assert_eq!(count_of(&h, "SealedRecap"), 1, "the back in the centre");
    at(&h, 1400);
    assert_eq!(count_of(&h, "RecapCard"), 1, "the face in the centre");
    at(&h, 1500);
    h.click_label("Close");
    at(&h, 2100);
    h.pump();
    assert_eq!(ui(&h).get_active_kind().as_str(), "");
    assert_eq!(
        (count_of(&h, "SealedRecap"), count_of(&h, "RecapCard")),
        (0, 1)
    );

    // "See the week" opens the full-screen recap.
    h.click_label("See the week");
    assert_eq!(count_of(&h, "RecapOpened"), 1);
}

/// A belt that moved is sealed in the versus hero too, and the tally stays.
#[test]
fn the_versus_hero_seals_its_moved_belt_and_keeps_its_tally() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    let at_ms = crate::head2head::now_ms() - HOUR_MS;
    let moved = serde_json::json!({
        "title_id": "the_collector", "holder_user_id": "u2", "value": 14, "unit": "installed",
        "since_ms": at_ms, "moved_at_ms": at_ms, "previous_holder_user_id": "user-1",
    });
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            2,
            vec![moved.clone()],
            vec![h2h_pair("u2", 0, 1)],
            Some(moved),
        )]),
    });
    assert_eq!(count_of(&h, "VersusHero"), 1);
    assert_eq!(sealed_controls(&h), 1, "the moved chip is sealed");
    let hero = first_card(&h).head2head;
    assert_eq!(
        (hero.left_score, hero.right_score),
        (0, 1),
        "the tally shows"
    );
    h.click_label("Sealed belt, open it");
    assert_eq!(ui(&h).get_active_kind().as_str(), "belt");
    assert_eq!(
        ui(&h).get_belt().holder.name.as_str(),
        "Jonas",
        "the card in the centre knows the new holder"
    );
}

/// The card in the centre is drawn at a fixed time of the manual clock: the
/// same frame on every run, the face with the count-up and the new holder.
#[test]
fn a_frame_at_a_fixed_clock_time_renders() {
    let mut h = Harness::new();
    open_feed(&mut h, feed_with_belts_moved(2, 30));
    at(&h, 0);
    h.click_label("Sealed belt, open it");
    at(&h, 1400);
    h.assert_not_blank();
    let texts = drawn_texts(&h);
    assert!(
        texts.iter().any(|t| t == "Max"),
        "the holder pops in: {texts:?}"
    );
    assert!(texts.iter().any(|t| t == "Night Owl"), "{texts:?}");
    assert!(texts.iter().any(|t| t == "took it from"), "{texts:?}");
    // The clock reads 1400 ms: 25 percent of the count-up is still to run.
    let value = texts.iter().find(|t| t.contains(':') && t.len() == 5);
    assert!(value.is_some(), "a clock value counts up: {texts:?}");
    at(&h, 2300);
    assert!(
        drawn_texts(&h).iter().any(|t| t == "03:40"),
        "the count ends at the value"
    );
}

/// The card is at rest from 1250 ms, and only then does the scrim close it.
#[test]
fn the_card_rests_from_1250_ms() {
    let mut h = Harness::new();
    open_feed(&mut h, feed_with_belts_moved(2, 30));
    at(&h, 0);
    assert!(!ui(&h).get_resting(), "nothing in the centre");
    h.click_label("Sealed belt, open it");
    at(&h, 1249);
    assert!(!ui(&h).get_resting());
    at(&h, 1250);
    assert!(ui(&h).get_resting());
    h.click_label("Close");
    assert!(!ui(&h).get_resting(), "it is on its way back");
}

#[test]
fn the_centre_view_has_the_bounds_of_the_feed_panel_and_a_hint_that_is_no_control() {
    let mut h = Harness::new();
    open_feed(&mut h, feed_with_belts_moved(2, 30));
    at(&h, 0);
    h.click_label("Sealed belt, open it");
    at(&h, 1800);

    let bounds = |name: &str| {
        let found: Vec<_> = ElementHandle::find_by_element_type_name(h.app(), name).collect();
        assert_eq!(found.len(), 1, "{name}");
        (found[0].absolute_position(), found[0].size())
    };
    assert_eq!(
        bounds("SealedStage"),
        bounds("CrewFeed"),
        "the centre view covers the feed panel and nothing else"
    );
    // The scrim is the only control. The hint is plain text.
    assert_eq!(h.controls_labelled("Close").len(), 1);
    assert!(h.controls_labelled("Click anywhere to close").is_empty());
}
