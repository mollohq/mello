//! Runtime game-icon glue: memory cache (`AppContext::game_icon_cache`) over
//! the PNG disk cache (`platform::exe_icon`), plus the extract-on-confirm
//! flow for custom games. Crew sharing (upload/fetch) layers on top via
//! `Command::UploadGameIcon` / the icon-fetch glue.

use slint::ComponentHandle;

use crate::app_context::AppContext;

/// Resolve a runtime icon for a game id: memory cache first, then the disk
/// cache (decoded on the UI thread). `None` when nothing is cached locally —
/// the crew-icon fetch layer handles misses.
pub fn resolve_runtime_icon(ctx: &AppContext, game_id: &str) -> Option<slint::Image> {
    if game_id.is_empty() {
        return None;
    }
    if let Some(img) = ctx.game_icon_cache.borrow().get(game_id) {
        return Some(img.clone());
    }
    let (rgba, w, h) = crate::platform::exe_icon::load_cached_icon_rgba(game_id)?;
    let img = crate::avatar::rgba_to_image(&rgba, w, h);
    ctx.game_icon_cache
        .borrow_mut()
        .insert(game_id.to_string(), img.clone());
    Some(img)
}

/// Resolve a runtime icon, requesting the crew-shared copy from the backend
/// when nothing is cached locally. Each id is requested at most once per run
/// (misses stay negative-cached); the async reply lands via
/// `Event::GameIconLoaded` and shows on the next model refresh.
pub fn resolve_or_fetch_icon(ctx: &AppContext, game_id: &str) -> Option<slint::Image> {
    if let Some(img) = resolve_runtime_icon(ctx, game_id) {
        return Some(img);
    }
    if game_id.is_empty() {
        return None;
    }
    thread_local! {
        static REQUESTED: std::cell::RefCell<std::collections::HashSet<String>> =
            std::cell::RefCell::new(std::collections::HashSet::new());
    }
    REQUESTED.with(|r| {
        if r.borrow_mut().insert(game_id.to_string()) {
            let _ = ctx.cmd_tx.send(mello_core::Command::FetchGameIcon {
                game_id: game_id.to_string(),
            });
        }
    });
    None
}

/// Local Steam header art for a game id, decoded and cached in memory. The
/// third rung of the tile ladder: the server icon, then this, then the
/// monogram. The file is read from the Steam cache on this disk. Nothing is
/// fetched and nothing is uploaded.
pub fn resolve_local_steam_art(ctx: &AppContext, game_id: &str) -> Option<slint::Image> {
    let key = format!("steam-art:{game_id}");
    if let Some(img) = ctx.game_icon_cache.borrow().get(&key) {
        return Some(img.clone());
    }
    let path = mello_core::library::local_steam_art(game_id)?;
    let (rgba, w, h) = decode_capsule(&path)?;
    let img = crate::avatar::rgba_to_image(&rgba, w, h);
    ctx.game_icon_cache.borrow_mut().insert(key, img.clone());
    Some(img)
}

/// Local Steam poster art (the library hero, else the header) for a game id,
/// for the wide live session card. Decoded and cached in memory. The file is
/// read from the Steam cache on this disk. Nothing is fetched and nothing is
/// uploaded.
pub fn resolve_local_steam_poster(ctx: &AppContext, game_id: &str) -> Option<slint::Image> {
    let key = format!("steam-poster:{game_id}");
    if let Some(img) = ctx.game_icon_cache.borrow().get(&key) {
        return Some(img.clone());
    }
    let path = mello_core::library::local_steam_poster(game_id)?;
    let (rgba, w, h) = decode_scaled(&path, POSTER_MAX_WIDTH)?;
    let img = crate::avatar::rgba_to_image(&rgba, w, h);
    ctx.game_icon_cache.borrow_mut().insert(key, img.clone());
    Some(img)
}

/// Widest decoded poster. The wide card is about 740 px at 1x, so this holds
/// the art at 2x on a normal window.
const POSTER_MAX_WIDTH: u32 = 1280;

