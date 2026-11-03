//! The game clip replay buffer (CLIPS.md 15.2 and 15.4): while the
//! user is in voice and a game runs, libmello records the game; F8 saves the
//! last 30 s as an MP4 with a poster.
//!
//! The libmello calls block: a start makes a device, an encoder and the ring
//! files, and a capture muxes for about a second. They run on one worker
//! thread, in order, never on the command loop. The worker reports with
//! events, and polls the replay state once a second.

use crate::events::{Event, GameClipQuality, GameClipState};
use crate::game_sensing::ActiveGame;
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The seconds of game that F8 saves (CLIPS.md 15.1).
pub(crate) const GAME_CLIP_SECONDS: f32 = 30.0;

/// The time a new game must keep the focus before it takes the replay
/// (CLIPS.md 15.2). An alt-tab between two games does not restart it each time.
pub(crate) const SWITCH_HOLD: Duration = Duration::from_secs(10);

/// The free space the buffer folder must have (CLIPS.md 15.2).
pub(crate) const DISK_FREE_MIN: u64 = 2 * 1024 * 1024 * 1024;

/// The ring files of a running replay (100 MB at 1080p60, the largest)
/// count as free space. Else a start at just over 2 GB stops itself.
pub(crate) const RING_ALLOWANCE: u64 = 128 * 1024 * 1024;

/// How often the worker reads the free space.
const DISK_CHECK_EVERY: Duration = Duration::from_secs(10);

/// The game that the replay records, at the quality of Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplayTarget {
    pub game_id: String,
    pub game_name: String,
    pub pid: u32,
    pub quality: GameClipQuality,
}

/// The game to record, from the rules of CLIPS.md 15.2 that core knows: the
/// setting is on, the user is in voice, and `game` runs. The worker adds the
/// disk check, and libmello the encoder and the capture.
pub(crate) fn replay_target(
    in_voice: bool,
    enabled: bool,
    quality: GameClipQuality,
    game: Option<&ActiveGame>,
) -> Option<ReplayTarget> {
    if !in_voice || !enabled {
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
        quality,
    })
}

/// One replay at a time (CLIPS.md 15.2). The replay keeps its game while that
/// game runs. A different game in focus takes it after `SWITCH_HOLD`.
#[derive(Debug, Default)]
pub(crate) struct GameHold {
    /// The game in focus that waits for the replay, and since when.
    candidate: Option<(String, Instant)>,
}

impl GameHold {
    /// The game to record now. `current` is the game the replay records, if
    /// it still runs and the rules still hold. `focused` is the game in
    /// focus under the same rules, or `None` when no replay may run.
    pub(crate) fn choose(
        &mut self,
        current: Option<ReplayTarget>,
        focused: Option<ReplayTarget>,
        now: Instant,
    ) -> Option<ReplayTarget> {
        let Some(focused) = focused else {
            self.candidate = None;
            return None;
        };
        let current = match current {
            Some(c) if c.game_id != focused.game_id => c,
            // No replay, or the same game: a new pid or quality applies at once.
            _ => {
                self.candidate = None;
                return Some(focused);
            }
        };
        let since = match &self.candidate {
            Some((id, since)) if *id == focused.game_id => *since,
            _ => {
                log::info!(
                    "[replay] {} has the focus; the replay stays on {} for {} s",
                    focused.game_name,
                    current.game_name,
                    SWITCH_HOLD.as_secs()
                );
                self.candidate = Some((focused.game_id.clone(), now));
                now
            }
        };
        if now.duration_since(since) >= SWITCH_HOLD {
            self.candidate = None;
            return Some(focused);
        }
        Some(current)
    }

    /// A game waits for its hold to end.
    pub(crate) fn waiting(&self) -> bool {
        self.candidate.is_some()
    }
}

/// CLIPS.md 15.2: too little free space for the replay. The ring files of a
/// running replay count as free.
pub(crate) fn disk_too_low(free: u64, running: bool) -> bool {
    let free = if running {
        free.saturating_add(RING_ALLOWANCE)
    } else {
        free
    };
    free < DISK_FREE_MIN
}

/// The bytes free for this user on the volume of `dir`.
#[cfg(windows)]
fn disk_free(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            free_to_caller: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
    }

    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

#[cfg(not(windows))]
fn disk_free(_dir: &Path) -> Option<u64> {
    None
}

