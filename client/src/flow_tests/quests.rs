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
        response: quests_feed("user-1", steps.clone(), serde_json::json!({})),
    });
    assert_eq!(
        gift_states(&h),
        ["open", "ready", "locked", "locked", "locked"]
    );
    // So does a restart: the key is in the settings.
    let mut next = h.restart();
    on_crew(&mut next);
    next.emit(Event::FeedLoaded {
        response: quests_feed("user-1", steps, serde_json::json!({})),
    });
    assert_eq!(
        gift_states(&next),
        ["open", "ready", "locked", "locked", "locked"]
    );
}

/// At `ends_ms` the server stops sending `quests`. The card goes and the
/// server's Stage is back, with its clip out of Clips, gifts opened or not.
#[test]
fn the_card_goes_when_the_server_stops_sending_quests() {
    let mut h = Harness::new();
    on_crew(&mut h);
    h.emit(Event::FeedLoaded {
        response: quests_feed("user-1", serde_json::json!({}), serde_json::json!({})),
    });
    assert_eq!(h.app().get_feed_zones().stage.as_str(), "quests");
    let mut after = quests_feed("user-1", serde_json::json!({}), serde_json::json!({}));
    after.zones.quests = None;
    h.emit(Event::FeedLoaded { response: after });
    let z = h.app().get_feed_zones();
    assert_eq!(z.stage.as_str(), "card");
    assert_eq!(
        z.quest_gifts.row_count(),
        0,
        "no gift track without the card"
    );
    assert_eq!(z.clips.row_count(), 1);
    assert!(h.controls_labelled("Sealed gift, open it").is_empty());
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
        "friend": {"user_id": "u-ida"}, "call": {"user_ids": ["u-ida"]}, "clip": {"clip_id": "c1"}, "session": {"minutes": 40}
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

fn png_bytes() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(4, 2, image::Rgba([200, 120, 40, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)
        .expect("a png in memory");
    out.into_inner()
}

fn fetches(cmds: &[Command]) -> Vec<String> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::FetchMediaImage { url } => Some(url.clone()),
            _ => None,
        })
        .collect()
}

/// An uploaded banner is a media image: the open tile asks for it once and
/// shows it when it arrives.
#[test]
fn an_uploaded_banner_loads_into_its_open_tile() {
    let mut h = Harness::new();
    on_crew(&mut h);
    let url = "https://media.example/crew-banners/crew-0/b.jpg";
    {
        let settings = h.settings();
        crate::sealed::mark_opened(
            &mut settings.borrow_mut(),
            "crew-0",
            "quest:made:crew-0",
            crate::head2head::now_ms(),
        );
    }
    h.commands();
    h.emit(Event::FeedLoaded {
        response: quests_feed(
            "user-1",
            serde_json::json!({}),
            serde_json::json!({"banner_url": url}),
        ),
    });
    assert_eq!(
        fetches(&h.commands()),
        [url],
        "the banner is asked for once"
    );
    let tile = |h: &Harness| {
        h.app()
            .get_feed_zones()
            .quest_gifts
            .row_data(0)
            .expect("gift 1")
    };
    assert_eq!(tile(&h).state.as_str(), "open");
    assert!(!tile(&h).has_art, "nothing to draw before it arrives");
    h.emit(Event::FeedLoaded {
        response: quests_feed(
            "user-1",
            serde_json::json!({}),
            serde_json::json!({"banner_url": url}),
        ),
    });
    assert!(
        fetches(&h.commands()).is_empty(),
        "a fetch in flight is not asked again"
    );

    h.emit(Event::MediaImageLoaded {
        url: url.into(),
        bytes: png_bytes(),
    });
    assert!(tile(&h).has_art, "the banner draws in its tile");
    h.emit(Event::FeedLoaded {
        response: quests_feed(
            "user-1",
            serde_json::json!({}),
            serde_json::json!({"banner_url": url}),
        ),
    });
    assert!(
        fetches(&h.commands()).is_empty(),
        "a loaded image is not asked again"
    );
}