/// Widest decoded capsule. The tile draws it at 76 px, so this holds the art
/// at 2x scale without keeping a full header in memory.
const CAPSULE_MAX_WIDTH: u32 = 184;

/// Decode a JPEG or PNG from the Steam cache to RGBA, shrunk to
/// `CAPSULE_MAX_WIDTH` wide at most. `None` for an unreadable file.
fn decode_capsule(path: &std::path::Path) -> Option<(Vec<u8>, u32, u32)> {
    decode_scaled(path, CAPSULE_MAX_WIDTH)
}

/// Decode an image to RGBA, shrunk to `max_width` wide at most.
fn decode_scaled(path: &std::path::Path, max_width: u32) -> Option<(Vec<u8>, u32, u32)> {
    let decoded = image::open(path).ok()?.to_rgba8();
    let (w, h) = decoded.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    if w <= max_width {
        return Some((decoded.into_raw(), w, h));
    }
    let nh = ((h as u64 * max_width as u64) / w as u64).max(1) as u32;
    let small = image::imageops::resize(
        &decoded,
        max_width,
        nh,
        image::imageops::FilterType::Triangle,
    );
    Some((small.into_raw(), max_width, nh))
}

/// Make sure a detected game has art, extracting the executable's own icon if
/// we have not already cached one.
///
/// Rung 1 of the icon ladder (assets §8.2). Runs for *every* detected game
/// rather than only user-confirmed ones, which is what lets an indie title the
/// catalogue has never heard of look exactly as good as Counter-Strike.
/// Cheap and idempotent: a cached id returns immediately.
pub fn ensure_icon(ctx: &AppContext, game_id: &str, exe_path: &str) {
    if game_id.is_empty() || exe_path.is_empty() {
        return;
    }
    if ctx.game_icon_cache.borrow().contains_key(game_id)
        || crate::platform::exe_icon::cached_icon_path(game_id).is_some_and(|p| p.exists())
    {
        return;
    }
    // A shared runtime's icon is the runtime's, not the game's — Java's coffee
    // cup on a Minecraft card reads as a bug. The badge is the better answer.
    if !crate::platform::exe_icon::icon_is_representative(exe_path) {
        log::debug!("[game-icon] {game_id}: {exe_path} is a generic host, keeping the badge");
        return;
    }
    extract_and_cache(
        ctx.game_icon_cache.clone(),
        ctx.cmd_tx.clone(),
        ctx.rt.clone(),
        ctx.app.as_weak(),
        game_id.to_string(),
        exe_path.to_string(),
    );
}

/// The art of one game tile, resolved by [`resolve_tile_art`]. At most one of
/// `icon` and `capsule` is set; with neither the tile draws the monogram.
pub struct TileArt {
    pub icon: slint::Image,
    pub has_icon: bool,
    pub capsule: slint::Image,
    pub has_capsule: bool,
}

/// Steam ids (`steam-<appid>`) that can name the local art of a game.
///
/// Surfaces know a game by different ids: the stats and the live game carry
/// the catalogue id (`brawlhalla`), the library carries the launcher id
/// (`steam-291550`), and a card can carry only a name. Every one of them must
/// reach the same art, so all the routes are tried, in this order: the ids as
/// given, the launcher id of an installed game with that name, then the
/// appids that the bundled index lists for the catalogue entry.
pub fn art_candidates(
    wire_id: &str,
    game_id: &str,
    name: &str,
    installed: &[mello_core::events::InstalledGameInfo],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |id: String| {
        if !id.is_empty() && !out.contains(&id) {
            out.push(id);
        }
    };
    push(wire_id.to_string());
    push(game_id.to_string());
    if let Some(i) = installed
        .iter()
        .find(|i| !name.is_empty() && i.name.trim().eq_ignore_ascii_case(name.trim()))
    {
        push(i.game_id.clone());
    }
    for id in [wire_id, game_id] {
        for appid in steam_ids_of_catalogue_game(id, name) {
            push(appid);
        }
    }
    out
}