/// The folder of the ring files and the clips: libmello's default, the OS
/// temp folder (CLIPS.md 4.4).
fn clip_dir() -> PathBuf {
    std::env::temp_dir().join("mello_clips")
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
    /// Start, change or stop the replay to match the setting, voice and the
    /// game. Called after each game event, each voice start and leave, each
    /// setting change, and each second of a switch hold.
    pub(super) fn sync_replay(&mut self) {
        // The upload pace: 3 Mbps while a game runs (CLIPS.md 15.7; 12-STREAMING.md 3.7, G8).
        crate::clip_upload::pace().set_game_running(self.game_state.current_game().is_some());
        // Game clips are Windows only until the macOS work (CLIPS.md 15.14).
        if !cfg!(windows) {
            return;
        }
        let rules = |game: Option<&ActiveGame>| {
            replay_target(
                self.replay_in_voice,
                self.replay_enabled,
                self.replay_quality,
                game,
            )
        };
        let focused = rules(self.game_state.current_game());
        let current = self
            .replay_target
            .as_ref()
            .and_then(|t| rules(self.game_state.running_game(&t.game_id)));
        let target = self.replay_hold.choose(current, focused, Instant::now());
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

    pub(super) fn handle_set_game_clips(&mut self, enabled: bool, quality: GameClipQuality) {
        if (enabled, quality) == (self.replay_enabled, self.replay_quality) {
            return;
        }
        log::info!(
            "[replay] game clips {}, quality {}",
            if enabled { "on" } else { "off" },
            quality.as_str()
        );
        self.replay_enabled = enabled;
        self.replay_quality = quality;
        self.sync_replay();
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

/// The wait before the next start after `failures` failed starts in a row.
/// A new game shows its window seconds after its process starts, and a game
/// makes its window again on a mode change: 3 s, then double, at most 30 s.
pub(crate) fn retry_delay(failures: u32) -> Duration {
    let doubling = failures.saturating_sub(1).min(4);
    Duration::from_secs((3u64 << doubling).min(30))
}

/// The worker thread's state.
struct Worker {
    ctx: *mut mello_sys::MelloContext,
    events: mpsc::Sender<Event>,
    /// The game to record while the user is in voice.
    wanted: Option<ReplayTarget>,
    /// libmello records `wanted` now.
    running: bool,
    /// Failed starts in a row, and when to try again.
    failures: u32,
    retry_at: Option<Instant>,
    /// Under 2 GB free: no replay until the disk has space again.
    disk_low: bool,
    disk_checked: Option<Instant>,
    /// The last state sent, so a poll sends only changes.
    sent: (GameClipState, String),
}

impl Worker {
    fn emit(&mut self, state: GameClipState, reason: String) {
        if self.sent == (state, reason.clone()) {
            return;
        }
        self.sent = (state, reason.clone());
        let game_name = self
            .wanted
            .as_ref()
            .map(|t| t.game_name.clone())
            .unwrap_or_default();
        let _ = self.events.send(Event::GameClipStateChanged {
            state,
            game_name,
            reason,
        });
    }

    fn stop_running(&mut self) {
        if self.running {
            unsafe { mello_sys::mello_replay_stop(self.ctx) };
            self.running = false;
        }
    }

    /// Reads the free space of the buffer folder's volume. An error reads
    /// as enough space: the ring files then fail on their own.
    fn disk_is_low(&mut self) -> bool {
        self.disk_checked = Some(Instant::now());
        disk_free(&std::env::temp_dir()).is_some_and(|free| disk_too_low(free, self.running))
    }

    /// CLIPS.md 15.2: stop, which deletes the ring, and tell the UI once.
    fn enter_disk_low(&mut self) {
        if !self.disk_low {
            log::warn!(
                "[replay] off: under {} GB free in {}",
                DISK_FREE_MIN >> 30,
                std::env::temp_dir().display()
            );
        }
        self.stop_running();
        self.disk_low = true;
        self.retry_at = None;
        self.emit(GameClipState::DiskLow, "the disk is almost full".into());
    }

    fn try_start(&mut self) {
        let Some(target) = self.wanted.clone() else {
            return;
        };
        if self.disk_is_low() {
            self.enter_disk_low();
            return;
        }
        self.disk_low = false;
        let (height, fps, bitrate_kbps) = target.quality.encoding();
        let config = mello_sys::MelloReplayConfig {
            hwnd: std::ptr::null_mut(),
            pid: target.pid,
            height,
            fps,
            bitrate_kbps,
            buffer_dir: std::ptr::null(),
        };
        #[cfg(feature = "e2e-game")]
        let result = if crate::e2e_game::is_fake_pid(target.pid) {
            // A fake game has no window: the replay records a reference clip
            // in its place (CLIPS.md 15.13).
            crate::e2e_game::start_reference_replay(self.ctx, &config)
        } else {
            unsafe { mello_sys::mello_replay_start(self.ctx, &config) }
        };
        #[cfg(not(feature = "e2e-game"))]
        let result = unsafe { mello_sys::mello_replay_start(self.ctx, &config) };
        if result == mello_sys::MelloResult_MELLO_OK {
            log::info!(
                "[replay] recording {} (pid {}) at {} after {} failed starts",
                target.game_name,
                target.pid,
                target.quality.as_str(),
                self.failures
            );
            self.running = true;
            self.failures = 0;
            self.retry_at = None;
            let state = game_clip_state(unsafe { mello_sys::mello_replay_state(self.ctx) });
            self.emit(state, String::new());
            return;
        }
        let reason = last_error(self.ctx);
        self.failures += 1;
        let delay = retry_delay(self.failures);
        self.retry_at = Some(Instant::now() + delay);
        if self.failures == 1 || self.sent.1 != reason {
            log::warn!(
                "[replay] not available for {} (pid {}): {}; trying again in {} s",
                target.game_name,
                target.pid,
                reason,
                delay.as_secs()
            );
        }
        self.emit(GameClipState::Unavailable, reason);
    }

    fn tick(&mut self) {
        if self.wanted.is_none() {
            return;
        }
        if self
            .disk_checked
            .is_none_or(|at| at.elapsed() >= DISK_CHECK_EVERY)
        {
            let low = self.disk_is_low();
            if low && !self.disk_low {
                self.enter_disk_low();
            } else if !low && self.disk_low {
                log::info!("[replay] the disk has space again");
                self.disk_low = false;
                self.try_start();
                return;
            }
        }
        if self.disk_low {
            return;
        }
        if self.running {
            let state = game_clip_state(unsafe { mello_sys::mello_replay_state(self.ctx) });
            if state == GameClipState::Unavailable {
                // The capture or the encoder failed (the game made its window
                // again, a device was lost): start again later.
                log::warn!("[replay] the replay failed; starting again");
                self.stop_running();
                self.failures = 1;
                self.retry_at = Some(Instant::now() + retry_delay(1));
                self.emit(state, "the capture stopped".into());
            } else {
                self.emit(state, String::new());
            }
        } else if self.retry_at.is_some_and(|at| Instant::now() >= at) {
            self.try_start();
        }
    }
}

fn run(ctx: SendCtx, rx: mpsc::Receiver<Op>, events: mpsc::Sender<Event>) {
    let mut w = Worker {
        ctx: ctx.0,
        events,
        wanted: None,
        running: false,
        failures: 0,
        retry_at: None,
        disk_low: false,
        disk_checked: None,
        sent: (GameClipState::Off, String::new()),
    };
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Op::Start(target)) => {
                if w.wanted.as_ref() == Some(&target) {
                    continue;
                }
                w.stop_running();
                w.wanted = Some(target);
                w.failures = 0;
                w.retry_at = None;
                w.try_start();
            }
            Ok(Op::Stop) => {
                if w.running {
                    log::info!("[replay] stopped");
                }
                w.stop_running();
                w.wanted = None;
                w.retry_at = None;
                w.disk_low = false;
                w.emit(GameClipState::Off, String::new());
            }
            Ok(Op::Capture { seconds }) => {
                let event = match (&w.wanted, w.running) {
                    (Some(target), true) => capture(w.ctx, seconds, target),
                    _ => Event::GameClipFailed {
                        reason: "Clips are not available now".into(),
                    },
                };
                let _ = w.events.send(event);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => w.tick(),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                w.stop_running();
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
    let dir = clip_dir();
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

    fn other_game(pid: u32) -> ActiveGame {
        ActiveGame {
            game_id: "hearthstone".into(),
            game_name: "Hearthstone".into(),
            exe: "Hearthstone.exe".into(),
            ..game(pid)
        }
    }

    fn target(game: &ActiveGame) -> Option<ReplayTarget> {
        replay_target(true, true, GameClipQuality::default(), Some(game))
    }

    #[test]
    fn the_replay_records_the_game_only_in_voice() {
        let cs2 = game(4242);
        let q = GameClipQuality::default();
        assert_eq!(replay_target(false, true, q, Some(&cs2)), None);
        assert_eq!(replay_target(true, true, q, None), None);
        let target = replay_target(true, true, q, Some(&cs2)).expect("in voice with a game");
        assert_eq!(target.pid, 4242);
        assert_eq!(target.game_id, "counter-strike-2");
        assert_eq!(target.quality, GameClipQuality::P720Fps60);
    }

    #[test]
    fn the_setting_off_records_nothing() {
        let q = GameClipQuality::default();
        assert_eq!(replay_target(true, false, q, Some(&game(4242))), None);
    }

    #[test]
    fn a_failed_start_tries_again_sooner_first_then_every_30_s() {
        let secs: Vec<u64> = (1..=7).map(|n| retry_delay(n).as_secs()).collect();
        assert_eq!(secs, vec![3, 6, 12, 24, 30, 30, 30]);
    }

    #[test]
    fn a_game_with_no_process_is_not_recorded() {
        assert_eq!(target(&game(0)), None);
    }

    #[test]
    fn a_new_game_in_focus_takes_the_replay_after_10_s() {
        let t0 = Instant::now();
        let cs2 = target(&game(1));
        let hs = target(&other_game(2));
        let mut hold = GameHold::default();

        // The first game starts at once.
        assert_eq!(hold.choose(None, cs2.clone(), t0), cs2);
        assert!(!hold.waiting());

        // Hearthstone in focus: CS2 keeps the replay for 10 s.
        assert_eq!(hold.choose(cs2.clone(), hs.clone(), t0), cs2);
        assert!(hold.waiting());
        let t9 = t0 + Duration::from_secs(9);
        assert_eq!(hold.choose(cs2.clone(), hs.clone(), t9), cs2);
        let t10 = t0 + SWITCH_HOLD;
        assert_eq!(hold.choose(cs2.clone(), hs.clone(), t10), hs);
        assert!(!hold.waiting());
    }

    #[test]
    fn an_alt_tab_back_inside_the_hold_keeps_the_replay() {
        let t0 = Instant::now();
        let cs2 = target(&game(1));
        let hs = target(&other_game(2));
        let mut hold = GameHold::default();

        assert_eq!(hold.choose(cs2.clone(), hs.clone(), t0), cs2);
        let t5 = t0 + Duration::from_secs(5);
        assert_eq!(hold.choose(cs2.clone(), cs2.clone(), t5), cs2);
        assert!(!hold.waiting());

        // A second alt-tab starts a new 10 s hold, not the rest of the old one.
        let t8 = t0 + Duration::from_secs(8);
        assert_eq!(hold.choose(cs2.clone(), hs.clone(), t8), cs2);
        assert_eq!(hold.choose(cs2.clone(), hs.clone(), t0 + SWITCH_HOLD), cs2);
        assert_eq!(hold.choose(cs2.clone(), hs.clone(), t8 + SWITCH_HOLD), hs);
    }

    #[test]
    fn an_exit_voice_or_the_setting_change_the_replay_at_once() {
        let t0 = Instant::now();
        let cs2 = target(&game(1));
        let hs = target(&other_game(2));
        let mut hold = GameHold::default();
        assert_eq!(hold.choose(cs2.clone(), hs.clone(), t0), cs2);

        // CS2 exits: `current` is gone, and Hearthstone starts at once.
        assert_eq!(hold.choose(None, hs.clone(), t0), hs);
        assert!(!hold.waiting());

        // Out of voice or the setting off: nothing is in focus under the rules.
        assert_eq!(hold.choose(hs.clone(), None, t0), None);
        assert!(!hold.waiting());
    }

    #[test]
    fn a_new_quality_or_pid_of_the_same_game_applies_at_once() {
        let t0 = Instant::now();
        let cs2 = target(&game(1));
        let mut hold = GameHold::default();

        let mut at_1080 = cs2.clone().expect("a target");
        at_1080.quality = GameClipQuality::P1080Fps60;
        assert_eq!(
            hold.choose(cs2.clone(), Some(at_1080.clone()), t0),
            Some(at_1080)
        );
        let new_pid = target(&game(7));
        assert_eq!(hold.choose(cs2, new_pid.clone(), t0), new_pid);
        assert!(!hold.waiting());
    }

    #[test]
    fn the_replay_needs_2_gb_free_and_its_own_ring_counts_as_free() {
        let gb = 1024 * 1024 * 1024;
        assert!(disk_too_low(2 * gb - 1, false));
        assert!(!disk_too_low(2 * gb, false));
        // A start at 2.0 GB leaves about 1.93 GB once the ring files exist.
        assert!(!disk_too_low(2 * gb - 72 * 1024 * 1024, true));
        assert!(disk_too_low(2 * gb - RING_ALLOWANCE - 1, true));
    }

    #[cfg(windows)]
    #[test]
    fn the_free_space_of_the_temp_folder_reads() {
        let free = disk_free(&std::env::temp_dir()).expect("GetDiskFreeSpaceExW");
        assert!(free > 0);
        assert_eq!(disk_free(Path::new("Q:\\no\\such\\folder")), None);
    }

    #[test]
    fn each_clip_quality_has_its_encoding_and_its_stored_name() {
        for q in [
            GameClipQuality::P1080Fps60,
            GameClipQuality::P720Fps60,
            GameClipQuality::P720Fps30,
        ] {
            assert_eq!(GameClipQuality::parse(q.as_str()), Some(q));
        }
        assert_eq!(GameClipQuality::parse("4k"), None);
        assert_eq!(GameClipQuality::P1080Fps60.encoding(), (1080, 60, 8000));
        assert_eq!(GameClipQuality::P720Fps30.encoding(), (720, 30, 3000));
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
