//! The art Worker rung of the tile ladder, against a fake art server:
//! a hit, a 404 with its negative cache, the ETag check, and the crop frames.

use mello_core::art::fake::FakeArt;
use mello_core::art::ArtSlot;

use super::*;

fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let img = image::RgbImage::from_pixel(w, h, image::Rgb([30, 140, 90]));
    image::codecs::jpeg::JpegEncoder::new(&mut out)
        .encode_image(&img)
        .expect("encode the art");
    out
}

fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(f)
}

/// The art fetches the UI asked for since the last call.
fn art_requests(h: &mut Harness) -> Vec<(Vec<String>, ArtSlot)> {
    h.commands()
        .into_iter()
        .filter_map(|c| match c {
            Command::FetchGameArt { ids, slot } => Some((ids, slot)),
            _ => None,
        })
        .collect()
}

/// The header fetch among the requests since the last call.
fn header_request(h: &mut Harness) -> (Vec<String>, ArtSlot) {
    art_requests(h)
        .into_iter()
        .find(|(_, s)| *s == ArtSlot::Header)
        .expect("the frame asked for the header slot")
}

/// The header fetches among the requests since the last call.
fn header_requests(h: &mut Harness) -> Vec<(Vec<String>, ArtSlot)> {
    art_requests(h)
        .into_iter()
        .filter(|(_, s)| *s == ArtSlot::Header)
        .collect()
}

/// Do what core does for the request, then deliver the reply.
fn serve(h: &mut Harness, ids: &[String], slot: ArtSlot) -> Option<String> {
    let store = h.ctx().art.clone();
    let got = block_on(store.walk(ids, slot));
    if let Some(game_id) = got.clone() {
        h.emit(Event::GameArtLoaded { game_id, slot });
    }
    got
}

/// A feed whose day on the board has this top game. The board draws its art
/// in a header frame.
fn top_game_feed(games: serde_json::Value) -> FeedResponse {
    let g = &games[0];
    feed_of(vec![feed_entry(
        "daily-1",
        "daily-recap",
        serde_json::json!({
            "game_id": g["game_id"], "top_game": g["game_name"],
            "day_start": now_ms(), "total_min": 60, "session_count": 1,
        }),
    )])
}

/// The art of the board's top game. Empty while the board draws no art.
fn top_art(h: &Harness) -> slint::Image {
    h.app().get_feed_zones().today_top.art
}

fn has_art(h: &Harness) -> bool {
    top_art(h).size().width > 0
}

#[test]
fn a_top_game_without_local_art_asks_the_worker_and_shows_the_header_crop() {
    let fake = FakeArt::start();
    fake.put("steam-2300340", "header", jpeg(1280, 720));
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    h.emit(Event::FeedLoaded {
        response: top_game_feed(serde_json::json!([
            { "game_id": "steam-2300340", "game_name": "Night Stones", "minutes": 600 }
        ])),
    });
    assert!(!has_art(&h), "no art until it lands");
    let (ids, slot) = header_request(&mut h);
    assert_eq!(slot, ArtSlot::Header);
    assert_eq!(ids[0], "steam-2300340");

    assert_eq!(serve(&mut h, &ids, slot).as_deref(), Some("steam-2300340"));
    assert!(has_art(&h), "the board shows the Worker art");
    // 16 by 9 art is cut to the 460 by 215 header frame.
    let size = top_art(&h).size();
    assert_eq!((size.width, size.height), (460, 215));
    assert_eq!(fake.count("steam-2300340", "header"), 1);
}

#[test]
fn the_hit_is_on_disk_so_a_restart_asks_nothing() {
    let fake = FakeArt::start();
    fake.put("steam-2300340", "header", jpeg(1280, 720));
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    let feed = top_game_feed(serde_json::json!([
        { "game_id": "steam-2300340", "game_name": "Night Stones", "minutes": 600 }
    ]));
    h.emit(Event::FeedLoaded {
        response: feed.clone(),
    });
    let (ids, slot) = header_request(&mut h);
    serve(&mut h, &ids, slot);
    // The memory copy is gone; the file stays.
    h.ctx().game_icon_cache.borrow_mut().clear();
    h.emit(Event::FeedLoaded { response: feed });
    assert!(has_art(&h), "decoded from the file");
    assert!(
        header_requests(&mut h).is_empty(),
        "a fresh file is not asked again"
    );
    assert_eq!(fake.count("steam-2300340", "header"), 1);
}

