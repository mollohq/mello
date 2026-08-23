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
