use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::game_db::GameDatabase;
use crate::session_store;

const GAME_SCAN_INTERVAL: Duration = Duration::from_secs(15);
const MAX_PROCESSES: usize = 512;
/// An unknown-game candidate must appear in this many consecutive scans
/// before it is surfaced (filters transient fullscreen apps like installers).
const UNKNOWN_DEBOUNCE_SCANS: u32 = 2;

/// Executables that look game-like (fullscreen/foreground) but never are.
/// Lowercase. Browsers, media players, launchers, comms, capture, system.
const UNKNOWN_DENYLIST: &[&str] = &[
    // browsers
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "brave.exe",
    "opera.exe",
    "opera_gx.exe",
    "vivaldi.exe",
    // media
    "vlc.exe",
    "wmplayer.exe",
    "mpc-hc64.exe",
    "spotify.exe",
    // launchers/stores
    "steam.exe",
    "steamwebhelper.exe",
    "epicgameslauncher.exe",
    "battle.net.exe",
    "riotclientservices.exe",
    "riotclientux.exe",
    "eadesktop.exe",
    "galaxyclient.exe",
    "upc.exe",
    "ubisoftconnect.exe",
    // comms/capture
    "discord.exe",
    "slack.exe",
    "zoom.exe",
    "teams.exe",
    "ms-teams.exe",
    "obs64.exe",
    // terminals/editors/dev tools
    "windowsterminal.exe",
    "wt.exe",
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "conhost.exe",
    "code.exe",
    "cursor.exe",
    "devenv.exe",
    "claude.exe",
    // system/self
    "explorer.exe",
    "taskmgr.exe",
    "mello.exe",
    "mello-client.exe",
];

#[derive(Debug, Clone)]
pub struct ActiveGame {
    pub game_id: String,
    pub game_name: String,
    pub short_name: String,
    pub color: String,
    pub exe: String,
    pub pid: u32,
    /// When the *game process* started, not when we noticed it (Unix ms).
    /// Falls back to the first scan that saw it when libmello could not read
    /// the creation time, so a session is never dated from the future.
    pub started_at: i64,
    /// Process creation time exactly as libmello reported it (0 = unknown).
    /// Kept separate from `started_at` because only an unfudged value can
    /// identify the process across a restart.
    pub started_at_ms: i64,
    /// Milliseconds this game has held the foreground, accumulated per scan.
    pub foreground_ms: i64,
}

#[derive(Debug, Clone)]
pub enum GameEvent {
    Started(ActiveGame),
    /// A game exited. `ended_at` is when it was last seen alive, which is not
    /// "now" for sessions recovered after a client restart.
    Stopped {
        game: Box<ActiveGame>,
        ended_at: i64,
    },
    /// The game the user is actually looking at changed. Drives presence and
    /// the NOW PLAYING bar; `None` when no game holds focus.
    PrimaryChanged {
        pid: Option<u32>,
    },
    /// A fullscreen/foreground process outside the game DB, surfaced (after
    /// debounce) for the one-tap "track it?" confirm flow. Purely local —
    /// nothing is tracked or broadcast unless the user confirms.
    UnknownCandidate {
        exe: String,
        path: String,
        window_title: String,
    },
}

/// Wrapper to make raw pointer Send-safe for the sensor thread.
/// Safety: MelloContext is only used for stateless process enumeration.
struct SendCtx(*mut mello_sys::MelloContext);
unsafe impl Send for SendCtx {}
unsafe impl Sync for SendCtx {}

pub struct GameSensor {
    _handle: Option<std::thread::JoinHandle<()>>,
}

impl GameSensor {
    /// Start the background scan loop. Returns the sensor handle and a receiver
    /// for game events. The database is shared so user-confirmed custom games
    /// apply live (Command::AddCustomGame writes through the same lock).
    pub fn start(
        ctx: *mut mello_sys::MelloContext,
        db: Arc<RwLock<GameDatabase>>,
    ) -> (Self, std::sync::mpsc::Receiver<GameEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let send_ctx = SendCtx(ctx);

        let handle = std::thread::Builder::new()
            .name("game-sensor".into())
            .spawn(move || {
                scan_loop(&send_ctx, &db, &tx);
            })
            .expect("failed to spawn game-sensor thread");

        (
            GameSensor {
                _handle: Some(handle),
            },
            rx,
        )
    }
}