#[test]
fn a_404_falls_through_to_no_art_and_is_not_asked_again() {
    let fake = FakeArt::start();
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    let feed = top_game_feed(serde_json::json!([
        { "game_id": "steam-999999", "game_name": "No Art Game", "minutes": 60 }
    ]));
    h.emit(Event::FeedLoaded {
        response: feed.clone(),
    });
    let (ids, slot) = header_request(&mut h);
    assert_eq!(serve(&mut h, &ids, slot), None, "the Worker has nothing");
    let asked = fake.count("steam-999999", "header");
    assert_eq!(asked, 1);
    assert!(!has_art(&h), "no art stays");

    // The feed is built again: the miss is remembered, so no new request.
    h.emit(Event::FeedLoaded { response: feed });
    assert!(header_requests(&mut h).is_empty(), "negative cache");
    assert_eq!(fake.count("steam-999999", "header"), asked);
    assert!(!has_art(&h));
}

#[test]
fn art_older_than_a_week_shows_at_once_and_is_checked_with_its_etag() {
    let fake = FakeArt::start();
    fake.put("steam-2300340", "header", jpeg(1280, 720));
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    let feed = top_game_feed(serde_json::json!([
        { "game_id": "steam-2300340", "game_name": "Night Stones", "minutes": 600 }
    ]));
    h.emit(Event::FeedLoaded {
        response: feed.clone(),
    });
    let (ids, slot) = header_request(&mut h);
    serve(&mut h, &ids, slot);

    let path = match h.ctx().art.lookup("steam-2300340", ArtSlot::Header) {
        mello_core::art::ArtLookup::Fresh(p) => p,
        other => panic!("expected a fresh file, got {other:?}"),
    };
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("open")
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 86400))
        .expect("age the file");
    h.ctx().game_icon_cache.borrow_mut().clear();

    h.emit(Event::FeedLoaded { response: feed });
    assert!(has_art(&h), "the old file still shows");
    let (ids, slot) = header_request(&mut h);
    assert_eq!(ids, vec!["steam-2300340".to_string()], "one id is checked");
    serve(&mut h, &ids, slot);
    let seen = fake.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(seen[1].if_none_match.is_some(), "the ETag was sent");
    assert_eq!(seen[1].status, 304, "unchanged art is not downloaded again");
}

#[test]
fn the_live_card_gets_the_hero_crop_and_the_wide_slot() {
    let fake = FakeArt::start();
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![]),
    });
    let ids = crate::game_icons::worker_ids(
        "counter-strike-2",
        "counter-strike-2",
        "Counter-Strike 2",
        &[],
    );
    assert!(!ids.is_empty(), "the catalogue names the game");
    fake.put(&ids[0], "hero", jpeg(1920, 1080));
    h.emit(game_detected(
        "counter-strike-2",
        "Counter-Strike 2",
        now_ms() - 60_000,
    ));
    let asks = art_requests(&mut h);
    let (hero_ids, slot) = asks
        .iter()
        .find(|(_, s)| *s == ArtSlot::Hero)
        .expect("the live card asked for the hero")
        .clone();
    assert_eq!(
        serve(&mut h, &hero_ids, slot).as_deref(),
        Some(ids[0].as_str())
    );
    let live = zone_model(&h)
        .iter()
        .find(|c| c.card_type.as_str() == "live-session")
        .expect("the live card");
    assert!(live.live.has_poster, "the poster arrived");
    let size = live.live.poster.size();
    // 1920 by 1080 is cut to 1920 by 620 and kept at 1280 wide.
    assert_eq!((size.width, size.height), (1280, 413));
}

