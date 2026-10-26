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

// ---- Day 0: the own chips of a new crew ---------------------------------

/// A crew of one with five own values. The card draws the first four.
fn solo_feed(carry_h: f64) -> FeedResponse {
    let mut entry = h2h_entry(1, vec![], vec![], None);
    entry.data["own_values"] = serde_json::json!([
        {"title_id": "the_carry", "value": carry_h, "unit": "h", "display": "4,167"},
        {"title_id": "night_owl", "value": "03:14", "unit": "", "display": "03:14"},
        {"title_id": "the_collector", "value": 5, "unit": "installed", "display": "5"},
        {"title_id": "locked_in", "value": 4.6, "unit": "h", "display": "4.6"},
        {"title_id": "sampler", "value": 3, "unit": "games", "display": "3"},
    ]);
    feed_of(vec![entry])
}

fn open_solo(h: &mut Harness, feed: FeedResponse) {
    app_with_crew_of(h, 1);
    set_roster(h, &[("user-1", "Test User")]);
    h.emit(Event::FeedLoaded { response: feed });
}

/// `(key, sealed, own)` of the own chips of the wide card.
fn own_chips(h: &Harness) -> Vec<(String, bool, bool)> {
    let chips = first_card(h).head2head.chips;
    (0..chips.row_count())
        .filter_map(|i| chips.row_data(i))
        .map(|t| (t.belt.key.to_string(), t.belt.sealed, t.belt.own))
        .collect()
}

/// A new crew seals every own chip, each under its own key, and the card keeps
/// its headline, the four open seats and the invite.
#[test]
fn a_new_crew_seals_all_four_own_chips() {
    let mut h = Harness::new();
    open_solo(&mut h, solo_feed(4167.0));
    assert_eq!(
        own_chips(&h),
        [
            ("own:the_carry:crew-0".to_string(), true, true),
            ("own:night_owl:crew-0".to_string(), true, true),
            ("own:the_collector:crew-0".to_string(), true, true),
            ("own:locked_in:crew-0".to_string(), true, true),
        ]
    );
    assert_eq!(
        h.controls_labelled("Sealed belt, open it").len(),
        4,
        "the own chip is labelled like any sealed belt"
    );
    assert_eq!(count_of(&h, "TitleChip"), 0, "the values are not drawn");
    let texts = drawn_texts(&h);
    assert!(texts.iter().any(|t| t == "GO HEAD2HEAD WITH YOUR CREW"));
    assert_eq!(texts.iter().filter(|t| t.as_str() == "OPEN").count(), 4);
    assert_eq!(h.controls_labelled("Invite your crew").len(), 1);
    assert!(
        !texts.iter().any(|t| t == "4,167"),
        "a sealed chip hides its value: {texts:?}"
    );
}

/// A click opens one chip: the centre view, the face with the title, the value
/// and the viewer as "You", no "took it from". The return leaves that chip
/// open and the other three sealed.
#[test]
fn opening_one_own_chip_opens_only_that_one() {
    let mut h = Harness::new();
    open_solo(&mut h, solo_feed(4167.0));
    at(&h, 0);
    h.click_label("Sealed belt, open it");
    assert_eq!(ui(&h).get_active_kind().as_str(), "belt");
    assert_eq!(stored_keys(&h), ["own:the_carry:crew-0"]);
    assert_eq!(
        h.controls_labelled("Sealed belt, open it").len(),
        3,
        "the other three wait"
    );

    at(&h, 1400);
    assert_eq!(count_of(&h, "BigBeltFace"), 1);
    let texts = drawn_texts(&h);
    for want in ["The Carry", "You", "Most hours in the top shared game."] {
        assert!(texts.iter().any(|t| t == want), "missing {want}: {texts:?}");
    }
    assert!(
        !texts.iter().any(|t| t == "took it from"),
        "an own value took nothing from anyone: {texts:?}"
    );

    at(&h, 1500);
    h.click_label("Close");
    at(&h, 2100);
    h.pump();
    assert_eq!(ui(&h).get_active_kind().as_str(), "");
    let c = own_chips(&h);
    assert_eq!(
        c.iter().map(|(_, sealed, _)| *sealed).collect::<Vec<_>>(),
        [false, true, true, true],
        "{c:?}"
    );
    assert_eq!(count_of(&h, "TitleChip"), 1, "the open chip is in the slot");
    assert!(drawn_texts(&h).iter().any(|t| t == "4,167"));
}

/// A reopened crew keeps the opened chips open, even when the value changed
/// since: the key has no value in it.
#[test]
fn a_reopened_crew_keeps_opened_chips_open_when_the_value_changes() {
    let mut h = Harness::new();
    h.settings().borrow_mut().sealed_opened.insert(
        "crew-0".into(),
        vec![crate::sealed::own_key("the_carry", "crew-0")],
    );
    open_solo(&mut h, solo_feed(4200.0));
    let c = own_chips(&h);
    assert_eq!(
        c.iter().map(|(_, sealed, _)| *sealed).collect::<Vec<_>>(),
        [false, true, true, true],
        "{c:?}"
    );
    assert!(
        drawn_texts(&h).iter().any(|t| t == "4,167"),
        "the open chip draws its value"
    );
}

