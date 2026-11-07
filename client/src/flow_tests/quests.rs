//! The quest card of a new crew (`handlers/quests.rs`, plans/crew-quests.md):
//! the server sends `zones.quests` beside its Stage, and the client puts the
//! card ahead of the Stage until every step is done and every gift opened.

use super::*;

const MADE_MS: i64 = 1_793_880_000_000;

fn clip_json(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id, "type": "clip", "ts": MADE_MS,
        "data": {"event_id": id, "clip_id": id, "clip_type": "game", "clipper_name": "Max",
                 "duration_seconds": 30.0, "poster_url": format!("https://media.example/{id}.jpg"),
                 "media_url": format!("https://media.example/{id}.mp4")}
    })
}

/// A feed with quests owned by `owner`, the steps in `steps`, a game clip on
/// the server's Stage and the same clip in Clips (the server keeps it there
/// while quests show).
fn quests_feed(owner: &str, steps: serde_json::Value, gifts: serde_json::Value) -> FeedResponse {
    FeedResponse {
        crew_id: "crew-0".into(),
        zones: serde_json::from_value(serde_json::json!({
            "free_days": 7,
            "stage": {"kind": "game_clip", "entry": clip_json("hero")},
            "board": {"mode": "tabs"},
            "clips": [clip_json("hero"), clip_json("c1")],
            "people": {"member_count": 2, "open_seats": 3},
            "history": [],
            "quests": {"owner_id": owner, "made_ms": MADE_MS, "ends_ms": MADE_MS + 14 * 86_400_000,
                       "steps": steps, "gifts": gifts}
        }))
        .expect("a zones payload"),
    }
}

fn on_crew(h: &mut Harness) {
    app_with_crew_of(h, 2);
    set_roster(h, &[("user-1", "Test User"), ("u-ida", "Ida")]);
}

fn gift_states(h: &Harness) -> Vec<String> {
    let z = h.app().get_feed_zones();
    (0..z.quest_gifts.row_count())
        .filter_map(|i| z.quest_gifts.row_data(i))
        .map(|g| g.state.to_string())
        .collect()
}

#[test]
fn a_new_crew_shows_the_quest_card_ahead_of_the_stage() {
    let mut h = Harness::new();
    on_crew(&mut h);
    h.emit(Event::FeedLoaded {
        response: quests_feed("user-1", serde_json::json!({}), serde_json::json!({})),
    });
    let z = h.app().get_feed_zones();
    assert_eq!(z.stage.as_str(), "quests");
    assert_eq!(z.quest_count.as_str(), "1 / 5");
    assert_eq!(z.quest_rows.row_count(), 5);
    assert_eq!(
        gift_states(&h),
        ["ready", "locked", "locked", "locked", "locked"]
    );
    assert_eq!(
        z.clips.row_count(),
        2,
        "the Stage clip stays in Clips while the card holds the Stage"
    );
    assert_eq!(h.controls_labelled("Sealed gift, open it").len(), 1);
}

#[test]
fn a_live_stream_takes_the_stage_ahead_of_the_quest_card() {
    let mut h = Harness::new();
    on_crew(&mut h);
    h.app()
        .set_active_stream_cards(slint::ModelRc::new(slint::VecModel::from(vec![
            crate::StreamCardData::default(),
        ])));
    h.emit(Event::FeedLoaded {
        response: quests_feed("user-1", serde_json::json!({}), serde_json::json!({})),
    });
    assert_eq!(h.app().get_feed_zones().stage.as_str(), "live");
}

fn clock_at(h: &Harness, ms: i64) {
    let clock = h.app().global::<crate::Clock>();
    clock.set_manual(true);
    clock.set_manual_now(ms);
    // Run the `changed` handlers and make new conditional elements, as the
    // event loop does between two frames.
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
}