/// A fetch that failed is not asked again on every rebuild.
#[test]
fn a_failed_media_image_is_not_asked_again() {
    let mut h = Harness::new();
    on_crew(&mut h);
    let url = "https://media.example/crew-emoji/crew-0/gg.png";
    let all = serde_json::json!({"call": {"user_ids": ["u-ida"]}});
    {
        let settings = h.settings();
        crate::sealed::mark_opened(
            &mut settings.borrow_mut(),
            "crew-0",
            "quest:call:crew-0",
            crate::head2head::now_ms(),
        );
    }
    h.commands();
    h.emit(Event::FeedLoaded {
        response: quests_feed(
            "user-1",
            all.clone(),
            serde_json::json!({"emoji_count": 1, "first_emoji_url": url}),
        ),
    });
    assert_eq!(fetches(&h.commands()), [url]);
    h.emit(Event::MediaImageLoaded {
        url: url.into(),
        bytes: Vec::new(),
    });
    h.emit(Event::FeedLoaded {
        response: quests_feed(
            "user-1",
            all,
            serde_json::json!({"emoji_count": 1, "first_emoji_url": url}),
        ),
    });
    assert!(fetches(&h.commands()).is_empty());
    let emoji = h
        .app()
        .get_feed_zones()
        .quest_gifts
        .row_data(2)
        .expect("gift 3");
    assert_eq!(emoji.state.as_str(), "open");
    assert!(!emoji.has_art);
}

// ── 4c: the banner picker and the crew header ──────────────────────────────

fn picker(h: &Harness) -> crate::BannerPicker<'_> {
    h.app().global::<crate::BannerPicker>()
}

fn feed_with_session(owner: &str) -> FeedResponse {
    let mut f = quests_feed(owner, serde_json::json!({}), serde_json::json!({}));
    f.zones.sessions = vec![serde_json::from_value(serde_json::json!({
        "id": "s1", "type": "session", "ts": MADE_MS,
        "data": {"game_id": "steam-291550", "game_name": "Brawlhalla", "duration_min": 60, "player_ids": ["user-1"]}
    }))
    .expect("a session entry")];
    f
}

fn open_face_action(h: &mut Harness, label: &str) {
    clock_at(h, 0);
    h.click_label("Sealed gift, open it");
    clock_at(h, 2000);
    h.click_label(label);
}

#[test]
fn game_art_opens_the_picker_with_the_crews_games_and_save_sets_it() {
    let mut h = Harness::new();
    on_crew(&mut h);
    h.emit(Event::FeedLoaded {
        response: feed_with_session("user-1"),
    });
    open_face_action(&mut h, "Game art");
    assert!(picker(&h).get_open(), "the picker opens");
    assert_eq!(picker(&h).get_tab(), 0);
    let games: Vec<String> = picker(&h)
        .get_games()
        .iter()
        .map(|g| g.name.to_string())
        .collect();
    assert_eq!(
        games.first().map(String::as_str),
        Some("Brawlhalla"),
        "{games:?}"
    );
    assert_eq!(
        picker(&h).get_picked_id().as_str(),
        "steam-291550",
        "the first game is picked"
    );

    h.commands();
    h.click_label("Save");
    let sent = h.commands();
    assert!(
        sent.iter().any(
            |c| matches!(c, Command::SetCrewBannerGame { crew_id, game_id }
            if crew_id == "crew-0" && game_id == "steam-291550")
        ),
        "{sent:?}"
    );
    assert!(picker(&h).get_busy());

    h.emit(Event::CrewGiftFailed {
        crew_id: "crew-0".into(),
        gift: "banner".into(),
        reason: "Only the owner sets the banner.".into(),
    });
    assert!(
        picker(&h).get_open() && !picker(&h).get_busy(),
        "a failure keeps the picker"
    );
    assert_eq!(
        picker(&h).get_error().as_str(),
        "Only the owner sets the banner."
    );

    h.click_label("Save");
    h.emit(Event::CrewGiftChanged {
        crew_id: "crew-0".into(),
        gift: "banner".into(),
    });
    assert!(!picker(&h).get_open(), "a saved banner closes the picker");
}

