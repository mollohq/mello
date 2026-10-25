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
    let now = crate::head2head::now_ms();
    let owl_at = now - owl_h * HOUR_MS;
    let carry_at = now - carry_h * HOUR_MS;
    let owl = serde_json::json!({
        "title_id": "night_owl", "holder_user_id": "u4", "value": "03:40", "unit": "",
        "since_ms": owl_at, "moved_at_ms": owl_at, "previous_holder_user_id": "u5",
    });
    let carry = serde_json::json!({
        "title_id": "the_carry", "holder_user_id": "u2", "value": 4201.0, "unit": "h",
        "since_ms": carry_at, "moved_at_ms": carry_at, "previous_holder_user_id": "u3",
    });
    feed_of(vec![h2h_entry(
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
    )])
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
