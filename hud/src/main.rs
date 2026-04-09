#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ipc;
mod mini_player;
mod mode;
mod overlay;
pub mod protocol;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mode::ModeManager;
use protocol::{HudMessage, HudMode, HudState};

fn main() {
    init_logging();
    log::info!("[hud] m3llo-hud starting");

    if let Err(e) = run() {
        log::error!("[hud] fatal: {}", e);
        std::process::exit(1);
    }
}

fn init_logging() {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr_layer = fmt::layer().with_target(true).with_writer(std::io::stderr);

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .init();
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let (state_rx, action_tx) = ipc::IpcClient::connect();

    let mini_player = mini_player::MiniPlayer::new(action_tx)?;
    let overlay_win = Rc::new(RefCell::new(overlay::OverlayWindow::new()?));
    let mode_mgr = Rc::new(RefCell::new(ModeManager::new()));
    let current_state = Rc::new(RefCell::new(HudState::default()));
    let clip_toast_deadline: Rc<RefCell<Option<Instant>>> = Rc::new(RefCell::new(None));

    log::info!("[hud] windows created, entering slint event loop");

    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), {
        let overlay_win = overlay_win.clone();
        let mode_mgr = mode_mgr.clone();
        let current_state = current_state.clone();
        let clip_toast_deadline = clip_toast_deadline.clone();

        move || {
            while let Ok(msg) = state_rx.try_recv() {
                match msg {
                    HudMessage::State(state) => {
                        log::debug!(
                            "[hud] recv state: mode={:?} crew={} voice={} members={}",
                            state.mode,
                            state.crew.is_some(),
                            state.voice.is_some(),
                            state.voice.as_ref().map_or(0, |v| v.members.len()),
                        );
                        let mode_changed = mode_mgr.borrow_mut().set_mode(state.mode);

                        if state.clip_toast.is_some() && clip_toast_deadline.borrow().is_none() {
                            *clip_toast_deadline.borrow_mut() =
                                Some(Instant::now() + Duration::from_secs(4));
                        }

                        *current_state.borrow_mut() = *state;

                        if mode_changed {
                            apply_mode(&mode_mgr.borrow(), &mini_player, &overlay_win.borrow());
                        }

                        let cs = current_state.borrow();
                        match mode_mgr.borrow().current() {
                            HudMode::MiniPlayer => {
                                mini_player.update_state(&cs);
                            }
                            HudMode::Overlay => {
                                overlay_win.borrow_mut().update_state(&cs);
                                overlay_win.borrow_mut().render();
                            }
                            HudMode::Hidden => {}
                        }
                    }
                    HudMessage::Settings(s) => {
                        log::debug!(
                            "[hud] recv settings: opacity={:.0}% overlay={} toasts={}",
                            s.overlay_opacity * 100.0,
                            s.overlay_enabled,
                            s.show_clip_toasts,
                        );
                    }
                    HudMessage::Shutdown => {
                        log::info!("[hud] received shutdown, exiting");
                        slint::quit_event_loop().ok();
                    }
                }
            }

            // Auto-dismiss clip toast
            let should_clear = clip_toast_deadline
                .borrow()
                .is_some_and(|d| Instant::now() >= d);
            if should_clear {
                *clip_toast_deadline.borrow_mut() = None;
                current_state.borrow_mut().clip_toast = None;
                let cs = current_state.borrow();
                match mode_mgr.borrow().current() {
                    HudMode::Overlay => {
                        overlay_win.borrow_mut().update_state(&cs);
                        overlay_win.borrow_mut().render();
                    }
                    HudMode::MiniPlayer => {
                        mini_player.update_state(&cs);
                    }
                    _ => {}
                }
            }

            mini_player.tick();

            if mode_mgr.borrow().current() == HudMode::Overlay {
                overlay_win.borrow().ensure_topmost();
            }
        }
    });

    slint::run_event_loop_until_quit()?;
    Ok(())
}


fn apply_mode(
    mode_mgr: &ModeManager,
    mini_player: &mini_player::MiniPlayer,
    overlay: &overlay::OverlayWindow,
) {
    let mode = mode_mgr.current();
    log::info!("[hud] apply_mode: {:?}", mode);
    match mode {
        HudMode::Hidden => {
            mini_player.hide();
            overlay.hide();
        }
        HudMode::MiniPlayer => {
            overlay.hide();
            mini_player.show();
        }
        HudMode::Overlay => {
            mini_player.hide();
            overlay.show();
        }
    }
}
