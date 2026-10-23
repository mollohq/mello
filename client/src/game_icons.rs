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

/// Show the game's icon on the NOW PLAYING bar, if one is cached.
///
/// Called on detection and again when extraction finishes, because the first
/// play of a game has no cached icon yet — the bar would otherwise keep the
/// initials badge until the next launch.
pub fn push_bar_icon(ctx: &AppContext, game_id: &str) {
    match resolve_runtime_icon(ctx, game_id) {
        Some(img) => {
            ctx.app.set_game_runtime_icon(img);
            ctx.app.set_game_has_runtime_icon(true);
        }
        None => ctx.app.set_game_has_runtime_icon(false),
    }
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
