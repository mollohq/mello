//! The stream key (F9) in the client (specs/12-STREAMING.md section 11,
//! "Stream key"). A press starts a stream of the detected game to the active
//! crew, or ends the hosted stream. The user can stay in a fullscreen game:
//! the key needs no voice and no HUD, and an OS notification tells the
//! result. The Keybinds row is in `callbacks/settings.rs`.

use crate::app_context::AppContext;
use crate::callbacks::game::{quick_start_stream, QuickStart};
use crate::notifications;

/// The stream start or stop that the stream key made, until core reports
/// how it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKeyAction {
    Start,
    Stop,
}

/// One press of the stream key.
pub(crate) fn stream_key_pressed(ctx: &AppContext) {
    if ctx.app.get_is_hosting() {
        // StreamStarting sets the host card to "starting"; StreamStarted
        // and StreamFailed change it. A press in that time does nothing.
        if ctx.app.get_stream_host_state().as_str() == "starting" {
            log::info!("[stream key] press ignored: a stream start is in flight");
            return;
        }
        log::info!("[stream key] end the hosted stream");
        ctx.stream_key_action.set(Some(StreamKeyAction::Stop));
        crate::callbacks::streaming::stop_stream(&ctx.app, &ctx.cmd_tx);
        return;
    }
    match quick_start_stream(&ctx.app, &ctx.cmd_tx, &ctx.fg_monitor) {
        QuickStart::Sent => {
            log::info!("[stream key] start a stream of the detected game");
            ctx.stream_key_action.set(Some(StreamKeyAction::Start));
        }
        QuickStart::NoCrew => {
            log::info!("[stream key] no start: no active crew");
            notify(ctx, notifications::STREAM_KEY_NO_CREW);
        }
        QuickStart::NoGame => {
            log::info!("[stream key] no start: no detected game");
            notify(ctx, notifications::STREAM_KEY_NO_GAME);
        }
        QuickStart::AlreadyHosting => {
            log::info!("[stream key] press ignored: the user hosts a stream");
        }
    }
}

/// `StreamStarted`: a start of the stream key is live.
pub(crate) fn stream_started(ctx: &AppContext) {
    if ctx.stream_key_action.get() == Some(StreamKeyAction::Start) {
        ctx.stream_key_action.set(None);
        notify(ctx, notifications::STREAM_KEY_LIVE);
    }
}

/// `StreamEnded` for the host: a start or a stop of the stream key ended.
pub(crate) fn stream_ended(ctx: &AppContext) {
    if ctx.stream_key_action.take().is_some() {
        notify(ctx, notifications::STREAM_KEY_ENDED);
    }
}

/// `StreamFailed` for the host: a start of the stream key did not start.
pub(crate) fn stream_failed(ctx: &AppContext) {
    if ctx.stream_key_action.take().is_some() {
        notify(ctx, notifications::STREAM_KEY_FAILED);
    }
}

fn notify(ctx: &AppContext, text: &str) {
    log::info!("[stream key] notice: {text}");
    if let Some(show) = ctx.stream_key_notice.borrow().as_ref() {
        show(text);
    }
}