#[test]
fn upload_opens_the_picker_on_your_image() {
    let mut h = Harness::new();
    on_crew(&mut h);
    h.emit(Event::FeedLoaded {
        response: feed_with_session("user-1"),
    });
    open_face_action(&mut h, "Upload");
    let bp = h.app().global::<crate::BannerPicker>();
    assert!(bp.get_open());
    assert_eq!(bp.get_tab(), 1);
    assert!(!bp.get_has_image(), "nothing chosen yet");
}

#[test]
fn the_search_finds_games_in_the_catalogue() {
    let mut h = Harness::new();
    on_crew(&mut h);
    h.emit(Event::FeedLoaded {
        response: feed_with_session("user-1"),
    });
    open_face_action(&mut h, "Game art");
    let bp = h.app().global::<crate::BannerPicker>();
    bp.invoke_search("rocket league".into());
    let hits: Vec<String> = bp
        .get_results()
        .iter()
        .map(|g| g.name.to_string())
        .collect();
    assert!(hits.iter().any(|n| n == "Rocket League"), "{hits:?}");
    assert!(
        bp.get_results().iter().all(|g| g.id.starts_with("igdb-")),
        "a catalogue game is an igdb- id for the art Worker"
    );
}

#[test]
fn an_uploaded_banner_shows_in_the_crew_header() {
    let mut h = Harness::new();
    on_crew(&mut h);
    let url = "https://media.example/crew-banners/crew-0/b.jpg";
    h.commands();
    let mut state = mello_core::crew_state::CrewState {
        crew_id: "crew-0".into(),
        name: "Crew 0".into(),
        ..Default::default()
    };
    state.banner = Some(mello_core::crew_state::CrewBanner {
        kind: "upload".into(),
        url: url.into(),
        version: 1,
        ..Default::default()
    });
    h.emit(Event::CrewStateLoaded { state });
    assert_eq!(fetches(&h.commands()), [url]);
    let crew = |h: &Harness| h.app().get_crews().row_data(0).expect("crew 0");
    assert!(!crew(&h).has_banner);
    h.emit(Event::MediaImageLoaded {
        url: url.into(),
        bytes: png_bytes(),
    });
    assert!(crew(&h).has_banner, "the header draws the banner");
    assert_eq!(crew(&h).banner_url.as_str(), url);
}

#[test]
fn a_banner_image_is_cropped_to_the_link_card_and_fits_the_limit() {
    let img = image::RgbaImage::from_fn(3000, 1000, |x, y| {
        image::Rgba([(x % 251) as u8, (y % 241) as u8, ((x * y) % 239) as u8, 255])
    });
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png)
        .expect("png");
    let (jpeg, _, w, h) =
        crate::callbacks::banner_jpeg_for_test(&png.into_inner()).expect("a banner");
    assert_eq!((w, h), (1200, 630));
    assert!(jpeg.len() <= 400 << 10, "{} bytes", jpeg.len());
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "a JPEG");
}

// ── 4d: the custom invite link ──────────────────────────────────────────────

fn names(h: &Harness) -> crate::InviteNamePicker<'_> {
    h.app().global::<crate::InviteNamePicker>()
}

fn friend_feed() -> FeedResponse {
    quests_feed(
        "user-1",
        serde_json::json!({"friend": {"done_ms": MADE_MS, "user_id": "u-ida", "name": "Ida"}}),
        serde_json::json!({}),
    )
}

/// Open the second gift (the link): the banner is opened first.
fn open_link_face(h: &mut Harness) {
    {
        let settings = h.settings();
        crate::sealed::mark_opened(
            &mut settings.borrow_mut(),
            "crew-0",
            "quest:made:crew-0",
            crate::head2head::now_ms(),
        );
    }
    h.emit(Event::FeedLoaded {
        response: friend_feed(),
    });
    open_face_action(h, "Set the link");
}

