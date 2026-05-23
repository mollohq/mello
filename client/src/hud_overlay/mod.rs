pub mod renderer;
pub mod window;

use std::sync::mpsc;

use crate::hud_manager::HudMessage;

/// Spawn the overlay on a dedicated thread. Returns the sender half of the
/// channel; the overlay loop runs until `HudMessage::Shutdown` is sent or the
/// sender is dropped.
pub fn spawn() -> mpsc::Sender<HudMessage> {
    let (tx, rx) = mpsc::channel::<HudMessage>();

    std::thread::Builder::new()
        .name("hud-overlay".into())
        .spawn(move || match window::Win32OverlayWindow::new() {
            Ok(mut win) => {
                win.run_loop(rx);
            }
            Err(e) => {
                log::error!("[overlay] failed to create overlay window: {}", e);
            }
        })
        .expect("failed to spawn overlay thread");

    tx
}
