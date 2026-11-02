//! Test-only fake game sensor input (`e2e-game` feature, never shipped).
//!
//! A journey cannot start a real game. `inject` queues the `GameEvent` that the
//! sensor would send for a catalogue game. `Client::drain_game_events` takes the
//! queue with the real sensor events, so the game state manager, the presence
//! publish, the UI events and `game_session_end` all run their normal code
//! (plans/E2E-QA.md section 5.1).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::catalogue::Head;
use crate::game_sensing::{ActiveGame, GameEvent};

static QUEUE: Mutex<Vec<GameEvent>> = Mutex::new(Vec::new());
/// The fake games that are open, by game id, so an end finds its start.
static OPEN: Mutex<Option<HashMap<String, ActiveGame>>> = Mutex::new(None);

/// What `inject` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    End,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Queue a sensor event for a game of the bundled catalogue. `ago_min` moves
/// the start into the past, so a short journey can end a session that is long
/// enough for the ledger (`MIN_SESSION_LEDGER_MIN`).
pub fn inject(action: Action, game_id: &str, ago_min: i64) -> Result<(), String> {
    let mut open = OPEN.lock().map_err(|_| "lock poisoned".to_string())?;
    let open = open.get_or_insert_with(HashMap::new);
    let event = match action {
        Action::Start => {
            let head = Head::bundled().ok_or("no bundled catalogue")?;
            let entry = head
                .by_game_id(game_id)
                .ok_or_else(|| format!("{game_id} is not in the catalogue"))?;
            let started = now_ms() - ago_min.max(0) * 60_000;
            let game = ActiveGame {
                game_id: entry.game_id.to_string(),
                game_name: entry.name.to_string(),
                short_name: entry.short_name.to_string(),
                color: "#2A2A30".into(),
                exe: format!("{game_id}.exe"),
                exe_path: String::new(),
                // A pid no real process of the sensor can have.
                pid: 4_000_000_000 - open.len() as u32,
                igdb_id: Some(entry.igdb_id),
                started_at: started,
                started_at_ms: started,
                foreground_ms: ago_min.max(0) * 60_000,
            };
            open.insert(game_id.to_string(), game.clone());
            GameEvent::Started(game)
        }
        Action::End => {
            let game = open
                .remove(game_id)
                .ok_or_else(|| format!("no fake game {game_id} is open"))?;
            GameEvent::Stopped {
                game: Box::new(game),
                ended_at: now_ms(),
            }
        }
    };
    log::info!("[e2e] fake game {action:?}: {game_id}");
    QUEUE
        .lock()
        .map_err(|_| "lock poisoned".to_string())?
        .push(event);
    Ok(())
}

/// The pids `inject` gives fake games: just under 4e9, where no real
/// process is.
pub fn is_fake_pid(pid: u32) -> bool {
    (3_999_999_000..=4_000_000_000).contains(&pid)
}

const REFERENCE_W: u32 = 320;
const REFERENCE_H: u32 = 180;
const REFERENCE_FRAMES: u32 = 120;

/// A 2 s loop of 320x180 at 60 fps in I420: a white bar that moves one step
/// a frame on grey. Written once to the temp folder.
fn reference_clip() -> Option<std::path::PathBuf> {
    let path = std::env::temp_dir().join("mello_e2e_reference_320x180.yuv");
    let frame_bytes = (REFERENCE_W * REFERENCE_H * 3 / 2) as u64;
    if std::fs::metadata(&path).map(|m| m.len()).ok() == Some(frame_bytes * REFERENCE_FRAMES as u64)
    {
        return Some(path);
    }
    let (w, h) = (REFERENCE_W as usize, REFERENCE_H as usize);
    let mut data = Vec::with_capacity((frame_bytes * REFERENCE_FRAMES as u64) as usize);
    for n in 0..REFERENCE_FRAMES as usize {
        let bar = (n * w / REFERENCE_FRAMES as usize) % w;
        for _y in 0..h {
            for x in 0..w {
                data.push(if x.abs_diff(bar) < 8 { 235 } else { 96 });
            }
        }
        data.extend(std::iter::repeat_n(128u8, w * h / 2)); // U and V: grey
    }
    if let Err(e) = std::fs::write(&path, &data) {
        log::warn!("[e2e] cannot write the reference clip: {e}");
        return None;
    }
    Some(path)
}

/// The replay of a fake game: the reference clip in place of the game's
/// window, which a fake game does not have (plans/GAME-CLIPS.md 10.5).
pub(crate) fn start_reference_replay(
    ctx: *mut mello_sys::MelloContext,
    config: &mello_sys::MelloReplayConfig,
) -> mello_sys::MelloResult {
    let Some(path) = reference_clip() else {
        return mello_sys::MelloResult_MELLO_ERROR_FAILED;
    };
    let Ok(c_path) = std::ffi::CString::new(path.to_string_lossy().into_owned()) else {
        return mello_sys::MelloResult_MELLO_ERROR_FAILED;
    };
    let set = unsafe {
        mello_sys::mello_test_set_reference_clip(
            ctx,
            c_path.as_ptr(),
            REFERENCE_W,
            REFERENCE_H,
            60,
            mello_sys::MelloReferencePixelFormat_MELLO_REFERENCE_I420,
        )
    };
    if set != mello_sys::MelloResult_MELLO_OK {
        return set;
    }
    let reference = mello_sys::MelloReplayConfig {
        hwnd: std::ptr::null_mut(),
        pid: 0,
        height: config.height,
        fps: config.fps,
        bitrate_kbps: config.bitrate_kbps,
        buffer_dir: config.buffer_dir,
    };
    log::info!("[e2e] the fake game's replay records the reference clip");
    unsafe { mello_sys::mello_test_replay_start_reference(ctx, &reference) }
}

/// The queued events, oldest first. Called by the client loop.
pub(crate) fn take() -> Vec<GameEvent> {
    QUEUE
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_and_an_end_queue_the_sensor_events_of_one_game() {
        inject(Action::Start, "rocket-league", 12).expect("start");
        assert!(inject(Action::Start, "no-such-game", 0).is_err());
        inject(Action::End, "rocket-league", 0).expect("end");
        assert!(inject(Action::End, "rocket-league", 0).is_err());
        let events = take();
        assert_eq!(events.len(), 2);
        let GameEvent::Started(g) = &events[0] else {
            panic!("first is a start");
        };
        assert_eq!(g.game_id, "rocket-league");
        assert!(now_ms() - g.started_at >= 12 * 60_000);
        assert!(
            matches!(&events[1], GameEvent::Stopped { game, .. } if game.game_id == "rocket-league")
        );
    }
}