#[test]
fn set_the_link_checks_the_suggested_name_and_saves_a_free_one() {
    let mut h = Harness::new();
    on_crew(&mut h);
    h.commands();
    open_link_face(&mut h);
    assert!(names(&h).get_open());
    assert_eq!(
        names(&h).get_name().as_str(),
        "crew0",
        "a name made from the crew name"
    );
    let sent = h.commands();
    assert!(
        sent.iter()
            .any(|c| matches!(c, Command::CheckInviteName { name, .. } if name == "crew0")),
        "{sent:?}"
    );
    assert_eq!(names(&h).get_state().as_str(), "checking");

    // An answer for another name is old: it changes nothing.
    h.emit(Event::InviteNameChecked {
        crew_id: "crew-0".into(),
        name: "other".into(),
        state: "taken".into(),
    });
    assert_eq!(names(&h).get_state().as_str(), "checking");
    h.emit(Event::InviteNameChecked {
        crew_id: "crew-0".into(),
        name: "crew0".into(),
        state: "free".into(),
    });
    assert_eq!(names(&h).get_state().as_str(), "free");

    h.click_label("Save");
    assert!(
        h.commands()
            .iter()
            .any(|c| matches!(c, Command::SetInviteName { crew_id, name }
            if crew_id == "crew-0" && name == "crew0")),
        "Save sets the name"
    );
    h.emit(Event::CrewGiftChanged {
        crew_id: "crew-0".into(),
        gift: "link".into(),
    });
    assert!(!names(&h).get_open(), "a saved name closes the screen");
}

#[test]
fn a_name_in_the_shape_of_a_code_is_refused_without_a_call() {
    let mut h = Harness::new();
    on_crew(&mut h);
    open_link_face(&mut h);
    h.commands();
    names(&h).set_name("abcd1234".into());
    names(&h).invoke_edited("abcd1234".into());
    assert_eq!(names(&h).get_state().as_str(), "not_allowed");
    assert!(
        h.commands().is_empty(),
        "no check for a name the server refuses"
    );
    assert!(h
        .controls_labelled("Save")
        .iter()
        .all(|c| !c.accessible_enabled().unwrap_or(true)));
}

#[test]
fn a_crew_with_a_custom_name_shares_that_link_and_a_copy_is_reported() {
    let mut h = Harness::new();
    on_crew(&mut h);
    let state = mello_core::crew_state::CrewState {
        crew_id: "crew-0".into(),
        name: "Crew 0".into(),
        invite_name: "brawlbros".into(),
        ..Default::default()
    };
    h.emit(Event::CrewStateLoaded { state });
    h.commands();
    h.app().invoke_invite_share_requested();
    assert!(h.app().get_invite_share_open());
    assert_eq!(
        h.app().get_invite_share_url().as_str(),
        "https://m3llo.app/join/brawlbros"
    );
    h.app().invoke_invite_copy_link();
    let sent = h.commands();
    assert!(
        !sent
            .iter()
            .any(|c| matches!(c, Command::CreateInviteCode { .. })),
        "no new code for a crew with a name"
    );
    assert!(
        sent.iter()
            .any(|c| matches!(c, Command::ReportInviteCopied { crew_id } if crew_id == "crew-0")),
        "{sent:?}"
    );
}

#[test]
fn the_invite_name_rule_matches_the_server() {
    use crate::callbacks::invite_name_ok_for_test as ok;
    for (name, want) in [
        ("brawlbros", true),
        ("bob-test-26", true),
        ("ab-cd1234", true),
        ("abc", true),
        ("ab", false),
        ("-ab", false),
        ("a--b", false),
        ("admin", false),
        ("abcd1234", false),
        ("abcd-1234", false),
        ("Brawl", false),
        ("brawl_bros", false),
    ] {
        assert_eq!(ok(name), want, "{name}");
    }
}