fn scan_loop(ctx: &SendCtx, db: &Arc<RwLock<GameDatabase>>, tx: &Sender<GameEvent>) {
    // Every game currently running, keyed by pid. v1 tracked a single
    // Option<ActiveGame>, which made alt-tabbing between two games look like a
    // stop/start pair and silently discarded the background game's session.
    let mut active: HashMap<u32, ActiveGame> = HashMap::new();
    let mut primary: Option<u32> = None;
    let mut unknown = UnknownTracker::default();
    let mut orphans = session_store::take();
    if !orphans.is_empty() {
        log::info!(
            "[game-sensor] {} session(s) open when the client last exited",
            orphans.len()
        );
    }

    log::info!(
        "[game-sensor] scan loop started (interval={:?})",
        GAME_SCAN_INTERVAL
    );

    loop {
        // Scan first, sleep after: the reconciliation below must run at
        // startup, not one interval late.
        let processes = enumerate_game_processes(ctx.0);
        let now = now_ms();

        let (detected, candidate) = {
            let db = db.read().expect("game db lock poisoned");
            let detected = detect_games(&db, &processes, now);
            // Unknown-game candidates only matter while no DB game is active.
            let candidate = if detected.is_empty() {
                pick_unknown_candidate(&db, &processes)
            } else {
                None
            };
            (detected, candidate)
        };

        // Restart recovery, first scan only. A session whose process is still
        // alive resumes with its original start time; one whose process is
        // gone is closed out at the last time we saw it, so the night is
        // recorded instead of vanishing.
        if !orphans.is_empty() {
            for orphan in std::mem::take(&mut orphans) {
                match detected
                    .iter()
                    .find(|g| session_store::resume_matches(&orphan, g.pid, g.started_at_ms))
                {
                    Some(live) => {
                        log::info!(
                            "[game-sensor] resuming session: {} (pid={}, running since {})",
                            orphan.game_name,
                            orphan.pid,
                            orphan.started_at_ms
                        );
                        let mut resumed = live.clone();
                        resumed.foreground_ms = orphan.foreground_ms;
                        active.insert(resumed.pid, resumed.clone());
                        if tx.send(GameEvent::Started(resumed)).is_err() {
                            return;
                        }
                    }
                    None => {
                        log::info!(
                            "[game-sensor] closing orphaned session: {} (ended by {})",
                            orphan.game_name,
                            orphan.last_seen_ms
                        );
                        let ended_at = orphan.last_seen_ms;
                        let game = ActiveGame {
                            game_id: orphan.game_id,
                            game_name: orphan.game_name,
                            short_name: orphan.short_name,
                            color: orphan.color,
                            exe: orphan.exe,
                            pid: orphan.pid,
                            started_at: orphan.started_at_ms,
                            started_at_ms: orphan.started_at_ms,
                            foreground_ms: orphan.foreground_ms,
                        };
                        let stopped = GameEvent::Stopped {
                            game: Box::new(game),
                            ended_at,
                        };
                        if tx.send(stopped).is_err() {
                            return;
                        }
                    }
                }
            }
        }

        if let Some(cand) = unknown.observe(candidate) {
            if let GameEvent::UnknownCandidate {
                exe, window_title, ..
            } = &cand
            {
                log::info!("[game-sensor] unknown game candidate: {exe} ({window_title})");
            }
            if tx.send(cand).is_err() {
                break;
            }
        }

        // --- diff the live set against what we were tracking ---
        let live: HashMap<u32, ActiveGame> = detected.into_iter().map(|g| (g.pid, g)).collect();

        for (pid, game) in &live {
            if !active.contains_key(pid) {
                log::info!("[game-sensor] game started: {} (pid={pid})", game.game_name);
                if tx.send(GameEvent::Started(game.clone())).is_err() {
                    return;
                }
            }
        }

        let gone: Vec<u32> = active
            .keys()
            .copied()
            .filter(|p| !live.contains_key(p))
            .collect();
        for pid in gone {
            if let Some(game) = active.remove(&pid) {
                log::info!("[game-sensor] game stopped: {} (pid={pid})", game.game_name);
                let stopped = GameEvent::Stopped {
                    game: Box::new(game),
                    ended_at: now,
                };
                if tx.send(stopped).is_err() {
                    return;
                }
            }
        }

        // Carry accumulated foreground time forward onto the fresh scan data.
        let interval_ms = GAME_SCAN_INTERVAL.as_millis() as i64;
        for (pid, mut game) in live {
            let carried = active.get(&pid).map_or(0, |prev| prev.foreground_ms);
            let is_fg = processes
                .iter()
                .any(|p| p.pid == pid && (p.is_foreground || p.is_fullscreen));
            game.foreground_ms = carried + if is_fg { interval_ms } else { 0 };
            // A resumed session keeps the start time we recovered.
            if let Some(prev) = active.get(&pid) {
                game.started_at = prev.started_at;
            }
            active.insert(pid, game);
        }

        let next_primary = pick_primary(&active, &processes);
        if next_primary != primary {
            log::info!(
                "[game-sensor] primary game: {}",
                next_primary
                    .and_then(|p| active.get(&p))
                    .map_or("none", |g| g.game_name.as_str())
            );
            primary = next_primary;
            if tx.send(GameEvent::PrimaryChanged { pid: primary }).is_err() {
                return;
            }
        }

        persist(&active, now);

        std::thread::sleep(GAME_SCAN_INTERVAL);
    }

    log::info!("[game-sensor] scan loop ended");
}