/// The 7 day silent open: a crew whose own chips first showed 8 days ago has
/// them open, with no animation and no stored key.
#[test]
fn own_chips_open_silently_after_seven_days() {
    let mut h = Harness::new();
    h.settings().borrow_mut().own_seal_since.insert(
        "crew-0".into(),
        crate::head2head::now_ms() - 8 * 24 * HOUR_MS,
    );
    open_solo(&mut h, solo_feed(4167.0));
    assert!(own_chips(&h).iter().all(|(_, sealed, _)| !sealed));
    assert!(stored_keys(&h).is_empty());
    assert_eq!(h.controls_labelled("Sealed belt, open it").len(), 0);
}

/// The first time the chips show, the crew's start is stored, so the 7 days
/// count from it and survive a restart.
#[test]
fn the_first_day_of_own_chips_is_stored() {
    let mut h = Harness::new();
    open_solo(&mut h, solo_feed(4167.0));
    assert!(h.settings().borrow().own_seal_since.contains_key("crew-0"));
}

// ---- The centre card fits the feed panel ---------------------------------

fn bounds_of(h: &Harness, name: &str) -> (f32, f32, f32, f32) {
    let found: Vec<_> = ElementHandle::find_by_element_type_name(h.app(), name).collect();
    assert_eq!(found.len(), 1, "{name}");
    let (p, s) = (found[0].absolute_position(), found[0].size());
    (p.x, p.y, s.width, s.height)
}

/// Size the window so that the feed panel is `panel_w` by `panel_h`.
fn size_feed_panel(h: &Harness, panel_w: f32, panel_h: f32) {
    let (_, _, w0, h0) = bounds_of(h, "CrewFeed");
    let win = h.app().window().size();
    h.app().window().set_size(slint::PhysicalSize::new(
        (win.width as f32 + panel_w - w0) as u32,
        (win.height as f32 + panel_h - h0) as u32,
    ));
    let (_, _, w, hh) = bounds_of(h, "CrewFeed");
    assert!(
        (w - panel_w).abs() < 1.0 && (hh - panel_h).abs() < 1.0,
        "the panel is {w} by {hh}"
    );
}

/// The card at rest keeps 40 px of the panel on every side and is centred.
fn assert_card_inside_panel(h: &Harness) {
    let (px, py, pw, ph) = bounds_of(h, "CrewFeed");
    let (x, y, w, hgt) = bounds_of(h, "SealedCardBounds");
    let msg = format!("card {x},{y} {w}x{hgt} in panel {px},{py} {pw}x{ph}");
    assert!(x >= px + 40.0 - 0.5 && y >= py + 40.0 - 0.5, "{msg}");
    assert!(
        x + w <= px + pw - 40.0 + 0.5 && y + hgt <= py + ph - 40.0 + 0.5,
        "{msg}"
    );
    assert!(
        ((x + w / 2.0) - (px + pw / 2.0)).abs() < 1.0
            && ((y + hgt / 2.0) - (py + ph / 2.0)).abs() < 1.0,
        "centred: {msg}"
    );
}

fn open_recap_in_the_centre(panel_w: f32, panel_h: f32) -> Harness {
    let mut h = Harness::new();
    emit_recap(&mut h, serde_json::json!({"week_end": now_ms()}));
    size_feed_panel(&h, panel_w, panel_h);
    at(&h, 0);
    h.click_label("Sealed recap, open it");
    at(&h, 1800);
    h
}

/// Bob's centre recap was taller than the panel: the top cut off, the belts held
/// row visible and "See the week" half outside. The card scales to
/// `min(1, (panel height - 80) / card height, (panel width - 80) / card width)`.
#[test]
fn the_opened_recap_is_inside_a_900_px_tall_feed_panel() {
    let h = open_recap_in_the_centre(1008.0, 900.0);
    assert_card_inside_panel(&h);
    // It does not grow past its own size.
    let (_, _, w, _) = bounds_of(&h, "SealedCardBounds");
    let (_, _, sw, _) = bounds_of(&h, "RecapCard");
    assert!(w <= sw + 0.5, "card {w} wide from a {sw} wide slot");
}

#[test]
fn the_opened_recap_shrinks_to_a_short_and_narrow_panel() {
    for (pw, ph) in [(1008.0, 400.0), (520.0, 900.0), (480.0, 460.0)] {
        let h = open_recap_in_the_centre(pw, ph);
        assert_card_inside_panel(&h);
    }
}

#[test]
fn the_big_belt_is_inside_the_panel_and_never_larger_than_800_by_400() {
    for (pw, ph) in [
        (1008.0, 900.0),
        (1008.0, 420.0),
        (500.0, 900.0),
        (480.0, 480.0),
    ] {
        let mut h = Harness::new();
        open_feed(&mut h, feed_with_belts_moved(2, 30));
        size_feed_panel(&h, pw, ph);
        at(&h, 0);
        h.click_label("Sealed belt, open it");
        at(&h, 1800);
        assert_card_inside_panel(&h);
        let (_, _, w, hgt) = bounds_of(&h, "SealedCardBounds");
        assert!(w <= 800.5 && hgt <= 400.5, "{w}x{hgt}");
    }
}