#[test]
fn crop_cover_fills_the_frame_from_the_centre() {
    use crate::game_icons::{crop_cover, HEADER_FRAME, HERO_FRAME};
    let wide = image::RgbaImage::from_pixel(1920, 1080, image::Rgba([1, 2, 3, 255]));
    assert_eq!(crop_cover(wide, HERO_FRAME).dimensions(), (1920, 620));
    let wide = image::RgbaImage::from_pixel(1280, 720, image::Rgba([1, 2, 3, 255]));
    assert_eq!(crop_cover(wide, HEADER_FRAME).dimensions(), (1280, 598));
    // Narrower than the frame: the height is cut instead.
    let tall = image::RgbaImage::from_pixel(528, 748, image::Rgba([1, 2, 3, 255]));
    assert_eq!(crop_cover(tall, HEADER_FRAME).dimensions(), (528, 246));
    // The centre stays: the top and bottom rows go, not the middle.
    let mut img = image::RgbaImage::from_pixel(100, 100, image::Rgba([0, 0, 0, 255]));
    for x in 0..100 {
        img.put_pixel(x, 50, image::Rgba([255, 0, 0, 255]));
    }
    let cut = crop_cover(img, (2, 1));
    assert_eq!(cut.dimensions(), (100, 50));
    assert_eq!(
        cut.get_pixel(0, 25).0,
        [255, 0, 0, 255],
        "row 50 is the middle"
    );
}

#[test]
fn the_pill_without_an_exe_icon_takes_the_worker_icon() {
    let fake = FakeArt::start();
    fake.put("steam-2300340", "icon", jpeg(90, 90));
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    h.emit(game_detected(
        "steam-2300340",
        "Night Stones",
        now_ms() - 60_000,
    ));
    assert!(
        !h.app().get_game_has_runtime_icon(),
        "monogram until it lands"
    );
    let (ids, slot) = art_requests(&mut h)
        .into_iter()
        .find(|(_, s)| *s == ArtSlot::Icon)
        .expect("the pill asked for the icon slot");
    serve(&mut h, &ids, slot);
    assert!(
        h.app().get_game_has_runtime_icon(),
        "the Worker icon fills the pill as an icon"
    );
    assert!(!h.app().get_game_has_capsule());
}

fn stats_of(id: &str) -> mello_core::crew_events::UserGameStats {
    mello_core::crew_events::UserGameStats {
        game_id: id.into(),
        wins: 3,
        losses: 1,
        ..Default::default()
    }
}

/// The stats strip and the profile cards are square frames: they ask for the
/// icon slot and draw the Worker icon, with the monogram until it lands.
#[test]
fn the_stats_strip_and_profile_cards_take_the_worker_icon() {
    let fake = FakeArt::start();
    fake.put("steam-2300340", "icon", jpeg(90, 90));
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    h.emit(Event::UserGameStatsLoaded {
        games: vec![stats_of("steam-2300340")],
    });
    assert!(!h.app().get_you_strip().has_icon, "monogram until it lands");
    let (ids, slot) = art_requests(&mut h)
        .into_iter()
        .find(|(_, s)| *s == ArtSlot::Icon)
        .expect("the strip asked for the icon slot");
    serve(&mut h, &ids, slot);

    assert!(h.app().get_you_strip().has_icon, "the strip has the icon");
    let cards = h.app().get_stats_profile_games();
    assert_eq!(cards.row_count(), 1);
    assert!(cards.row_data(0).expect("a card").has_icon);

    // A later build finds the file on disk and asks nothing.
    h.emit(Event::UserGameStatsLoaded {
        games: vec![stats_of("steam-2300340")],
    });
    assert!(h.app().get_you_strip().has_icon);
    assert!(art_requests(&mut h).is_empty());
}

/// The Games settings tab: an integration row and a hidden games row are
/// square frames. Both take the Worker icon, and a landing icon reaches only
/// the rows that it names.
#[test]
fn the_settings_game_rows_take_the_worker_icon() {
    let fake = FakeArt::start();
    fake.put("steam-2300340", "icon", jpeg(90, 90));
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    h.emit(Event::GamesSettings {
        games: vec![mello_core::events::GameIntegrationStatus {
            game_id: "steam-2300340".into(),
            name: "Night Stones".into(),
            short_name: "NS".into(),
            color: "#6E8FB8".into(),
            installed: Some(true),
            writes_files: false,
            note: String::new(),
            account_link: None,
        }],
    });
    h.emit(Event::InstalledGames {
        games: vec![
            mello_core::events::InstalledGameInfo {
                game_id: "steam-2300340".into(),
                name: "Night Stones".into(),
                steam_hidden: false,
            },
            mello_core::events::InstalledGameInfo {
                game_id: "steam-999999".into(),
                name: "No Art Game".into(),
                steam_hidden: false,
            },
        ],
    });
    assert!(!h.app().get_games_list().row_data(0).unwrap().has_icon);
    assert!(!h.app().get_installed_games().row_data(0).unwrap().has_icon);
    let asks = art_requests(&mut h);
    let (ids, slot) = asks
        .into_iter()
        .find(|(ids, s)| *s == ArtSlot::Icon && ids[0] == "steam-2300340")
        .expect("the rows asked for the icon slot");
    serve(&mut h, &ids, slot);

    assert!(h.app().get_games_list().row_data(0).unwrap().has_icon);
    let installed = h.app().get_installed_games();
    assert!(installed.row_data(0).unwrap().has_icon);
    assert!(
        !installed.row_data(1).unwrap().has_icon,
        "a game that the icon does not name keeps its monogram"
    );
}

