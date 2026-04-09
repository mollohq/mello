use mello_core::Command;

use crate::app_context::AppContext;

pub fn wire(ctx: &AppContext) {
    let cmd = ctx.cmd_tx.clone();
    ctx.app.on_clip_captured(move || {
        log::info!("UI: clip button pressed");
        let _ = cmd.try_send(Command::CaptureClip { seconds: 30.0 });
    });
}
