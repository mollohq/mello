use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::game_db::GameDatabase;

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
    pub started_at: i64,
}

#[derive(Debug, Clone)]
pub enum GameEvent {
    Started(ActiveGame),
    Stopped(ActiveGame),
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
    let mut previous: Option<ActiveGame> = None;
    let mut unknown = UnknownTracker::default();
    log::info!(
        "[game-sensor] scan loop started (interval={:?})",
        GAME_SCAN_INTERVAL
    );

    loop {
        std::thread::sleep(GAME_SCAN_INTERVAL);

        let processes = enumerate_game_processes(ctx.0);
        let (detected, candidate) = {
            let db = db.read().expect("game db lock poisoned");
            let detected = pick_primary_game(&db, &processes);
            // Unknown-game candidates only matter while no DB game is active.
            let candidate = if detected.is_none() {
                pick_unknown_candidate(&db, &processes)
            } else {
                None
            };
            (detected, candidate)
        };

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

        match (&previous, &detected) {
            (None, Some(game)) => {
                log::info!(
                    "[game-sensor] game started: {} (pid={})",
                    game.game_name,
                    game.pid
                );
                if tx.send(GameEvent::Started(game.clone())).is_err() {
                    break;
                }
            }
            (Some(prev), None) => {
                log::info!("[game-sensor] game stopped: {}", prev.game_name);
                if tx.send(GameEvent::Stopped(prev.clone())).is_err() {
                    break;
                }
            }
            (Some(prev), Some(game)) if prev.pid != game.pid => {
                log::info!(
                    "[game-sensor] game switched: {} -> {}",
                    prev.game_name,
                    game.game_name
                );
                let _ = tx.send(GameEvent::Stopped(prev.clone()));
                if tx.send(GameEvent::Started(game.clone())).is_err() {
                    break;
                }
            }
            _ => {}
        }

        previous = detected;
    }

    log::info!("[game-sensor] scan loop ended");
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

fn pick_primary_game(db: &GameDatabase, processes: &[RawGameProcess]) -> Option<ActiveGame> {
    let mut matches: Vec<(ActiveGame, bool)> = processes
        .iter()
        .filter_map(|p| {
            let entry = db.lookup_by_exe(&p.exe)?;
            Some((
                ActiveGame {
                    game_id: entry.id.clone(),
                    game_name: entry.name.clone(),
                    short_name: entry.short_name.clone(),
                    color: entry.color.clone().unwrap_or_else(|| "#888888".into()),
                    exe: p.exe.clone(),
                    pid: p.pid,
                    started_at: now_ms(),
                },
                p.is_fullscreen,
            ))
        })
        .collect();

    // Prefer fullscreen games (likely the active one)
    matches.sort_by(|a, b| b.1.cmp(&a.1));
    matches.into_iter().next().map(|(game, _)| game)
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
        }
    }

    fn test_db() -> GameDatabase {
        GameDatabase::load_bundled()
    }

    #[test]
    fn pick_primary_no_processes() {
        let db = test_db();
        assert!(pick_primary_game(&db, &[]).is_none());
    }

    #[test]
    fn pick_primary_single_match() {
        let db = test_db();
        let procs = vec![make_process("cs2.exe", 1234, false)];
        let result = pick_primary_game(&db, &procs);
        assert!(result.is_some());
        assert_eq!(result.unwrap().game_id, "counter-strike-2");
    }

    #[test]
    fn pick_primary_prefers_fullscreen() {
        let db = test_db();
        let procs = vec![
            make_process("cs2.exe", 1234, false),
            make_process("dota2.exe", 5678, true),
        ];
        let result = pick_primary_game(&db, &procs);
        assert!(result.is_some());
        assert_eq!(result.unwrap().game_id, "dota-2");
    }

    #[test]
    fn pick_primary_unknown_exe_filtered() {
        let db = test_db();
        let procs = vec![make_process("notepad.exe", 999, false)];
        assert!(pick_primary_game(&db, &procs).is_none());
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