/// The stream source menu row of the detected game is a square frame. With no
/// exe icon it takes the Worker icon, as the pill beside it does.
#[test]
fn the_stream_source_menu_takes_the_worker_icon() {
    let fake = FakeArt::start();
    fake.put("steam-2300340", "icon", jpeg(90, 90));
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    h.app()
        .window()
        .set_size(slint::PhysicalSize::new(1600, 1000));
    h.emit(game_detected(
        "steam-2300340",
        "Night Stones",
        now_ms() - 60_000,
    ));
    h.app().set_source_menu_open(true);
    let menu_badges = |h: &Harness| {
        let menu = ElementHandle::find_by_element_type_name(h.app(), "StreamSourceMenu")
            .next()
            .expect("the menu is open");
        let (pos, size) = (menu.absolute_position(), menu.size());
        ElementHandle::find_by_element_type_name(h.app(), "GameBadge")
            .filter(|i| {
                let p = i.absolute_position();
                p.x >= pos.x
                    && p.x <= pos.x + size.width
                    && p.y >= pos.y
                    && p.y <= pos.y + size.height
            })
            .count()
    };
    assert_eq!(menu_badges(&h), 1, "the monogram until the icon lands");
    let (ids, slot) = art_requests(&mut h)
        .into_iter()
        .find(|(_, s)| *s == ArtSlot::Icon)
        .expect("the game asked for the icon slot");
    serve(&mut h, &ids, slot);
    assert_eq!(menu_badges(&h), 0, "the menu row draws the Worker icon");
}

/// The games played strip and the opened recap grid are header frames (460 by
/// 215). Their rows ask for the header slot and never the capsule slot.
#[test]
fn the_recap_games_strip_asks_for_header_art() {
    let fake = FakeArt::start();
    let ids = crate::game_icons::worker_ids("rocket-league", "rocket-league", "Rocket League", &[]);
    assert!(!ids.is_empty(), "the catalogue names the game");
    fake.put(&ids[0], "header", jpeg(1280, 720));
    let mut h = tall_recap_crew_in(Harness::with_art_server(&fake.base), week_with_everything());
    let asks = art_requests(&mut h);
    assert!(!asks.is_empty(), "the recap games asked for art");
    assert!(
        asks.iter().all(|(_, s)| *s != ArtSlot::Capsule),
        "no frame draws the capsule slot: {asks:?}"
    );
    let (hids, slot) = asks
        .iter()
        .find(|(i, s)| *s == ArtSlot::Header && i.contains(&ids[0]))
        .cloned()
        .expect("the strip asked for the header slot");
    assert_eq!(slot, ArtSlot::Header);
    serve(&mut h, &hids, slot);

    let card = zone_model(&h)
        .iter()
        .find(|c| c.card_type.as_str() == "recap")
        .expect("the recap card");
    let row = card
        .recap_games
        .iter()
        .find(|r| r.name.as_str() == "Rocket League")
        .expect("the game row");
    assert!(row.has_capsule, "the header art is in the row");
    assert!(!row.has_icon, "an art frame never takes an icon");
    let size = row.capsule.size();
    assert_eq!(
        (size.width, size.height),
        (460, 215),
        "cut to the header frame"
    );
}