fn persist(active: &HashMap<u32, ActiveGame>, now: i64) {
    let sessions: Vec<session_store::PersistedSession> = active
        .values()
        .map(|g| session_store::PersistedSession {
            pid: g.pid,
            started_at_ms: g.started_at_ms,
            game_id: g.game_id.clone(),
            game_name: g.game_name.clone(),
            short_name: g.short_name.clone(),
            color: g.color.clone(),
            exe: g.exe.clone(),
            last_seen_ms: now,
            foreground_ms: g.foreground_ms,
        })
        .collect();
    session_store::save(&sessions);
}

/// The game the user is actually looking at: the focused one, else the
/// fullscreen one, else the longest-running. Ties break on pid so the choice
/// does not flap between scans.
fn pick_primary(active: &HashMap<u32, ActiveGame>, processes: &[RawGameProcess]) -> Option<u32> {
    let rank = |pid: u32| -> (u8, i64, u32) {
        let p = processes.iter().find(|p| p.pid == pid);
        let tier = match p {
            Some(p) if p.is_foreground => 0,
            Some(p) if p.is_fullscreen => 1,
            _ => 2,
        };
        (tier, active.get(&pid).map_or(0, |g| g.started_at), pid)
    };
    active.keys().copied().min_by_key(|&pid| rank(pid))
}

/// Install-location classes that are never games: system dirs, Store apps,
/// and `%LOCALAPPDATA%\Programs` (the default Electron install target —
/// Claude, VS Code, Discord forks, Slack all live there). Lowercase,
/// matched as substrings of the full exe path.
const UNKNOWN_PATH_DENYLIST: &[&str] = &[
    "\\windows\\system32\\",
    "\\windows\\systemapps\\",
    "\\windowsapps\\",
    "\\appdata\\local\\programs\\",
];

/// A fullscreen/foreground windowed process that is not in the game DB and
/// not denied by exe name or install location. Merely-foreground candidates
/// (games played windowed) are accepted, which is why the path denylist
/// matters: any focused desktop app would otherwise qualify — the exact
/// false positives seen in testing were claude.exe (Electron under
/// AppData\Local\Programs) and WindowsTerminal.exe (a Store app).
/// Pure; debounce lives in [`UnknownTracker`].
fn pick_unknown_candidate(db: &GameDatabase, processes: &[RawGameProcess]) -> Option<GameEvent> {
    processes
        .iter()
        .filter(|p| {
            let path = p.path.to_lowercase();
            (p.is_fullscreen || p.is_foreground)
                && !p.window_title.is_empty()
                && !p.path.is_empty()
                && db.lookup_by_exe(&p.exe).is_none()
                && !UNKNOWN_DENYLIST.contains(&p.exe.to_lowercase().as_str())
                && !UNKNOWN_PATH_DENYLIST.iter().any(|d| path.contains(d))
        })
        // Fullscreen beats merely-foreground when both qualify.
        .max_by_key(|p| p.is_fullscreen)
        .map(|p| GameEvent::UnknownCandidate {
            exe: p.exe.clone(),
            path: p.path.clone(),
            window_title: p.window_title.clone(),
        })
}

/// Debounce for unknown-game candidates: a candidate must survive
/// `UNKNOWN_DEBOUNCE_SCANS` consecutive scans and is emitted once per exe per
/// run. Pure state machine, unit-tested below.
#[derive(Default)]
struct UnknownTracker {
    counts: HashMap<String, u32>,
    emitted: HashSet<String>,
}