/// `steam-<appid>` ids of the catalogue game that `id` (or else `name`) names.
/// Cached by key: the reverse index lookup is a linear pass.
fn steam_ids_of_catalogue_game(id: &str, name: &str) -> Vec<String> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static BUNDLE: (Option<mello_core::catalogue::Head>, Option<mello_core::catalogue::AppIdIndex>) =
            (mello_core::catalogue::Head::bundled(), mello_core::catalogue::AppIdIndex::bundled());
        static CACHE: RefCell<HashMap<(String, String), Vec<String>>> = RefCell::new(HashMap::new());
    }
    if id.is_empty() || id.starts_with("steam-") {
        return Vec::new();
    }
    let key = (id.to_string(), name.to_lowercase());
    if let Some(hit) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }
    let ids = BUNDLE.with(|(head, index)| {
        let (Some(head), Some(index)) = (head, index) else {
            return Vec::new();
        };
        let entry = head.by_game_id(id).or_else(|| head.by_name(name));
        entry
            .map(|e| index.appids_for_igdb(e.igdb_id))
            .unwrap_or_default()
            .into_iter()
            .map(|a| format!("steam-{a}"))
            .collect()
    });
    CACHE.with(|c| c.borrow_mut().insert(key, ids.clone()));
    ids
}

/// The one art ladder of every game tile: the server icon (cached, or the
/// crew copy), then the local Steam art under any id that names the game
/// ([`art_candidates`]), then nothing, which draws the monogram. The recent
/// games card, the session card, the live card and the NOW PLAYING pill all
/// call this, so one game gets one kind of art everywhere.
pub fn resolve_tile_art(ctx: &AppContext, wire_id: &str, game_id: &str, name: &str) -> TileArt {
    let none = slint::Image::default;
    if let Some(icon) = resolve_or_fetch_icon(ctx, game_id) {
        return TileArt {
            icon,
            has_icon: true,
            capsule: none(),
            has_capsule: false,
        };
    }
    let ids = art_candidates(wire_id, game_id, name, &ctx.installed_games.borrow());
    match ids.iter().find_map(|id| resolve_local_steam_art(ctx, id)) {
        Some(capsule) => TileArt {
            icon: none(),
            has_icon: false,
            capsule,
            has_capsule: true,
        },
        None => TileArt {
            icon: none(),
            has_icon: false,
            capsule: none(),
            has_capsule: false,
        },
    }
}

/// Local Steam poster for the wide live card, under the same ids as the tile.
pub fn resolve_poster(
    ctx: &AppContext,
    wire_id: &str,
    game_id: &str,
    name: &str,
) -> Option<slint::Image> {
    art_candidates(wire_id, game_id, name, &ctx.installed_games.borrow())
        .iter()
        .find_map(|id| resolve_local_steam_poster(ctx, id))
}

/// Set the NOW PLAYING pill from the shared ladder. The pill is the game the
/// sensor detected, so its id is both the wire id and the catalogue id.
pub fn push_bar_art(ctx: &AppContext, game_id: &str, name: &str) {
    let art = resolve_tile_art(ctx, game_id, game_id, name);
    ctx.app.set_game_runtime_icon(art.icon);
    ctx.app.set_game_has_runtime_icon(art.has_icon);
    ctx.app.set_game_capsule(art.capsule);
    ctx.app.set_game_has_capsule(art.has_capsule);
    ctx.app
        .set_game_monogram(crate::recent_games::monogram(name).into());
    ctx.app
        .set_game_hue_index(crate::recent_games::hue_index(game_id, name));
}

