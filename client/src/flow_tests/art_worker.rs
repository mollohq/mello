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

/// Do what core does for the request, then deliver the reply.
fn serve(h: &mut Harness, ids: &[String], slot: ArtSlot) -> Option<String> {
    let store = h.ctx().art.clone();
    let got = block_on(store.walk(ids, slot));
    if let Some(game_id) = got.clone() {
        h.emit(Event::GameArtLoaded { game_id, slot });
    }
    got
}

fn recent_feed(games: serde_json::Value) -> FeedResponse {
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
    let asks = art_requests(&mut h);
    assert!(!asks.is_empty(), "the UI asked the Worker");
    let (ids, slot) = &asks[0];
    assert_eq!(*slot, ArtSlot::Header);
    assert_eq!(ids[0], "steam-2300340");

    assert_eq!(serve(&mut h, ids, *slot).as_deref(), Some("steam-2300340"));
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
    let (ids, slot) = art_requests(&mut h).remove(0);
    serve(&mut h, &ids, slot);
    // The memory copy is gone; the file stays.
    h.ctx().game_icon_cache.borrow_mut().clear();
    h.emit(Event::FeedLoaded { response: feed });
    assert!(first_tile(&h).has_capsule, "decoded from the file");
    assert!(
        art_requests(&mut h).is_empty(),
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
    let (ids, slot) = art_requests(&mut h).remove(0);
    assert_eq!(serve(&mut h, &ids, slot), None, "the Worker has nothing");
    let asked = fake.count("steam-999999", "header");
    assert_eq!(asked, 1);
    assert!(!first_tile(&h).has_capsule, "the monogram stays");

    // The feed is built again: the miss is remembered, so no new request.
    h.emit(Event::FeedLoaded { response: feed });
    assert!(art_requests(&mut h).is_empty(), "negative cache");
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
    let (ids, slot) = art_requests(&mut h).remove(0);
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
    let (ids, slot) = art_requests(&mut h).remove(0);
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
