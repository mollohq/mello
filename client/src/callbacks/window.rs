use slint::ComponentHandle;

use crate::app_context::AppContext;

pub fn wire(ctx: &AppContext) {
    // --- Close → tray ---
    let app_weak = ctx.app.as_weak();
    let settings = ctx.settings.clone();
    ctx.app.window().on_close_requested(move || {
        if settings.borrow().close_to_tray {
            if let Some(app) = app_weak.upgrade() {
                // A user who closes the window thinks they left the call.
                // A call that stays up in the tray lets the channel hear
                // them, so the close hangs up the same way the button does.
                if app.get_in_voice() {
                    log::info!("[close] window closed to tray: leave voice");
                    app.invoke_voice_toggle();
                }
                app.hide().ok();
            }
            slint::CloseRequestResponse::KeepWindowShown
        } else {
            log::info!("[quit] close requested (close_to_tray=false)");
            slint::quit_event_loop().ok();
            slint::CloseRequestResponse::KeepWindowShown
        }
    });
}
