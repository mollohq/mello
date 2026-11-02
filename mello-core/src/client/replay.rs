//! The game clip replay buffer (plans/GAME-CLIPS.md 4 and 5.3): while the
//! user is in voice and a game runs, libmello records the game; F8 saves the
//! last 30 s as an MP4 with a poster.
//!
//! The libmello calls block: a start makes a device, an encoder and the ring
//! files, and a capture muxes for about a second. They run on one worker
//! thread, in order, never on the command loop. The worker reports with
//! events, and polls the replay state once a second.

use crate::events::{Event, GameClipState};
use crate::game_sensing::ActiveGame;
use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The seconds of game that F8 saves (plan section 1).
pub(crate) const GAME_CLIP_SECONDS: f32 = 30.0;

/// The game that the replay records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplayTarget {
    pub game_id: String,
    pub game_name: String,
    pub pid: u32,
}

/// The game to record, from the rules of plan 4.1 that core knows: the user
/// is in voice, and a game runs (the one the user looks at). The setting, the
/// foreground hold and the disk check come with the replay manager of todo
/// `core-lifecycle`.
pub(crate) fn replay_target(in_voice: bool, game: Option<&ActiveGame>) -> Option<ReplayTarget> {
    if !in_voice {
        return None;
    }
    let game = game?;
    if game.pid == 0 {
        return None;
    }
    Some(ReplayTarget {
        game_id: game.game_id.clone(),
        game_name: game.game_name.clone(),
        pid: game.pid,
    })
}

/// The libmello state as the UI knows it.
pub(crate) fn game_clip_state(state: mello_sys::MelloReplayState) -> GameClipState {
    match state {
        mello_sys::MelloReplayState_MELLO_REPLAY_RECORDING => GameClipState::Recording,
        mello_sys::MelloReplayState_MELLO_REPLAY_WAITING => GameClipState::Waiting,
        mello_sys::MelloReplayState_MELLO_REPLAY_PAUSED => GameClipState::Paused,
        mello_sys::MelloReplayState_MELLO_REPLAY_FAILED => GameClipState::Unavailable,
        _ => GameClipState::Off,
    }
}

enum Op {
    Start(ReplayTarget),
    Stop,
    Capture { seconds: f32 },
}

/// The context pointer for the worker thread. The replay calls of libmello
/// lock inside the context (`Context::replay`), and the worker stops before
/// the context goes (the client field order, see `Client::replay`).
struct SendCtx(*mut mello_sys::MelloContext);
unsafe impl Send for SendCtx {}

pub(crate) struct ReplayWorker {
    tx: Option<mpsc::Sender<Op>>,
    thread: Option<JoinHandle<()>>,
}

impl ReplayWorker {
    pub(crate) fn start(ctx: *mut mello_sys::MelloContext, events: mpsc::Sender<Event>) -> Self {
        let (tx, rx) = mpsc::channel();
        let ctx = SendCtx(ctx);
        let thread = std::thread::Builder::new()
            .name("game-clip-replay".into())
            .spawn(move || run(ctx, rx, events))
            .map_err(|e| log::error!("[replay] worker thread did not start: {e}"))
            .ok();
        Self {
            tx: Some(tx),
            thread,
        }
    }

    /// Record `target`, or nothing. A change stops the old replay first.
    pub(crate) fn set_target(&self, target: Option<ReplayTarget>) {
        let op = match target {
            Some(t) => Op::Start(t),
            None => Op::Stop,
        };
        if let Some(tx) = &self.tx {
            let _ = tx.send(op);
        }
    }

    pub(crate) fn capture(&self, seconds: f32) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Op::Capture { seconds });
        }
    }
}

