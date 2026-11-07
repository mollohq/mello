//! The last picture of a live stream, for the card before you watch
//! (mockup next-gen-feed frames C, D and G).
//!
//! The SFU writes a JPEG of the stream to R2. While a live stream card
//! shows and you do not watch it, `sync` asks the backend (`stream_picture`
//! RPC) for the newest URL every 15 s. The picture is fetched once per URL
//! and kept on the window (`active-stream-picture` and the URL it came
//! from), so every card sync can draw it again. The card draws the game's
//! hero art until it arrives, and when the SFU keeps no snapshots.
use std::cell::RefCell;
use std::time::Duration;

use slint::{ComponentHandle, Model};

use crate::app_context::AppContext;
use crate::snapshot_cache;

/// Time between two asks for the newest picture of the live stream.
const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Widest decoded picture. The wide card is about 740 px at 1x, so this
/// holds it at 2x on a normal window.
const PICTURE_MAX_WIDTH: u32 = 1280;

thread_local! {
    /// The URL under fetch, so a crew state that repeats it asks once.
    static INFLIGHT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Fetch the picture of the active stream when its URL changed. The
/// `StreamPictureLoaded` handler and `sync` call it. A URL that is empty, already
/// shown or under fetch does nothing.
pub fn request(ctx: &AppContext) {
    let url = ctx.app.get_active_stream_thumbnail_url().to_string();
    if url.is_empty() {
        if !ctx.app.get_active_stream_picture_url().is_empty() {
            ctx.app.set_active_stream_picture(slint::Image::default());
            ctx.app.set_active_stream_picture_url("".into());
        }
        return;
    }
    if url == ctx.app.get_active_stream_picture_url().as_str() {
        return;
    }
    let started = INFLIGHT.with(|f| {
        let mut f = f.borrow_mut();
        if f.as_deref() == Some(url.as_str()) {
            false
        } else {
            *f = Some(url.clone());
            true
        }
    });
    if !started {
        return;
    }
    log::debug!("[stream-picture] fetch {url}");
    let app_weak = ctx.app.as_weak();
    let client = reqwest::Client::new();
    ctx.rt.spawn(async move {
        let decoded = match snapshot_cache::ensure_raw_on_disk(&client, &url).await {
            Some(raw) => tokio::task::spawn_blocking(move || decode(&raw))
                .await
                .ok()
                .flatten(),
            None => None,
        };
        let _ = slint::invoke_from_event_loop(move || {
            INFLIGHT.with(|f| {
                let mut f = f.borrow_mut();
                if f.as_deref() == Some(url.as_str()) {
                    *f = None;
                }
            });
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let Some((rgba, w, h)) = decoded else {
                log::warn!("[stream-picture] no picture from {url}");
                return;
            };
            // The state moved on while the fetch ran: this picture is of
            // a stream that is gone.
            if app.get_active_stream_thumbnail_url().as_str() != url {
                return;
            }
            let img = snapshot_cache::rgba_bytes_to_image(rgba, w, h);
            app.set_active_stream_picture(img.clone());
            app.set_active_stream_picture_url(url.as_str().into());
            apply_to_cards(&app, &img);
            log::debug!("[stream-picture] shown {url}");
        });
    });
}

/// Start or stop the asks for the newest picture of the live stream. The
/// card sync calls it on every sync.
///
/// - `poll` true: a live stream card shows and you do not watch it. Ask
///   now, then every `POLL_INTERVAL`. A timer that runs already continues.
/// - `poll` false: stop the timer and clear the picture.
pub fn sync(ctx: &AppContext, poll: bool) {
    if !poll {
        if ctx.stream_picture_timer.borrow_mut().take().is_some() {
            log::debug!("[stream-picture] poll stopped");
        }
        if !ctx.app.get_active_stream_thumbnail_url().is_empty() {
            ctx.app.set_active_stream_thumbnail_url("".into());
            request(ctx);
        }
        return;
    }
    if ctx.stream_picture_timer.borrow().is_some() {
        return;
    }
    log::debug!("[stream-picture] poll started");
    let app_weak = ctx.app.as_weak();
    let cmd = ctx.cmd_tx.clone();
    let ask = move || {
        let Some(app) = app_weak.upgrade() else {
            return;
        };
        let session_id = app.get_active_stream_session_id().to_string();
        // No live session to ask for.
        if session_id.is_empty() {
            return;
        }
        let _ = cmd.send(mello_core::Command::FetchStreamPicture {
            crew_id: app.get_active_crew_id().to_string(),
            session_id,
        });
    };
    // Ask now, so the first picture does not wait for the timer.
    ask();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, POLL_INTERVAL, ask);
    *ctx.stream_picture_timer.borrow_mut() = Some(timer);
}

/// Decode the JPEG on disk to RGBA, at most `PICTURE_MAX_WIDTH` wide.
fn decode(raw: &std::path::Path) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::ImageReader::open(raw)
        .ok()?
        .decode()
        .ok()?
        .to_rgba8();
    let (w, h) = img.dimensions();
    snapshot_cache::fit_width(img.into_raw(), w, h, PICTURE_MAX_WIDTH)
}

/// Put the picture on the watchable card that is not watched, without a
/// full card sync (the sync has no context here).
fn apply_to_cards(app: &crate::MainWindow, img: &slint::Image) {
    let cards = app.get_active_stream_cards();
    let host = app.get_active_streamer_id();
    for i in 0..cards.row_count() {
        let Some(mut row) = cards.row_data(i) else {
            continue;
        };
        if row.can_watch && row.host_id == host {
            row.picture = img.clone();
            row.picture_kind = "frame".into();
            cards.set_row_data(i, row);
        }
    }
}