/// The played card (a game session without a match record) is filled by the
/// header art of a catalogue game. Until the art lands it carries the game id
/// and the monogram; the GameArtLoaded event of the header slot puts the art
/// on the card, cut to the header frame.
#[test]
fn the_played_card_takes_the_header_art() {
    let fake = FakeArt::start();
    let ids = crate::game_icons::worker_ids("rocket-league", "rocket-league", "Rocket League", &[]);
    assert!(!ids.is_empty(), "the catalogue names the game");
    fake.put(&ids[0], "header", jpeg(1280, 720));
    fake.put(&ids[0], "icon", jpeg(90, 90));
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    let mut feed = zero_telemetry_game_session_feed();
    feed.zones.sessions[0].data["game_name"] = serde_json::json!("Rocket League");
    feed.zones.sessions[0].data["game_id"] = serde_json::json!("rocket-league");
    h.emit(Event::FeedLoaded { response: feed });

    let card = first_card(&h);
    assert_eq!(card.card_type.as_str(), "game-session");
    assert_eq!(card.game_matches, 0, "no record: the played card");
    assert_eq!(card.game_id.as_str(), "rocket-league");
    assert_eq!(
        card.game_monogram.as_str(),
        crate::recent_games::monogram("Rocket League"),
        "the monogram until the art lands"
    );
    assert!(!card.game_has_capsule && !card.game_has_icon);

    let asks = art_requests(&mut h);
    assert!(
        asks.iter().all(|(_, s)| *s == ArtSlot::Header),
        "the played card asks for the header slot only: {asks:?}"
    );
    let (hids, slot) = asks
        .into_iter()
        .find(|(i, _)| i.contains(&ids[0]))
        .expect("the card asked for the game's header");
    serve(&mut h, &hids, slot);

    let card = first_card(&h);
    assert!(card.game_has_capsule, "the header art is on the card");
    assert!(!card.game_has_icon, "an art frame never takes an icon");
    let size = card.game_capsule.size();
    assert_eq!(
        (size.width, size.height),
        (460, 215),
        "cut to the header frame"
    );
}

/// The tile of the card with the record is a 40 px square: it asks for the
/// icon slot, not the header, and draws the Worker icon uncropped from a
/// header.
#[test]
fn the_session_card_tile_takes_the_icon_slot() {
    let fake = FakeArt::start();
    fake.put("steam-2300340", "icon", jpeg(90, 90));
    fake.put("steam-2300340", "header", jpeg(1280, 720));
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    let mut feed = zero_telemetry_game_session_feed();
    feed.zones.sessions[0].data["game_name"] = serde_json::json!("Night Stones");
    feed.zones.sessions[0].data["game_id"] = serde_json::json!("steam-2300340");
    feed.zones.sessions[0].data["wins"] = serde_json::json!(2);
    feed.zones.sessions[0].data["losses"] = serde_json::json!(1);
    h.emit(Event::FeedLoaded { response: feed });
    let card = first_card(&h);
    assert!(!card.game_has_icon && !card.game_has_capsule);
    let asks = art_requests(&mut h);
    assert!(!asks.is_empty(), "the card asked for art");
    assert!(
        asks.iter().all(|(_, s)| *s == ArtSlot::Icon),
        "a square tile asks for the icon slot only: {asks:?}"
    );
    let (ids, slot) = asks.into_iter().next().expect("an ask");
    serve(&mut h, &ids, slot);
    let card = first_card(&h);
    assert!(card.game_has_icon, "the Worker icon is on the card");
    assert!(!card.game_has_capsule, "no header in a square frame");
    let size = card.game_icon.size();
    assert_eq!(
        (size.width, size.height),
        (90, 90),
        "the icon keeps its shape"
    );
}

/// The live card tile is a square too: the icon slot, and the hero for the
/// poster.
#[test]
fn the_live_card_tile_takes_the_icon_slot() {
    let fake = FakeArt::start();
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![]),
    });
    fake.put("steam-2300340", "icon", jpeg(90, 90));
    h.emit(game_detected(
        "steam-2300340",
        "Night Stones",
        now_ms() - 60_000,
    ));
    let asks = art_requests(&mut h);
    assert!(
        asks.iter().all(|(_, s)| *s != ArtSlot::Header),
        "the live card does not ask for a header: {asks:?}"
    );
    let asks_dbg = asks.clone();
    let (iids, slot) = asks
        .into_iter()
        .find(|(_, s)| *s == ArtSlot::Icon)
        .unwrap_or_else(|| panic!("the live card asked for the icon slot: {asks_dbg:?}"));
    serve(&mut h, &iids, slot);
    let live = zone_model(&h)
        .iter()
        .find(|c| c.card_type.as_str() == "live-session")
        .expect("the live card");
    assert!(live.game_has_icon && !live.game_has_capsule);
}