/// Extract the exe's icon on a worker thread, write the PNG disk cache, then
/// decode it into the memory cache on the UI thread. Runs on custom-game
/// confirm; the crew upload is triggered from the same worker once the PNG
/// exists (via the Send-safe command channel). Takes the cloneable context
/// pieces so `'static` UI callbacks can call it.
pub fn extract_and_cache(
    cache: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, slint::Image>>>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<mello_core::Command>,
    rt: tokio::runtime::Handle,
    app: slint::Weak<crate::MainWindow>,
    game_id: String,
    exe_path: String,
) {
    let (tx, rx) = std::sync::mpsc::channel::<(Vec<u8>, u32, u32)>();

    let worker_game_id = game_id.clone();
    rt.spawn_blocking(move || {
        let Some((rgba, w, h)) = crate::platform::exe_icon::extract_exe_icon_rgba(&exe_path) else {
            log::debug!("[game-icon] no icon extracted from {exe_path}");
            return;
        };
        if crate::platform::exe_icon::cache_icon_png(&worker_game_id, &rgba, w, h).is_none() {
            return;
        }
        // Share with the crew (small PNG via the crew-avatar storage pattern).
        if let Some(png) = crate::platform::exe_icon::cached_icon_png_bytes(&worker_game_id) {
            let _ = cmd_tx.send(mello_core::Command::UploadGameIcon {
                game_id: worker_game_id.clone(),
                png,
            });
        }
        let _ = tx.send((rgba, w, h));
    });

    // Slint images must be built on the UI thread and the cache is Rc-owned,
    // so collect the worker's result with a short-lived UI timer.
    let timer_slot = std::rc::Rc::new(std::cell::RefCell::new(Some(slint::Timer::default())));
    let keepalive = timer_slot.clone();
    timer_slot.borrow().as_ref().expect("timer present").start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(100),
        move || match rx.try_recv() {
            Ok((rgba, w, h)) => {
                let img = crate::avatar::rgba_to_image(&rgba, w, h);
                cache.borrow_mut().insert(game_id.clone(), img.clone());
                log::info!("[game-icon] runtime icon ready for {game_id}");
                // Show it now rather than on the next launch: extraction
                // finishes a moment after detection, and the bar is already up.
                if let Some(app) = app.upgrade() {
                    app.set_game_runtime_icon(img);
                    app.set_game_has_runtime_icon(true);
                    app.set_game_has_capsule(false);
                }
                keepalive.borrow_mut().take();
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                keepalive.borrow_mut().take();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(id: &str, name: &str) -> mello_core::events::InstalledGameInfo {
        mello_core::events::InstalledGameInfo {
            game_id: id.into(),
            name: name.into(),
            steam_hidden: false,
        }
    }

    #[test]
    fn a_catalogue_id_reaches_the_steam_id_through_the_appid_index() {
        let ids = art_candidates("brawlhalla", "brawlhalla", "Brawlhalla", &[]);
        assert!(ids.contains(&"steam-291550".to_string()), "{ids:?}");
        assert_eq!(ids[0], "brawlhalla");
    }

    #[test]
    fn a_launcher_id_and_an_installed_name_are_candidates() {
        let ids = art_candidates("steam-730", "counter-strike-2", "Counter-Strike 2", &[]);
        assert_eq!(ids[0], "steam-730");
        let ids = art_candidates(
            "",
            "x-game",
            "Some Game",
            &[installed("steam-42", "some game")],
        );
        assert!(ids.contains(&"steam-42".to_string()), "{ids:?}");
        assert!(art_candidates("", "", "", &[]).is_empty());
    }

    fn write_jpeg(path: &std::path::Path, w: u32, h: u32) {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([200, 40, 40]));
        img.save_with_format(path, image::ImageFormat::Jpeg)
            .expect("write fixture jpeg");
    }

    #[test]
    fn a_header_jpeg_decodes_and_shrinks() {
        let dir = std::env::temp_dir().join(format!("m3llo-capsule-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("header.jpg");
        write_jpeg(&big, 460, 215);
        let (rgba, w, h) = decode_capsule(&big).expect("decodes");
        assert_eq!((w, h), (CAPSULE_MAX_WIDTH, 86));
        assert_eq!(rgba.len(), (w * h * 4) as usize);

        let small = dir.join("small.jpg");
        write_jpeg(&small, 100, 47);
        assert_eq!(
            decode_capsule(&small).map(|(_, w, h)| (w, h)),
            Some((100, 47))
        );

        let bad = dir.join("bad.jpg");
        std::fs::write(&bad, b"not a jpeg").unwrap();
        assert!(decode_capsule(&bad).is_none());
        assert!(decode_capsule(&dir.join("missing.jpg")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