// ── 4e: crew emoji ──────────────────────────────────────────────────────────

fn emoji_ui(h: &Harness) -> crate::CrewEmojiUi<'_> {
    h.app().global::<crate::CrewEmojiUi>()
}

const GG: &str = "https://media.example/crew-emoji/crew-0/gg.png";

/// Crew state with the emoji :gg: (added by Ida) for the viewer, the owner.
fn crew_with_gg(h: &mut Harness) {
    h.app().set_user_id("user-1".into());
    let state = mello_core::crew_state::CrewState {
        crew_id: "crew-0".into(),
        name: "Crew 0".into(),
        my_role: 0,
        emoji: vec![mello_core::crew_state::CrewEmoji {
            id: "e1".into(),
            name: "gg".into(),
            url: GG.into(),
            added_by: "u-ida".into(),
            added_by_name: "Ida".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    h.emit(Event::CrewStateLoaded { state });
}

#[test]
fn crew_emoji_load_into_the_picker() {
    let mut h = Harness::new();
    on_crew(&mut h);
    h.commands();
    crew_with_gg(&mut h);
    assert_eq!(fetches(&h.commands()), [GG], "the image is asked for");
    let row = |h: &Harness| emoji_ui(h).get_emoji().row_data(0).expect(":gg:");
    assert_eq!(row(&h).name.as_str(), "gg");
    assert!(!row(&h).has_image);
    assert!(row(&h).can_remove, "the owner removes any emoji");
    assert_eq!(row(&h).added_by.as_str(), "Ida");
    assert_eq!(emoji_ui(&h).get_free(), 4);
    assert!(
        emoji_ui(&h).get_enabled(),
        "a crew with no quests in the feed has emoji"
    );
    h.emit(Event::MediaImageLoaded {
        url: GG.into(),
        bytes: png_bytes(),
    });
    assert!(row(&h).has_image, "the image arrives in the picker");
}

#[test]
fn a_crew_emoji_draws_in_reactions_and_alone_in_a_message() {
    let mut h = Harness::new();
    on_crew(&mut h);
    crew_with_gg(&mut h);
    h.emit(Event::MediaImageLoaded {
        url: GG.into(),
        bytes: png_bytes(),
    });
    let mut alone = chat_message_fixture("c1", "alice", 0);
    alone.content = ":gg:".into();
    alone.display_body = ":gg:".into();
    let mut words = chat_message_fixture("c2", "bob", 1);
    words.content = ":gg: nice".into();
    words.display_body = ":gg: nice".into();
    let mut unknown = chat_message_fixture("c3", "bob", 2);
    unknown.content = ":nope:".into();
    unknown.display_body = ":nope:".into();
    h.emit(Event::MessagesLoaded {
        messages: vec![alone, words, unknown],
        has_more_history: false,
    });
    h.emit(Event::ChatReactionsLoaded {
        crew_id: "crew-0".into(),
        items: vec![reaction_update(
            "c2",
            &[(":gg:", &["u-ida"]), ("🔥", &["user-1"])],
        )],
    });
    let rows = h.app().get_messages();
    let row = |id: &str| {
        (0..rows.row_count())
            .filter_map(|i| rows.row_data(i))
            .find(|r| r.message_id == id)
            .expect("row")
    };
    assert_eq!(
        row("c1").crew_emoji.row_count(),
        1,
        "one crew emoji alone draws big"
    );
    assert_eq!(row("c2").crew_emoji.row_count(), 0, "with words it is text");
    assert_eq!(
        row("c3").crew_emoji.row_count(),
        0,
        "an emoji the crew has not is text"
    );
    let chips: Vec<(String, bool)> = row("c2")
        .reactions
        .iter()
        .map(|r| (r.emoji.to_string(), r.has_image))
        .collect();
    assert!(chips.contains(&(":gg:".into(), true)), "{chips:?}");
    assert!(chips.contains(&("🔥".into(), false)), "{chips:?}");
}

#[test]
fn add_emoji_opens_the_screen_and_remove_sends_the_command() {
    let mut h = Harness::new();
    on_crew(&mut h);
    crew_with_gg(&mut h);
    {
        let settings = h.settings();
        for step in ["made", "friend"] {
            crate::sealed::mark_opened(
                &mut settings.borrow_mut(),
                "crew-0",
                &crate::sealed::quest_key(step, "crew-0"),
                crate::head2head::now_ms(),
            );
        }
    }
    h.emit(Event::FeedLoaded {
        response: quests_feed(
            "user-1",
            serde_json::json!({"friend": {"user_id": "u-ida"}, "call": {"user_ids": ["u-ida"]}}),
            serde_json::json!({"invite_name": "x1y2z", "banner_url": "u"}),
        ),
    });
    open_face_action(&mut h, "Add emoji");
    assert!(emoji_ui(&h).get_open(), "Add emoji opens the screen");
    // Wait a frame: the modal is a conditional element.
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    h.commands();
    h.click_label("Remove :gg:");
    assert!(
        h.commands()
            .iter()
            .any(|c| matches!(c, Command::RemoveCrewEmoji { crew_id, name }
            if crew_id == "crew-0" && name == "gg")),
        "Remove sends the command"
    );

    emoji_ui(&h).set_busy(true);
    h.emit(Event::CrewGiftChanged {
        crew_id: "crew-0".into(),
        gift: "emoji".into(),
    });
    assert!(!emoji_ui(&h).get_open(), "an added emoji closes the screen");
}

#[test]
fn the_picker_offers_a_free_slot_that_opens_the_screen() {
    let mut h = Harness::new();
    on_crew(&mut h);
    crew_with_gg(&mut h);
    h.app().set_emoji_popover_open(true);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    assert_eq!(
        h.controls_labelled(":gg:").len(),
        1,
        "the crew's emoji are in the picker"
    );
    assert_eq!(
        h.controls_labelled("Add emoji").len(),
        4,
        "a + for each free slot"
    );
    h.click_label("Add emoji");
    assert!(emoji_ui(&h).get_open());
    assert!(!h.app().get_emoji_popover_open(), "the picker closes");
}

#[test]
fn an_emoji_image_is_a_square_png_and_names_follow_the_server() {
    let img = image::RgbaImage::from_pixel(300, 200, image::Rgba([10, 20, 30, 255]));
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png)
        .expect("png");
    let (out, _, w, h) = crate::callbacks::emoji_png_for_test(&png.into_inner()).expect("an emoji");
    assert_eq!((w, h), (128, 128));
    assert_eq!(&out[1..4], b"PNG");
    use crate::callbacks::emoji_name_ok_for_test as ok;
    assert!(ok("gg") && ok("ida_rage") && ok("w45"));
    assert!(!ok("g") && !ok("Ida") && !ok("ida-rage") && !ok(&"a".repeat(25)));
}

/// A member's emoji slots open with the feed that has the first call done.
/// Crew state loaded before it, so only the feed load can open them (seen in
/// the quest journey, 2026-11-07).
#[test]
fn the_emoji_slots_open_with_the_feed_of_the_first_call() {
    let mut h = Harness::new();
    on_crew(&mut h);
    h.emit(Event::FeedLoaded {
        response: quests_feed("u-ida", serde_json::json!({}), serde_json::json!({})),
    });
    let enabled = |h: &Harness| h.app().global::<crate::CrewEmojiUi>().get_enabled();
    assert!(!enabled(&h), "no slots before the first call");
    h.emit(Event::FeedLoaded {
        response: quests_feed(
            "u-ida",
            serde_json::json!({"call": {"user_ids": ["u-ida", "user-1"]}}),
            serde_json::json!({}),
        ),
    });
    assert!(enabled(&h), "the feed of the first call opens the slots");
}
