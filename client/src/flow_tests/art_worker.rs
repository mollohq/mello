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
        .expect("the grid asked for the header slot")
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

/// A recent games card of two PCs: the poster grid, header frames.
fn recent_feed(games: serde_json::Value) -> FeedResponse {
    let mut entry = recent_games_entry(games);
    entry.data["pc_count"] = serde_json::json!(2);
    feed_of(vec![entry])
}

/// A recent games card of one PC: the list, 48 px square rows.
fn recent_list_feed(games: serde_json::Value) -> FeedResponse {
    feed_of(vec![recent_games_entry(games)])
}

fn first_tile(h: &Harness) -> crate::RecentGameTile {
    h.app()
        .get_feed_cards()
        .iter()
        .find(|c| c.card_type.as_str() == "recent-games")
        .expect("the recent games card")
        .recent_games
        .row_data(0)
        .expect("a tile")
}

#[test]
fn a_tile_without_local_art_asks_the_worker_and_shows_the_header_crop() {
    let fake = FakeArt::start();
    fake.put("steam-2300340", "header", jpeg(1280, 720));
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    h.emit(Event::FeedLoaded {
        response: recent_feed(serde_json::json!([
            { "game_id": "steam-2300340", "game_name": "Night Stones", "minutes": 600 }
        ])),
    });
    assert!(!first_tile(&h).has_capsule, "monogram until the art lands");
    let (ids, slot) = header_request(&mut h);
    assert_eq!(slot, ArtSlot::Header);
    assert_eq!(ids[0], "steam-2300340");

    assert_eq!(serve(&mut h, &ids, slot).as_deref(), Some("steam-2300340"));
    let tile = first_tile(&h);
    assert!(tile.has_capsule, "the tile shows the Worker art");
    // 16 by 9 art is cut to the 460 by 215 header frame.
    let size = tile.capsule.size();
    assert_eq!((size.width, size.height), (460, 215));
    assert_eq!(fake.count("steam-2300340", "header"), 1);
}

