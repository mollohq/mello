//! Test only: hold the command loop on purpose (`Command::TestHoldLoop`).
//!
//! The hold blocks the loop thread with a sleep, as a hung native call does.
//! On a current-thread runtime it also stops every other task. Tests read
//! when each hold started and ended, by token, to prove that something
//! happened while the loop was held.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub(crate) struct HoldWindow {
    pub started: Instant,
    pub ended: Option<Instant>,
}

fn windows() -> &'static Mutex<HashMap<u64, HoldWindow>> {
    static WINDOWS: OnceLock<Mutex<HashMap<u64, HoldWindow>>> = OnceLock::new();
    WINDOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Block the calling thread for `duration` and record the window.
pub(crate) fn hold(token: u64, duration: Duration) {
    log::info!(
        "test: holding the command loop for {} ms (token {})",
        duration.as_millis(),
        token
    );
    windows().lock().expect("hold windows").insert(
        token,
        HoldWindow {
            started: Instant::now(),
            ended: None,
        },
    );
    std::thread::sleep(duration);
    if let Some(w) = windows().lock().expect("hold windows").get_mut(&token) {
        w.ended = Some(Instant::now());
    }
    log::info!("test: command loop released (token {})", token);
}

/// The window of the hold `token`, once it started.
pub(crate) fn window(token: u64) -> Option<HoldWindow> {
    windows().lock().expect("hold windows").get(&token).copied()
}