/// A ready gift goes to the centre view: the back, the flip, the face of the
/// moment and the gift, the actions at rest. A click on the scrim brings it
/// back open, and only that gift is open (SEALED-REVEAL.md section 3).
#[test]
fn opening_a_gift_runs_the_reveal_and_opens_only_that_gift() {
    let mut h = Harness::new();
    on_crew(&mut h);
    let steps = serde_json::json!({"friend": {"done_ms": MADE_MS, "user_id": "u-ida", "name": "Ida",
                                              "invited_by": "user-1", "invited_by_name": "Test User"}});
    h.emit(Event::FeedLoaded {
        response: quests_feed("user-1", steps.clone(), serde_json::json!({})),
    });
    assert_eq!(
        gift_states(&h),
        ["ready", "ready", "locked", "locked", "locked"]
    );
    clock_at(&h, 0);
    h.click_label("Sealed gift, open it");
    let ui = |h: &Harness| {
        h.app()
            .global::<crate::SealedUi>()
            .get_active_kind()
            .to_string()
    };
    assert_eq!(ui(&h), "quest");
    assert_eq!(
        h.app()
            .global::<crate::SealedUi>()
            .get_quest()
            .step
            .as_str(),
        "made",
        "the first ready gift"
    );
    let settings = h.settings();
    assert!(
        crate::sealed::opened_keys(&settings.borrow(), "crew-0").contains("quest:made:crew-0"),
        "the key is stored when the card leaves"
    );
    assert!(
        h.controls_labelled("Sealed gift, open it").is_empty(),
        "no other gift opens while one is in the centre"
    );

    clock_at(&h, 700);
    assert_eq!(
        (count_of(&h, "QuestBack"), count_of(&h, "QuestFace")),
        (1, 0)
    );
    clock_at(&h, 1400);
    assert_eq!(
        (count_of(&h, "QuestBack"), count_of(&h, "QuestFace")),
        (0, 1)
    );
    clock_at(&h, 2000);
    let texts = drawn_texts(&h);
    for t in ["Crew banner", "UNLOCKED", "2 / 5", "Made 5 November"] {
        assert!(texts.iter().any(|x| x == t), "{t} on the face: {texts:?}");
    }
    assert_eq!(
        h.controls_labelled("Game art").len(),
        1,
        "the actions show at rest"
    );
    assert_eq!(h.controls_labelled("Upload").len(), 1);

    h.click_label("Close");
    clock_at(&h, 2600);
    h.pump();
    assert_eq!(ui(&h), "");
    assert_eq!(
        gift_states(&h),
        ["open", "ready", "locked", "locked", "locked"]
    );
    assert_eq!(
        h.app()
            .get_feed_zones()
            .quest_gifts
            .row_data(0)
            .map(|g| g.label.to_string()),
        Some("BANNER".into()),
        "the name shows once the gift is open"
    );
    // A new feed load keeps it open.
    h.emit(Event::FeedLoaded {
        response: quests_feed("user-1", steps, serde_json::json!({})),
    });
    assert_eq!(
        gift_states(&h),
        ["open", "ready", "locked", "locked", "locked"]
    );
}

/// An action on the face closes the view and asks for its screen.
#[test]
fn an_action_on_the_face_closes_the_view() {
    let mut h = Harness::new();
    on_crew(&mut h);
    h.emit(Event::FeedLoaded {
        response: quests_feed("user-1", serde_json::json!({}), serde_json::json!({})),
    });
    clock_at(&h, 0);
    h.click_label("Sealed gift, open it");
    clock_at(&h, 2000);
    h.click_label("Game art");
    assert!(
        h.app().global::<crate::SealedUi>().get_closing(),
        "the card goes back"
    );
}

#[test]
fn a_member_never_gets_a_key() {
    let mut h = Harness::new();
    on_crew(&mut h);
    let steps = serde_json::json!({"friend": {"done_ms": MADE_MS, "user_id": "user-1", "name": "Test User"}});
    h.emit(Event::FeedLoaded {
        response: quests_feed(
            "u-owner",
            steps,
            serde_json::json!({"invite_name": "brawlbros"}),
        ),
    });
    assert_eq!(
        gift_states(&h),
        ["ready", "open", "locked", "locked", "locked"]
    );
    assert!(
        h.controls_labelled("Sealed gift, open it").is_empty(),
        "only the owner opens a gift"
    );
    let row = h
        .app()
        .get_feed_zones()
        .quest_rows
        .row_data(1)
        .expect("row 2");
    assert_eq!(row.title.as_str(), "You joined");
}

#[test]
fn the_card_goes_when_every_step_is_done_and_every_gift_opened() {
    let mut h = Harness::new();
    on_crew(&mut h);
    for step in ["made", "friend", "call", "clip", "session"] {
        let key = crate::sealed::quest_key(step, "crew-0");
        let settings = h.settings();
        crate::sealed::mark_opened(
            &mut settings.borrow_mut(),
            "crew-0",
            &key,
            crate::head2head::now_ms(),
        );
    }
    let all = serde_json::json!({
        "friend": {"user_id": "u-ida"}, "call": {"minutes": 6}, "clip": {"clip_id": "c1"}, "session": {"minutes": 40}
    });
    h.emit(Event::FeedLoaded {
        response: quests_feed("user-1", all, serde_json::json!({})),
    });
    let z = h.app().get_feed_zones();
    assert_eq!(z.stage.as_str(), "card", "the server's Stage is back");
    assert_eq!(z.stage_card.id.as_str(), "hero");
    assert_eq!(
        z.clips.row_count(),
        1,
        "the Stage clip leaves Clips when it is on the Stage"
    );
}

#[test]
fn the_row_actions_do_what_they_say() {
    let mut h = Harness::new();
    on_crew(&mut h);
    h.settings().borrow_mut().share_game_activity = false;
    h.emit(Event::FeedLoaded {
        response: quests_feed("user-1", serde_json::json!({}), serde_json::json!({})),
    });
    h.commands();

    h.click_label("Invite");
    assert!(
        h.commands()
            .iter()
            .any(|c| matches!(c, Command::CreateInviteCode { crew_id } if crew_id == "crew-0")),
        "Invite asks for a code; the share modal opens when it comes back"
    );

    h.click_label("Start voice");
    assert!(
        h.commands()
            .iter()
            .any(|c| matches!(c, Command::JoinVoice { .. })),
        "Start voice joins voice"
    );

    h.click_label("Game activity settings");
    assert!(h.app().get_settings_open(), "Settings opens");
    assert_eq!(h.app().get_settings_active_tab(), 3, "on the Games tab");
}