#[test]
fn the_hit_is_on_disk_so_a_restart_asks_nothing() {
    let fake = FakeArt::start();
    fake.put("steam-2300340", "header", jpeg(1280, 720));
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    let feed = recent_feed(serde_json::json!([
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
    assert!(first_tile(&h).has_capsule, "decoded from the file");
    assert!(
        header_requests(&mut h).is_empty(),
        "a fresh file is not asked again"
    );
    assert_eq!(fake.count("steam-2300340", "header"), 1);
}

#[test]
fn a_404_falls_through_to_the_monogram_and_is_not_asked_again() {
    let fake = FakeArt::start();
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    let feed = recent_feed(serde_json::json!([
        { "game_id": "steam-999999", "game_name": "No Art Game", "minutes": 60 }
    ]));
    h.emit(Event::FeedLoaded {
        response: feed.clone(),
    });
    let (ids, slot) = header_request(&mut h);
    assert_eq!(serve(&mut h, &ids, slot), None, "the Worker has nothing");
    let asked = fake.count("steam-999999", "header");
    assert_eq!(asked, 1);
    assert!(!first_tile(&h).has_capsule, "the monogram stays");

    // The feed is built again: the miss is remembered, so no new request.
    h.emit(Event::FeedLoaded { response: feed });
    assert!(header_requests(&mut h).is_empty(), "negative cache");
    assert_eq!(fake.count("steam-999999", "header"), asked);
    assert!(!first_tile(&h).has_capsule);
}

#[test]
fn art_older_than_a_week_shows_at_once_and_is_checked_with_its_etag() {
    let fake = FakeArt::start();
    fake.put("steam-2300340", "header", jpeg(1280, 720));
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    let feed = recent_feed(serde_json::json!([
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
    assert!(first_tile(&h).has_capsule, "the old file still shows");
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
    let live = h
        .app()
        .get_feed_cards()
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
fn the_grid_poster_frame_is_the_header_shape() {
    let fake = FakeArt::start();
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    let both = serde_json::json!(["user-1", "u2"]);
    h.emit(Event::FeedLoaded {
        response: two_member_feed_with_games(
            serde_json::json!([
                { "game_id": "steam-2300340", "game_name": "Night Stones", "minutes": 600, "member_ids": both },
                { "game_id": "steam-252950", "game_name": "Rocket League", "minutes": 400, "member_ids": both }
            ]),
            None,
        ),
    });
    let frames: Vec<_> = ElementHandle::find_by_element_type_name(h.app(), "RecentGamePoster")
        .flat_map(|p| {
            let pos = p.absolute_position();
            let w = p.size().width;
            ElementHandle::find_by_element_type_name(h.app(), "GameArt")
                .filter(move |a| {
                    (a.absolute_position().x - pos.x).abs() < 0.5 && a.size().width == w
                })
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(!frames.is_empty(), "the posters draw a header frame");
    for f in frames {
        let s = f.size();
        let ratio = s.width / s.height;
        assert!(
            (ratio - 460.0 / 215.0).abs() < 0.02,
            "{s:?} is not 460 by 215"
        );
    }
}

fn an_icon() -> slint::Image {
    crate::avatar::rgba_to_image(&[255u8; 4 * 4 * 4], 4, 4)
}

/// The ladder is per slot. A game with an exe icon and Worker art: the art
/// frames (tile) show the art and never the icon; the icon slot (pill) shows
/// the icon and does not ask for the Worker icon.
#[test]
fn an_exe_icon_stays_in_the_icon_slot_and_the_art_fills_the_frames() {
    let fake = FakeArt::start();
    fake.put("steam-2300340", "header", jpeg(1280, 720));
    fake.put("steam-2300340", "icon", jpeg(90, 90));
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    h.ctx()
        .game_icon_cache
        .borrow_mut()
        .insert("steam-2300340".into(), an_icon());

    h.emit(Event::FeedLoaded {
        response: recent_feed(serde_json::json!([
            { "game_id": "steam-2300340", "game_name": "Night Stones", "minutes": 600 }
        ])),
    });
    let (ids, slot) = header_request(&mut h);
    assert_eq!(slot, ArtSlot::Header, "the frame asks for header art");
    serve(&mut h, &ids, slot);
    let tile = first_tile(&h);
    assert!(tile.has_capsule, "the art is in the header frame");
    assert!(
        tile.has_icon,
        "the exe icon is in the square slot of the tile, not in the header frame"
    );

    h.emit(game_detected(
        "steam-2300340",
        "Night Stones",
        now_ms() - 60_000,
    ));
    assert!(
        h.app().get_game_has_runtime_icon(),
        "the pill shows the icon"
    );
    assert!(!h.app().get_game_has_capsule());
    assert!(
        art_requests(&mut h)
            .iter()
            .all(|(_, s)| *s != ArtSlot::Icon),
        "an exe icon needs no Worker icon"
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

fn count_of_type(h: &Harness, name: &str) -> usize {
    ElementHandle::find_by_element_type_name(h.app(), name).count()
}

/// The list row is a 48 px square: it asks for the icon slot, draws the
/// Worker icon in a `GameIcon` and never asks for the header.
#[test]
fn the_recent_games_list_row_draws_the_worker_icon() {
    let fake = FakeArt::start();
    fake.put("steam-2300340", "icon", jpeg(90, 90));
    fake.put("steam-2300340", "header", jpeg(1280, 720));
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    h.emit(Event::FeedLoaded {
        response: recent_list_feed(serde_json::json!([
            { "game_id": "steam-2300340", "game_name": "Night Stones", "minutes": 600 }
        ])),
    });
    assert_eq!(count_of_type(&h, "RecentGameTileCell"), 1, "the list");
    assert!(!first_tile(&h).has_icon, "monogram until the art lands");
    assert_eq!(count_of_type(&h, "GameIcon"), 0);
    let asks = art_requests(&mut h);
    assert!(
        asks.iter().all(|(_, s)| *s == ArtSlot::Icon),
        "a list asks for the icon slot only: {asks:?}"
    );
    let (ids, slot) = asks.into_iter().next().expect("the row asked for art");
    serve(&mut h, &ids, slot);

    let tile = first_tile(&h);
    assert!(tile.has_icon, "the Worker icon is on the tile");
    assert!(!tile.has_capsule);
    assert_eq!(count_of_type(&h, "GameIcon"), 1, "the cell draws the image");
    assert_eq!(count_of_type(&h, "GameBadge"), 0, "no monogram");
}

/// Art in one slot does not stop the search in the other: a tile that has its
/// header still gets its icon when the icon lands.
#[test]
fn refresh_looks_up_each_slot_of_a_tile_on_its_own() {
    let fake = FakeArt::start();
    fake.put("steam-2300340", "header", jpeg(1280, 720));
    fake.put("steam-2300340", "icon", jpeg(90, 90));
    let mut h = Harness::with_art_server(&fake.base);
    app_with_crew_of(&mut h, 1);
    h.emit(Event::FeedLoaded {
        response: recent_feed(serde_json::json!([
            { "game_id": "steam-2300340", "game_name": "Night Stones", "minutes": 600 }
        ])),
    });
    let asks = art_requests(&mut h);
    let header = asks.iter().find(|(_, s)| *s == ArtSlot::Header).cloned();
    let icon = asks.iter().find(|(_, s)| *s == ArtSlot::Icon).cloned();
    let (hids, hslot) = header.expect("the grid asks for the header");
    let (iids, islot) = icon.expect("the grid keeps a list for narrow slots");
    serve(&mut h, &hids, hslot);
    let tile = first_tile(&h);
    assert!(tile.has_capsule && !tile.has_icon);
    serve(&mut h, &iids, islot);
    let tile = first_tile(&h);
    assert!(tile.has_capsule, "the header stays");
    assert!(tile.has_icon, "the icon landed after the header");
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