impl UnknownTracker {
    fn observe(&mut self, candidate: Option<GameEvent>) -> Option<GameEvent> {
        let Some(cand) = candidate else {
            self.counts.clear();
            return None;
        };
        let GameEvent::UnknownCandidate { exe, .. } = &cand else {
            return None;
        };
        let key = exe.to_lowercase();
        // A different candidate resets the streaks of everything else.
        self.counts.retain(|k, _| *k == key);
        let count = self.counts.entry(key.clone()).or_insert(0);
        *count += 1;
        if *count >= UNKNOWN_DEBOUNCE_SCANS && !self.emitted.contains(&key) {
            self.emitted.insert(key);
            return Some(cand);
        }
        None
    }
}

pub(crate) struct RawGameProcess {
    pub(crate) pid: u32,
    #[allow(dead_code)]
    pub(crate) name: String,
    pub(crate) exe: String,
    pub(crate) is_fullscreen: bool,
    /// Full executable path; empty for windowless processes.
    pub(crate) path: String,
    /// Main window title; empty for windowless processes.
    pub(crate) window_title: String,
    pub(crate) is_foreground: bool,
    /// Process creation time in Unix ms; 0 when libmello could not read it.
    pub(crate) started_at_ms: i64,
}

fn enumerate_game_processes(ctx: *mut mello_sys::MelloContext) -> Vec<RawGameProcess> {
    let mut buf = vec![
        mello_sys::MelloGameProcess {
            pid: 0,
            name: [0i8; 128],
            exe: [0i8; 260],
            is_fullscreen: false,
            path: [0i8; 520],
            title: [0i8; 256],
            is_foreground: false,
            started_at_ms: 0,
        };
        MAX_PROCESSES
    ];

    let count =
        unsafe { mello_sys::mello_enumerate_games(ctx, buf.as_mut_ptr(), MAX_PROCESSES as i32) };

    let cstr = |arr: &[i8]| {
        unsafe { std::ffi::CStr::from_ptr(arr.as_ptr()) }
            .to_string_lossy()
            .to_string()
    };

    let mut out = Vec::new();
    for gp in buf.iter().take(count.max(0) as usize) {
        out.push(RawGameProcess {
            pid: gp.pid,
            name: cstr(&gp.name),
            exe: cstr(&gp.exe),
            is_fullscreen: gp.is_fullscreen,
            path: cstr(&gp.path),
            window_title: cstr(&gp.title),
            is_foreground: gp.is_foreground,
            started_at_ms: gp.started_at_ms,
        });
    }
    out
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Every running process the database recognises as a game.
///
/// v1 returned only one, so a second game running alongside was invisible and
/// alt-tabbing between two looked like the first had exited. `now` is passed in
/// rather than read here so a single scan timestamps consistently.
fn detect_games(db: &GameDatabase, processes: &[RawGameProcess], now: i64) -> Vec<ActiveGame> {
    processes
        .iter()
        .filter_map(|p| {
            let entry = db.lookup_by_exe(&p.exe)?;
            Some(ActiveGame {
                game_id: entry.id.clone(),
                game_name: entry.name.clone(),
                short_name: entry.short_name.clone(),
                color: entry.color.clone().unwrap_or_else(|| "#888888".into()),
                exe: p.exe.clone(),
                pid: p.pid,
                // The real process start when we have it. Falling back to the
                // scan time is what v1 always did, and it is why a game
                // already running at client launch reported minutes instead of
                // hours — but a bogus 0 would report 1970, which is worse.
                started_at: if p.started_at_ms > 0 {
                    p.started_at_ms
                } else {
                    now
                },
                started_at_ms: p.started_at_ms,
                foreground_ms: 0,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_process(exe: &str, pid: u32, fullscreen: bool) -> RawGameProcess {
        RawGameProcess {
            pid,
            name: exe.to_string(),
            exe: exe.to_string(),
            is_fullscreen: fullscreen,
            path: format!("C:\\Games\\{exe}"),
            window_title: exe.trim_end_matches(".exe").to_string(),
            is_foreground: false,
            started_at_ms: 0,
        }
    }

    fn test_db() -> GameDatabase {
        GameDatabase::load_bundled()
    }

    const NOW: i64 = 1_700_000_000_000;

    fn active_map(games: Vec<ActiveGame>) -> HashMap<u32, ActiveGame> {
        games.into_iter().map(|g| (g.pid, g)).collect()
    }

    #[test]
    fn detect_no_processes() {
        let db = test_db();
        assert!(detect_games(&db, &[], NOW).is_empty());
    }

    #[test]
    fn detect_single_match() {
        let db = test_db();
        let procs = vec![make_process("cs2.exe", 1234, false)];
        let found = detect_games(&db, &procs, NOW);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].game_id, "counter-strike-2");
    }

    #[test]
    fn detect_unknown_exe_filtered() {
        let db = test_db();
        let procs = vec![make_process("notepad.exe", 999, false)];
        assert!(detect_games(&db, &procs, NOW).is_empty());
    }

    #[test]
    fn detect_returns_every_running_game() {
        // v1 returned Option and lost the second game entirely; both sessions
        // must now be tracked.
        let db = test_db();
        let procs = vec![
            make_process("cs2.exe", 1234, false),
            make_process("dota2.exe", 5678, true),
        ];
        let found = detect_games(&db, &procs, NOW);
        assert_eq!(found.len(), 2);
        let mut ids: Vec<&str> = found.iter().map(|g| g.game_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["counter-strike-2", "dota-2"]);
    }

    #[test]
    fn detect_uses_real_process_start_time() {
        let db = test_db();
        let mut proc = make_process("cs2.exe", 1234, false);
        proc.started_at_ms = NOW - 3 * 3_600_000; // running for three hours
        let found = detect_games(&db, &[proc], NOW);
        // The session must date from when the game started, not from the scan
        // that first noticed it — this is the "played 4hrs" honesty fix.
        assert_eq!(found[0].started_at, NOW - 3 * 3_600_000);
        assert_eq!(found[0].started_at_ms, NOW - 3 * 3_600_000);
    }

    #[test]
    fn detect_falls_back_to_scan_time_when_start_unknown() {
        let db = test_db();
        let proc = make_process("cs2.exe", 1234, false); // started_at_ms = 0
        let found = detect_games(&db, &[proc], NOW);
        // Never 0 — that would date the session to 1970 and report a 55-year
        // session. The scan time is the honest floor.
        assert_eq!(found[0].started_at, NOW);
        assert_eq!(found[0].started_at_ms, 0);
    }

    #[test]
    fn primary_prefers_foreground_over_fullscreen() {
        let db = test_db();
        let mut fullscreen = make_process("dota2.exe", 5678, true);
        fullscreen.started_at_ms = NOW - 1000;
        let mut focused = make_process("cs2.exe", 1234, false);
        focused.is_foreground = true;
        focused.started_at_ms = NOW - 500;
        let procs = vec![fullscreen, focused];
        let active = active_map(detect_games(&db, &procs, NOW));
        // v1 sorted on is_fullscreen only and ignored is_foreground entirely,
        // so the game you were actually looking at lost to a backgrounded one.
        assert_eq!(pick_primary(&active, &procs), Some(1234));
    }

    #[test]
    fn primary_falls_back_to_fullscreen_then_oldest() {
        let db = test_db();
        let procs = vec![
            make_process("cs2.exe", 1234, false),
            make_process("dota2.exe", 5678, true),
        ];
        let active = active_map(detect_games(&db, &procs, NOW));
        assert_eq!(pick_primary(&active, &procs), Some(5678));

        // Neither focused nor fullscreen: the longest-running wins, and the
        // answer is stable across calls rather than hash-order dependent.
        let plain = vec![
            make_process("cs2.exe", 1234, false),
            make_process("dota2.exe", 5678, false),
        ];
        let mut games = detect_games(&db, &plain, NOW);
        games[0].started_at = NOW - 10_000;
        games[1].started_at = NOW - 90_000;
        let active = active_map(games);
        assert_eq!(pick_primary(&active, &plain), Some(5678));
        assert_eq!(pick_primary(&active, &plain), Some(5678));
    }

    #[test]
    fn primary_is_none_without_games() {
        assert_eq!(pick_primary(&HashMap::new(), &[]), None);
    }

    // ------------------------------------------------------------------
    // Unknown-game candidate heuristic + debounce
    // ------------------------------------------------------------------

    fn unknown_proc(exe: &str, fullscreen: bool, foreground: bool) -> RawGameProcess {
        RawGameProcess {
            pid: 4242,
            name: exe.to_string(),
            exe: exe.to_string(),
            is_fullscreen: fullscreen,
            path: format!("C:\\Games\\{exe}"),
            window_title: exe.trim_end_matches(".exe").to_string(),
            is_foreground: foreground,
            started_at_ms: 0,
        }
    }

    fn candidate_exe(ev: &GameEvent) -> &str {
        match ev {
            GameEvent::UnknownCandidate { exe, .. } => exe,
            other => panic!("expected UnknownCandidate, got {other:?}"),
        }
    }

    #[test]
    fn unknown_candidate_requires_fullscreen_or_foreground() {
        let db = test_db();
        assert!(
            pick_unknown_candidate(&db, &[unknown_proc("night stones.exe", false, false)])
                .is_none()
        );
        let c = pick_unknown_candidate(&db, &[unknown_proc("night stones.exe", true, false)])
            .expect("fullscreen unknown exe qualifies");
        assert_eq!(candidate_exe(&c), "night stones.exe");
        assert!(
            pick_unknown_candidate(&db, &[unknown_proc("night stones.exe", false, true)]).is_some()
        );
    }

    #[test]
    fn unknown_candidate_skips_db_games_and_denylist() {
        let db = GameDatabase::load_bundled();
        // A DB game is never an unknown candidate.
        assert!(pick_unknown_candidate(&db, &[unknown_proc("cs2.exe", true, true)]).is_none());
        // Denylisted apps are never candidates, however game-like they look.
        assert!(pick_unknown_candidate(&db, &[unknown_proc("chrome.exe", true, true)]).is_none());
        assert!(pick_unknown_candidate(&db, &[unknown_proc("OBS64.exe", true, true)]).is_none());
    }

    #[test]
    fn unknown_candidate_rejects_denied_install_locations() {
        let db = test_db();
        // The exact false positives from live testing: an Electron app under
        // AppData\Local\Programs and a Store app under WindowsApps.
        let mut electron = unknown_proc("someapp.exe", false, true);
        electron.path = "C:\\Users\\bob\\AppData\\Local\\Programs\\SomeApp\\someapp.exe".into();
        assert!(pick_unknown_candidate(&db, &[electron]).is_none());

        let mut store_app = unknown_proc("terminal.exe", false, true);
        store_app.path = "C:\\Program Files\\WindowsApps\\Microsoft.Terminal\\terminal.exe".into();
        assert!(pick_unknown_candidate(&db, &[store_app]).is_none());

        // A Steam-library exe stays eligible.
        let mut steam_game = unknown_proc("night stones.exe", false, true);
        steam_game.path =
            "C:\\Program Files (x86)\\Steam\\steamapps\\common\\Night Stones\\night stones.exe"
                .into();
        assert!(pick_unknown_candidate(&db, &[steam_game]).is_some());
    }

    #[test]
    fn unknown_candidate_requires_window_and_path() {
        let db = test_db();
        let mut no_title = unknown_proc("mystery.exe", true, true);
        no_title.window_title = String::new();
        assert!(pick_unknown_candidate(&db, &[no_title]).is_none());

        let mut no_path = unknown_proc("mystery.exe", true, true);
        no_path.path = String::new();
        assert!(pick_unknown_candidate(&db, &[no_path]).is_none());
    }

    #[test]
    fn unknown_candidate_prefers_fullscreen_over_foreground() {
        let db = test_db();
        let c = pick_unknown_candidate(
            &db,
            &[
                unknown_proc("fg-only.exe", false, true),
                unknown_proc("fullscreen.exe", true, false),
            ],
        )
        .unwrap();
        assert_eq!(candidate_exe(&c), "fullscreen.exe");
    }

    #[test]
    fn tracker_debounces_and_emits_once() {
        let db = test_db();
        let mut tracker = UnknownTracker::default();
        let cand = || pick_unknown_candidate(&db, &[unknown_proc("night stones.exe", true, true)]);

        // First scan: seen but not yet emitted.
        assert!(tracker.observe(cand()).is_none());
        // Second consecutive scan: emitted.
        assert!(tracker.observe(cand()).is_some());
        // Never emitted again this run.
        assert!(tracker.observe(cand()).is_none());
        assert!(tracker.observe(cand()).is_none());
    }

    #[test]
    fn tracker_resets_on_gap_or_different_candidate() {
        let db = test_db();
        let mut tracker = UnknownTracker::default();
        let a = || pick_unknown_candidate(&db, &[unknown_proc("aaa.exe", true, true)]);
        let b = || pick_unknown_candidate(&db, &[unknown_proc("bbb.exe", true, true)]);

        assert!(tracker.observe(a()).is_none());
        // Gap (no candidate) resets the streak.
        assert!(tracker.observe(None).is_none());
        assert!(tracker.observe(a()).is_none());
        // A different candidate also resets the streak for "aaa".
        assert!(tracker.observe(b()).is_none());
        assert!(tracker.observe(a()).is_none());
        assert!(tracker.observe(a()).is_some());
    }
}
