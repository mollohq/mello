//! The testers on the ABOUT tab: seven octagons that open a Steam profile,
//! their avatars from a fake Steam Community, the cache and the monogram.

use mello_core::testers::fake::FakeSteam;
use mello_core::testers::{Fetched, Lookup, TESTERS};
use slint::Model;

use super::*;
use crate::TesterData;

const PROFILES: [(&str, &str); 7] = [
    ("Ostkatt", "https://steamcommunity.com/id/Larzon/"),
    (
        "Propotato",
        "https://steamcommunity.com/id/martin1337maden/",
    ),
    ("xtrÖm", "https://steamcommunity.com/id/xtroem/"),
    ("Erle", "https://steamcommunity.com/id/erlexd/"),
    (
        "Halvtokig",
        "https://steamcommunity.com/profiles/76561197974140093/",
    ),
    (
        "LipzzarN",
        "https://steamcommunity.com/profiles/76561198156554449/",
    ),
    (
        "Keyden",
        "https://steamcommunity.com/profiles/76561198173235561/",
    ),
];

fn jpeg() -> Vec<u8> {
    let mut out = Vec::new();
    let img = image::RgbImage::from_pixel(184, 184, image::Rgb([90, 120, 200]));
    image::codecs::jpeg::JpegEncoder::new(&mut out)
        .encode_image(&img)
        .expect("encode the avatar");
    out
}

fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(f)
}

fn rows(h: &Harness) -> Vec<TesterData> {
    h.app().get_settings_testers().iter().collect()
}

/// The slugs of the `FetchTesterAvatars` commands since the last call.
fn avatar_requests(h: &mut Harness) -> Vec<String> {
    h.commands()
        .into_iter()
        .flat_map(|c| match c {
            Command::FetchTesterAvatars { slugs } => slugs,
            _ => Vec::new(),
        })
        .collect()
}

#[test]
fn the_tab_has_seven_octagon_controls_each_with_an_image_or_a_monogram() {
    let mut h = Harness::new();
    about_tab(&mut h);
    h.click_label("ABOUT");
    assert_eq!(
        labelled_count(&h, "THE OG CREW THAT HELPED SHAPE M3LLO"),
        1,
        "the section label"
    );
    assert_eq!(
        labelled_count(
            &h,
            "They played the first builds and told me what was wrong."
        ),
        0,
        "no line under the label"
    );
    let rows = rows(&h);
    assert_eq!(rows.len(), 7);
    for (i, (name, _)) in PROFILES.iter().enumerate() {
        let label = format!("{name} on Steam");
        let controls = h.controls_labelled(&label);
        assert_eq!(controls.len(), 1, "one control labelled {label:?}");
        assert_eq!(
            controls[0].accessible_role(),
            Some(i_slint_backend_testing::AccessibleRole::Button)
        );
        assert_eq!(rows[i].name.as_str(), *name);
        // No Steam in this harness: every tile wears its monogram.
        assert!(!rows[i].has_avatar, "{name}");
        assert!(!rows[i].initials.is_empty(), "{name}");
    }
    assert_eq!(rows[2].initials.as_str(), "X");
}

#[test]
fn a_click_opens_that_testers_steam_profile() {
    let mut h = Harness::new();
    about_tab(&mut h);
    h.click_label("ABOUT");
    let _ = crate::open_url::take_opened();
    for (name, url) in PROFILES {
        h.click_label(&format!("{name} on Steam"));
        assert_eq!(
            crate::open_url::take_opened(),
            vec![url.to_string()],
            "{name}"
        );
    }
}

#[test]
fn nothing_is_fetched_before_the_tab_is_opened() {
    let steam = FakeSteam::start();
    let mut h = Harness::with_steam_server(&steam.base);
    about_tab(&mut h);
    assert!(
        avatar_requests(&mut h).is_empty(),
        "settings open, no tab yet"
    );
    assert_eq!(steam.requests(), 0);
    assert!(rows(&h).is_empty(), "no tiles before the tab opens");
    h.click_label("ABOUT");
    assert_eq!(
        avatar_requests(&mut h).len(),
        7,
        "the tab asks for all seven"
    );
}

#[test]
fn avatars_come_from_steam_are_cached_and_fall_back_to_the_monogram() {
    let steam = FakeSteam::start();
    steam.put_profile(TESTERS[0].path, jpeg());
    steam.put_private_profile(TESTERS[1].path);
    // The others answer 404: a failed fetch.
    let mut h = Harness::with_steam_server(&steam.base);
    about_tab(&mut h);
    h.click_label("ABOUT");
    let asked = avatar_requests(&mut h);
    assert_eq!(asked.len(), 7);

    // Do what core does for the command, then deliver the replies.
    let store = h.ctx().testers.clone();
    for slug in &asked {
        if let Fetched::Hit(_) = block_on(store.fetch(slug)) {
            h.emit(Event::TesterAvatarLoaded { slug: slug.clone() });
        }
    }
    let rows_now = rows(&h);
    assert!(rows_now[0].has_avatar, "Ostkatt has the Steam avatar");
    assert!(
        rows_now[1..].iter().all(|r| !r.has_avatar),
        "a private profile and a failed fetch keep the monogram"
    );
    assert!(!rows_now[1].initials.is_empty());
    assert!(
        !h.controls_labelled("Ostkatt on Steam").is_empty(),
        "the tile is still a control"
    );

    // The next open reads the cache: Ostkatt is not asked for again, and
    // Steam is not hit for it.
    assert!(matches!(store.lookup("ostkatt"), Lookup::Fresh(_)));
    let before = steam.requests();
    h.click_label("ABOUT");
    let again = avatar_requests(&mut h);
    assert!(!again.contains(&"ostkatt".to_string()), "{again:?}");
    assert_eq!(steam.requests(), before, "a cache hit asks Steam nothing");
    assert!(rows(&h)[0].has_avatar, "the cached avatar shows at once");
}