impl Drop for ReplayWorker {
    fn drop(&mut self) {
        // The worker stops the replay and ends when the channel closes.
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl super::Client {
    /// Start, change or stop the replay to match voice and the game. Called
    /// after each game event and each voice start and leave.
    pub(super) fn sync_replay(&mut self) {
        // Game clips are Windows only until the macOS work (plan 9.2).
        if !cfg!(windows) {
            return;
        }
        let target = replay_target(self.replay_in_voice, self.game_state.current_game());
        if target == self.replay_target {
            return;
        }
        if self.replay.is_none() {
            if target.is_none() {
                return;
            }
            self.replay = Some(ReplayWorker::start(
                self.voice.mello_ctx(),
                self.event_tx.clone(),
            ));
        }
        if let Some(worker) = &self.replay {
            worker.set_target(target.clone());
        }
        self.replay_target = target;
    }

    pub(super) fn handle_capture_game_clip(&self) {
        match (&self.replay, &self.replay_target) {
            (Some(worker), Some(_)) => worker.capture(GAME_CLIP_SECONDS),
            _ => {
                let _ = self.event_tx.send(Event::GameClipFailed {
                    reason: "Clips are not available now".into(),
                });
            }
        }
    }
}

fn last_error(ctx: *mut mello_sys::MelloContext) -> String {
    let text = unsafe { mello_sys::mello_get_error(ctx) };
    if text.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}

fn run(ctx: SendCtx, rx: mpsc::Receiver<Op>, events: mpsc::Sender<Event>) {
    let ctx = ctx.0;
    let mut current: Option<ReplayTarget> = None;
    let mut last_state = GameClipState::Off;
    let emit_state =
        |state: GameClipState, game: &str, reason: String, last: &mut GameClipState| {
            if state != *last || state == GameClipState::Unavailable {
                *last = state;
                let _ = events.send(Event::GameClipStateChanged {
                    state,
                    game_name: game.to_string(),
                    reason,
                });
            }
        };

    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Op::Start(target)) => {
                if current.as_ref() == Some(&target) {
                    continue;
                }
                if current.take().is_some() {
                    unsafe { mello_sys::mello_replay_stop(ctx) };
                }
                // The clip quality of plan section 7, default 720p60. The
                // bitrate is the estimate of 3.4 until the replay lane sets it.
                let config = mello_sys::MelloReplayConfig {
                    hwnd: std::ptr::null_mut(),
                    pid: target.pid,
                    height: 720,
                    fps: 60,
                    bitrate_kbps: 5000,
                    buffer_dir: std::ptr::null(),
                };
                let result = unsafe { mello_sys::mello_replay_start(ctx, &config) };
                if result == mello_sys::MelloResult_MELLO_OK {
                    log::info!(
                        "[replay] recording {} (pid {})",
                        target.game_name,
                        target.pid
                    );
                    let state = game_clip_state(unsafe { mello_sys::mello_replay_state(ctx) });
                    emit_state(state, &target.game_name, String::new(), &mut last_state);
                    current = Some(target);
                } else {
                    let reason = last_error(ctx);
                    log::warn!(
                        "[replay] not available for {} (pid {}): {}",
                        target.game_name,
                        target.pid,
                        reason
                    );
                    emit_state(
                        GameClipState::Unavailable,
                        &target.game_name,
                        reason,
                        &mut last_state,
                    );
                }
            }
            Ok(Op::Stop) => {
                if current.take().is_some() {
                    unsafe { mello_sys::mello_replay_stop(ctx) };
                    log::info!("[replay] stopped");
                }
                emit_state(GameClipState::Off, "", String::new(), &mut last_state);
            }
            Ok(Op::Capture { seconds }) => {
                let event = match &current {
                    Some(target) => capture(ctx, seconds, target),
                    None => Event::GameClipFailed {
                        reason: "Clips are not available now".into(),
                    },
                };
                let _ = events.send(event);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(target) = &current {
                    let state = game_clip_state(unsafe { mello_sys::mello_replay_state(ctx) });
                    if state != last_state {
                        let reason = if state == GameClipState::Unavailable {
                            last_error(ctx)
                        } else {
                            String::new()
                        };
                        emit_state(state, &target.game_name, reason, &mut last_state);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if current.take().is_some() {
                    unsafe { mello_sys::mello_replay_stop(ctx) };
                }
                return;
            }
        }
    }
}

fn capture(ctx: *mut mello_sys::MelloContext, seconds: f32, target: &ReplayTarget) -> Event {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let clip_id = format!("game_clip_{ms}");
    let dir: PathBuf = std::env::temp_dir().join("mello_clips");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Event::GameClipFailed {
            reason: format!("Cannot create the clips folder: {e}"),
        };
    }
    let mp4 = dir.join(format!("{clip_id}.mp4"));
    let poster = dir.join(format!("{clip_id}.jpg"));
    let (Ok(c_mp4), Ok(c_poster)) = (
        CString::new(mp4.to_string_lossy().into_owned()),
        CString::new(poster.to_string_lossy().into_owned()),
    ) else {
        return Event::GameClipFailed {
            reason: "Bad clip path".into(),
        };
    };
    let mut clip = mello_sys::MelloReplayClip {
        duration_us: 0,
        width: 0,
        height: 0,
        bytes: 0,
        has_game_audio: false,
        poster_live: false,
    };
    let result = unsafe {
        mello_sys::mello_replay_capture(ctx, seconds, c_mp4.as_ptr(), c_poster.as_ptr(), &mut clip)
    };
    if result != mello_sys::MelloResult_MELLO_OK {
        let reason = last_error(ctx);
        log::warn!("[replay] clip of {} failed: {}", target.game_name, reason);
        return Event::GameClipFailed { reason };
    }
    log::info!(
        "[replay] clip of {}: {} ({:.1} s, {}x{}, {} bytes, game audio {}, poster {})",
        target.game_name,
        mp4.display(),
        clip.duration_us as f64 / 1e6,
        clip.width,
        clip.height,
        clip.bytes,
        if clip.has_game_audio { "on" } else { "off" },
        if clip.poster_live {
            "live"
        } else {
            "from the clip"
        }
    );
    Event::GameClipCaptured {
        clip_id,
        path: mp4.to_string_lossy().into_owned(),
        poster_path: poster.to_string_lossy().into_owned(),
        duration_seconds: clip.duration_us as f32 / 1e6,
        width: clip.width,
        height: clip.height,
        game_name: target.game_name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(pid: u32) -> ActiveGame {
        ActiveGame {
            game_id: "counter-strike-2".into(),
            game_name: "Counter-Strike 2".into(),
            short_name: "CS2".into(),
            color: String::new(),
            exe: "cs2.exe".into(),
            exe_path: String::new(),
            pid,
            igdb_id: None,
            started_at: 0,
            started_at_ms: 0,
            foreground_ms: 0,
        }
    }

    #[test]
    fn the_replay_records_the_game_only_in_voice() {
        let cs2 = game(4242);
        assert_eq!(replay_target(false, Some(&cs2)), None);
        assert_eq!(replay_target(true, None), None);
        let target = replay_target(true, Some(&cs2)).expect("in voice with a game");
        assert_eq!(target.pid, 4242);
        assert_eq!(target.game_id, "counter-strike-2");
    }

    #[test]
    fn a_game_with_no_process_is_not_recorded() {
        assert_eq!(replay_target(true, Some(&game(0))), None);
    }

    #[test]
    fn every_libmello_state_maps_to_a_ui_state() {
        assert_eq!(
            game_clip_state(mello_sys::MelloReplayState_MELLO_REPLAY_RECORDING),
            GameClipState::Recording
        );
        assert_eq!(
            game_clip_state(mello_sys::MelloReplayState_MELLO_REPLAY_PAUSED),
            GameClipState::Paused
        );
        assert_eq!(
            game_clip_state(mello_sys::MelloReplayState_MELLO_REPLAY_FAILED),
            GameClipState::Unavailable
        );
        assert_eq!(
            game_clip_state(mello_sys::MelloReplayState_MELLO_REPLAY_OFF),
            GameClipState::Off
        );
    }
}
