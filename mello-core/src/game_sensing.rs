use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::library::LibraryIndex;
use crate::session_store;

/// Idle cadence. Low enough to be cheap, high enough that a game start can
/// sit unnoticed for a quarter minute — which is why it tightens after any
/// change (see `GAME_SCAN_INTERVAL_ACTIVE`).
const GAME_SCAN_INTERVAL: Duration = Duration::from_secs(15);
/// Cadence for a short window after anything starts or stops. Launching a game
/// is followed by more churn (launcher exits, anti-cheat starts, the game takes
/// focus), and reacting in seconds rather than a quarter minute is the
/// difference between the bar feeling live and feeling broken.
const GAME_SCAN_INTERVAL_ACTIVE: Duration = Duration::from_secs(4);
/// How many scans stay fast after a change.
const ACTIVE_CADENCE_SCANS: u32 = 8;
/// Rescan installed libraries every this many process scans (~5 minutes).
/// Reading a few hundred small manifests is cheap but not free, and a game
/// installed mid-session only has to be noticed eventually.
const LIBRARY_RESCAN_SCANS: u32 = 20;
const MAX_PROCESSES: usize = 512;
/// How many consecutive scans a resolved game may present no window before its
/// session is closed.
///
/// A named process is not the same as a running game. Roblox drops to the tray
/// on quit: `RobloxPlayerBeta.exe` stays alive with no window, and the curated
/// exe table happily resolves it, so the session never ended. Every process
/// that was genuinely being played carried a window title — "Fortnite",
/// "VALORANT", "Counter-Strike 2" — and every lingering companion carried none.
///
/// The grace period exists so one bad read cannot split a night in two. A
/// session that closes and reopens is recorded as two sessions, which is worse
/// than ending a few seconds late.
const WINDOWLESS_GRACE_SCANS: u32 = 2;
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
    /// Full path to the running executable. Carried so the client can extract
    /// the game's own icon, which §8.2 makes the primary art source.
    pub exe_path: String,
    pub pid: u32,
    /// IGDB id when the catalogue resolved this game; `None` for a user's own
    /// custom entry the catalogue has never heard of.
    pub igdb_id: Option<u32>,
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
    ///
    /// Carries the `game_id`, not the representative pid. Which process stands
    /// for a game is this module's business: `upgrade_representative` swaps it
    /// silently when a better process appears, so a pid is not a stable handle
    /// outside the scan loop.
    PrimaryChanged {
        game_id: Option<String>,
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
    ) -> (Self, std::sync::mpsc::Receiver<GameEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let send_ctx = SendCtx(ctx);

        let handle = std::thread::Builder::new()
            .name("game-sensor".into())
            .spawn(move || {
                scan_loop(&send_ctx, &tx);
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

fn scan_loop(ctx: &SendCtx, tx: &Sender<GameEvent>) {
    // Every game currently running, keyed by pid. v1 tracked a single
    // Option<ActiveGame>, which made alt-tabbing between two games look like a
    // stop/start pair and silently discarded the background game's session.
    let mut active: HashMap<u32, ActiveGame> = HashMap::new();
    let mut primary: Option<u32> = None;
    let mut library = LibraryIndex::scan();
    let mut scans_since_library_refresh = 0u32;
    let mut fast_scans_left = ACTIVE_CADENCE_SCANS;
    let mut shadow: HashMap<u32, String> = HashMap::new();
    let mut windowless: HashMap<u32, u32> = HashMap::new();
    let mut last_scan_at: Option<Instant> = None;
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
        let scan_instant = Instant::now();
        let elapsed_ms = last_scan_at
            .map(|prev| scan_instant.duration_since(prev).as_millis() as i64)
            .unwrap_or(0);
        last_scan_at = Some(scan_instant);

        // Scan first, sleep after: the reconciliation below must run at
        // startup, not one interval late.
        let processes = enumerate_game_processes(ctx.0);
        let now = now_ms();

        let detected = detect_games(&library, &processes, now);
        let detected = drop_windowless(detected, &processes, &mut windowless);

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
                            exe_path: orphan.exe_path,
                            pid: orphan.pid,
                            // Restart recovery only knows what the previous run
                            // persisted; the id is re-resolved on next detect.
                            igdb_id: None,
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

        // --- diff the live set against what we were tracking ---
        let live: HashMap<u32, ActiveGame> = detected.into_iter().map(|g| (g.pid, g)).collect();
        let mut changed = false;
        let (start_events, starts_changed) = coalesce_starts(&mut active, &mut shadow, &live);
        for ev in start_events {
            if let GameEvent::Started(ref game) = ev {
                log::info!(
                    "[game-sensor] game started: {} (pid={})",
                    game.game_name,
                    game.pid
                );
            }
            changed |= starts_changed;
            if tx.send(ev).is_err() {
                return;
            }
        }

        let (stop_events, stops_changed) =
            coalesce_stops(&mut active, &mut shadow, &live, now, &mut primary);
        for ev in stop_events {
            if let GameEvent::Stopped { ref game, .. } = ev {
                log::info!(
                    "[game-sensor] game stopped: {} (pid={})",
                    game.game_name,
                    game.pid
                );
            }
            changed |= stops_changed;
            if tx.send(ev).is_err() {
                return;
            }
        }

        // Carry accumulated foreground time forward onto session representatives.
        for session in active.values_mut() {
            let is_fg = any_foreground_for_game(&session.game_id, &live, &processes);
            session.foreground_ms =
                accumulate_foreground_ms(session.foreground_ms, is_fg, elapsed_ms);
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
            let game_id = primary
                .and_then(|p| active.get(&p))
                .map(|g| g.game_id.clone());
            if tx.send(GameEvent::PrimaryChanged { game_id }).is_err() {
                return;
            }
        }

        persist(&active, now);

        if changed {
            fast_scans_left = ACTIVE_CADENCE_SCANS;
        }

        scans_since_library_refresh += 1;
        if scans_since_library_refresh >= LIBRARY_RESCAN_SCANS {
            scans_since_library_refresh = 0;
            library = LibraryIndex::scan();
        }

        let interval = if fast_scans_left > 0 {
            fast_scans_left -= 1;
            GAME_SCAN_INTERVAL_ACTIVE
        } else {
            GAME_SCAN_INTERVAL
        };
        std::thread::sleep(interval);
    }
    // Unreachable: every exit above returns directly when the receiver is
    // gone, which is the only way this loop ends.
}

/// Foreground time for a scan tick. Elapsed is zero on the first tick so we
/// never credit a full interval before any wall time has passed.
fn accumulate_foreground_ms(carried_ms: i64, is_foreground: bool, elapsed_ms: i64) -> i64 {
    carried_ms + if is_foreground { elapsed_ms } else { 0 }
}

fn any_foreground_for_game(
    game_id: &str,
    live: &HashMap<u32, ActiveGame>,
    processes: &[RawGameProcess],
) -> bool {
    live.iter()
        .filter(|(_, g)| g.game_id == game_id)
        .any(|(pid, _)| {
            processes
                .iter()
                .any(|p| p.pid == *pid && (p.is_foreground || p.is_fullscreen))
        })
}

fn representative_pid(active: &HashMap<u32, ActiveGame>, game_id: &str) -> Option<u32> {
    active
        .iter()
        .find(|(_, g)| g.game_id == game_id)
        .map(|(pid, _)| *pid)
}

fn representative_rank(game: &ActiveGame) -> (u8, u32) {
    (
        is_auxiliary_binary(&game.exe.to_ascii_lowercase()) as u8,
        game.pid,
    )
}

fn should_upgrade_representative(rep: &ActiveGame, candidate: &ActiveGame) -> bool {
    let rep_aux = is_auxiliary_binary(&rep.exe.to_ascii_lowercase());
    let cand_aux = is_auxiliary_binary(&candidate.exe.to_ascii_lowercase());
    rep_aux && !cand_aux
}

fn upgrade_representative(
    active: &mut HashMap<u32, ActiveGame>,
    shadow: &mut HashMap<u32, String>,
    old_pid: u32,
    new_pid: u32,
    candidate: &ActiveGame,
) {
    let mut upgraded = candidate.clone();
    if let Some(old) = active.remove(&old_pid) {
        upgraded.foreground_ms = old.foreground_ms;
        upgraded.started_at = old.started_at;
        upgraded.started_at_ms = old.started_at_ms;
        shadow.insert(old_pid, upgraded.game_id.clone());
    }
    active.insert(new_pid, upgraded);
}

/// Open sessions for newly seen processes. A second pid with the same `game_id`
/// is tracked silently so helper binaries under one install cannot duplicate
/// ledger rows.
fn coalesce_starts(
    active: &mut HashMap<u32, ActiveGame>,
    shadow: &mut HashMap<u32, String>,
    live: &HashMap<u32, ActiveGame>,
) -> (Vec<GameEvent>, bool) {
    let mut events = Vec::new();
    let mut changed = false;

    let mut newcomers: Vec<(u32, ActiveGame)> = live
        .iter()
        .filter(|(pid, _)| !active.contains_key(pid) && !shadow.contains_key(pid))
        .map(|(pid, game)| (*pid, game.clone()))
        .collect();
    newcomers.sort_by_key(|(_, g)| representative_rank(g));

    let mut opened: HashSet<String> = HashSet::new();
    for (pid, game) in newcomers {
        if let Some(rep_pid) = representative_pid(active, &game.game_id) {
            let rep = active.get(&rep_pid).expect("representative exists");
            if should_upgrade_representative(rep, &game) {
                upgrade_representative(active, shadow, rep_pid, pid, &game);
            } else {
                shadow.insert(pid, game.game_id);
            }
            continue;
        }
        if opened.contains(&game.game_id) {
            shadow.insert(pid, game.game_id);
            continue;
        }
        changed = true;
        opened.insert(game.game_id.clone());
        active.insert(pid, game.clone());
        events.push(GameEvent::Started(game));
    }

    (events, changed)
}

/// Close sessions only when every pid for a `game_id` has exited. A surviving
/// sibling promotes in place without a stop/start pair.
fn coalesce_stops(
    active: &mut HashMap<u32, ActiveGame>,
    shadow: &mut HashMap<u32, String>,
    live: &HashMap<u32, ActiveGame>,
    now: i64,
    primary: &mut Option<u32>,
) -> (Vec<GameEvent>, bool) {
    let mut events = Vec::new();
    let mut changed = false;

    shadow.retain(|pid, _| live.contains_key(pid));

    let gone_reps: Vec<u32> = active
        .keys()
        .copied()
        .filter(|pid| !live.contains_key(pid))
        .collect();
    for rep_pid in gone_reps {
        let Some(session) = active.remove(&rep_pid) else {
            continue;
        };
        let game_id = session.game_id.clone();

        if let Some((new_pid, new_game)) = live
            .iter()
            .filter(|(_, g)| g.game_id == game_id)
            .min_by_key(|(_, g)| representative_rank(g))
        {
            let mut promoted = new_game.clone();
            promoted.foreground_ms = session.foreground_ms;
            promoted.started_at = session.started_at;
            promoted.started_at_ms = session.started_at_ms;
            active.insert(*new_pid, promoted);
            shadow.remove(new_pid);
            if *primary == Some(rep_pid) {
                *primary = Some(*new_pid);
            }
            continue;
        }

        changed = true;
        if *primary == Some(rep_pid) {
            *primary = active.keys().copied().next();
        }
        events.push(GameEvent::Stopped {
            game: Box::new(session),
            ended_at: now,
        });
    }

    (events, changed)
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
            exe_path: g.exe_path.clone(),
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

#[derive(Clone)]
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

/// The compiled-in Steam appid index, decoded once.
fn appid_index() -> Option<&'static crate::catalogue::AppIdIndex> {
    static INDEX: std::sync::OnceLock<Option<crate::catalogue::AppIdIndex>> =
        std::sync::OnceLock::new();
    INDEX
        .get_or_init(|| {
            let index = crate::catalogue::AppIdIndex::bundled();
            match &index {
                Some(i) => log::info!("[game-sensor] appid index loaded: {} entries", i.len()),
                None => log::error!("[game-sensor] bundled appid index failed to parse"),
            }
            index
        })
        .as_ref()
}

/// The compiled-in catalogue head, parsed once.
fn catalogue_head() -> Option<&'static crate::catalogue::Head> {
    static HEAD: std::sync::OnceLock<Option<crate::catalogue::Head>> = std::sync::OnceLock::new();
    HEAD.get_or_init(|| {
        let head = crate::catalogue::Head::bundled();
        match &head {
            Some(h) => log::info!(
                "[game-sensor] catalogue loaded: {} games, {} curated executables",
                h.len(),
                h.exe_count()
            ),
            None => log::error!("[game-sensor] bundled catalogue failed to parse"),
        }
        head
    })
    .as_ref()
}

/// What a process resolved to, and how.
struct Matched {
    game_id: String,
    game_name: String,
    short_name: String,
    color: String,
    igdb_id: Option<u32>,
}

/// Files that mark a directory as a game build rather than an application.
///
/// Checked in the executable's own directory: Unity drops `UnityPlayer.dll`
/// and `GameAssembly.dll` beside the player, Unreal ships
/// `<Name>-Win64-Shipping.exe`, Godot writes a `.pck`. These are what let an
/// unrecognised process be tracked at all — without a positive signal the
/// alternative is either tracking every focused window or tracking nothing.
const ENGINE_MARKER_FILES: &[&str] = &[
    "unityplayer.dll",
    "gameassembly.dll",
    "mono-2.0-bdwgc.dll",
    "steam_api64.dll",
    "steam_api.dll",
    "galaxy64.dll",
    "d3d12core.dll",
];

/// Suffixes marking a binary that ships *with* a game but is not the game.
///
/// The engine-marker check is directory-scoped, so every executable beside a
/// Unity or Unreal build inherits that build's signature. Running against a
/// real Hearthstone install, that made `Hearthstone Beta Launcher.exe` its own
/// tracked game sitting next to the real one.
///
/// Matched as suffixes of the stem rather than substrings, so a game whose
/// name merely contains one of these words is unaffected — `agent47.exe` ends
/// in "47", not "agent".
const AUXILIARY_SUFFIXES: &[&str] = &[
    "launcher",
    "updater",
    "update",
    "patcher",
    "setup",
    "installer",
    "uninstall",
    "crashhandler",
    "crashreporter",
    "crashpad",
    "errorreporter",
    "helper",
    "service",
    "services",
    "daemon",
    "server",
    "config",
    "settings",
    "benchmark",
];

/// Is this a launcher, updater or similar companion rather than the game?
fn is_auxiliary_binary(exe_lower: &str) -> bool {
    let stem = exe_lower.trim_end_matches(".exe");
    // Normalise separators so "crash_handler" and "Crash Handler" both match.
    let mut squashed: String = stem.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    // Architecture suffixes hide the real ending: "LeagueCrashHandler64"
    // does not end with "crashhandler" until the 64 comes off.
    let had_digits = squashed.ends_with(|c: char| c.is_ascii_digit());
    while squashed.ends_with(|c: char| c.is_ascii_digit()) {
        squashed.pop();
    }
    if had_digits && squashed.ends_with('x') {
        squashed.pop();
    }
    AUXILIARY_SUFFIXES
        .iter()
        .any(|suffix| squashed.ends_with(suffix))
}

/// True when the process looks like a game build we have never heard of.
///
/// This gates **provisional tracking**, not just prompting. An earlier draft
/// of the plan had it gate prompting only, which would have recorded every
/// focused window as a session — "played Notepad for 4 hours" is worse than
/// missing an obscure game, and the confirm prompt still catches what this
/// misses.
fn looks_like_a_game(p: &RawGameProcess) -> bool {
    if p.path.is_empty() || p.window_title.trim().is_empty() {
        return false;
    }
    let exe = p.exe.to_ascii_lowercase();
    let path = p.path.to_ascii_lowercase();
    if UNKNOWN_DENYLIST.contains(&exe.as_str())
        || UNKNOWN_PATH_DENYLIST.iter().any(|d| path.contains(d))
    {
        return false;
    }
    if is_auxiliary_binary(&exe) {
        return false;
    }
    // Unreal's shipping binaries are self-describing.
    if exe.ends_with("-win64-shipping.exe") || exe.ends_with("-shipping.exe") {
        return true;
    }
    // Otherwise the process must be fullscreen, or sit next to engine files.
    p.is_fullscreen || has_engine_marker(std::path::Path::new(&p.path))
}

/// Does the executable's directory contain engine or platform runtime files?
///
/// Results are cached per directory: the check runs only for unresolved
/// processes (normally none), but a game sitting unresolved would otherwise
/// re-read the directory on every scan for as long as it runs.
fn has_engine_marker(exe_path: &std::path::Path) -> bool {
    use std::sync::Mutex;
    static CACHE: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);

    let Some(dir) = exe_path.parent() else {
        return false;
    };
    let key = dir.to_string_lossy().to_ascii_lowercase();
    if let Ok(mut guard) = CACHE.lock() {
        if let Some(hit) = guard.get_or_insert_with(HashMap::new).get(&key) {
            return *hit;
        }
    }

    let mut found = false;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            // Unity also ships a `<ExeStem>_Data` directory beside the player.
            if ENGINE_MARKER_FILES.contains(&name.as_str())
                || name.ends_with("_data")
                || name.ends_with(".pck")
            {
                found = true;
                break;
            }
        }
    }
    if let Ok(mut guard) = CACHE.lock() {
        guard.get_or_insert_with(HashMap::new).insert(key, found);
    }
    found
}

/// Tally an unresolved game once per executable per run.
///
/// These are the rows worth curating into `scripts/exe_mappings.json`, and
/// this is how we learn which ones actually matter rather than guessing. Once
/// per run because the sensor re-resolves the same process every scan.
fn note_unresolved(exe: &str, exe_path: &str, name: &str) {
    use std::sync::Mutex;
    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);

    let key = exe.to_lowercase();
    let first_time = match SEEN.lock() {
        Ok(mut guard) => guard.get_or_insert_with(HashSet::new).insert(key),
        Err(_) => false,
    };
    if !first_time {
        return;
    }
    log::info!("[game-sensor] unresolved game: {exe} ({name}) — tracked provisionally");
    crate::unresolved::record(exe, exe_path, name, now_ms());
}

/// Stable id for a game no rung could name.
///
/// Derived from the executable so two crew members running the same unknown
/// game agree on it, which is what lets their sessions aggregate. Namespaced
/// away from curated ids and from Steam appids so it can be upgraded later
/// without colliding.
fn provisional_game_id(exe: &str) -> String {
    let stem = exe
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(exe)
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE");
    let mut slug = String::with_capacity(stem.len());
    let mut last_dash = true;
    for c in stem.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            slug.push(c);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "local-game".to_string()
    } else {
        format!("local-{slug}")
    }
}

/// Best display name available without reading PE version metadata: the
/// window title, else the executable stem.
fn provisional_name(p: &RawGameProcess) -> String {
    let title = p.window_title.trim();
    if !title.is_empty() && title.len() <= 64 {
        return title.to_string();
    }
    let stem = p
        .exe
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE")
        .replace(['_', '-'], " ");
    if stem.is_empty() {
        return p.exe.clone();
    }
    // Title-case the stem so "night stones.exe" reads as a name.
    stem.split_whitespace()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The resolution ladder (plans/GAME-SENSING-V2.md §5.2), first hit wins.
///
/// 0. **Curated exe table** — launcher-independent, so it catches Hearthstone
///    whether Battle.net installed it to the default location or not.
/// 1. **Installed library** — the process path sits under a known install
///    directory. Exact, needs no per-game mapping, and covers the whole tail.
/// 2. **Legacy database** — the user's own confirmed custom games.
fn resolve_process(
    head: Option<&'static crate::catalogue::Head>,
    library: &LibraryIndex,
    p: &RawGameProcess,
) -> Option<Matched> {
    if let Some(entry) = head.and_then(|h| h.lookup_exe(&p.exe, &p.path)) {
        return Some(Matched {
            game_id: entry.game_id.to_string(),
            game_name: entry.name.to_string(),
            short_name: entry.short_name.to_string(),
            // Accent colours are not in the artifact: §8.2 made the exe's own
            // icon the primary asset, so the coloured badge is a last resort.
            color: "#888888".to_string(),
            igdb_id: Some(entry.igdb_id),
        });
    }
    if let Some(entry) = library.resolve(&p.path) {
        // A Steam appid maps to an IGDB id through the bundled index, which
        // upgrades a library-discovered game to full catalogue identity —
        // cover art, and the same id whichever launcher it came from.
        let igdb_id = (entry.source == crate::library::LibrarySource::Steam)
            .then(|| entry.external_id.parse::<u32>().ok())
            .flatten()
            .and_then(|appid| appid_index()?.igdb_id(appid));

        if let Some(catalogued) = igdb_id.and_then(|id| head.and_then(|h| h.get(id))) {
            return Some(Matched {
                game_id: catalogued.game_id.to_string(),
                game_name: catalogued.name.to_string(),
                short_name: catalogued.short_name.to_string(),
                color: "#888888".to_string(),
                igdb_id: Some(catalogued.igdb_id),
            });
        }

        // Epic and GOG have no id map, but the launcher still names the game,
        // and the catalogue can be searched by that name. Without this step one
        // game answers to two ids: Fortnite resolves as `fortnite` through the
        // curated exe table and as `epic-4fe75bbc…` through this scan, so its
        // hours split across two `user_game_stats` keys depending on which of
        // its processes was seen. The name is the only join available here.
        //
        // Only an unambiguous name is accepted. 17 names in the bundled head
        // are carried by two entries — a remake and its original share one, so
        // `prey`, `dead space`, `tomb raider` and `resident evil 2` all collide.
        // Taking either would file the session under the wrong game, which is
        // worse than the launcher id, and the launcher id is at least exact.
        let named = head.map(|h| h.all_by_name(&entry.name)).unwrap_or_default();
        if let [catalogued] = named.as_slice() {
            return Some(Matched {
                game_id: catalogued.game_id.to_string(),
                game_name: catalogued.name.to_string(),
                short_name: catalogued.short_name.to_string(),
                color: "#888888".to_string(),
                igdb_id: Some(catalogued.igdb_id),
            });
        }

        return Some(Matched {
            game_id: entry.game_id(),
            game_name: entry.name.clone(),
            short_name: entry.short_name(),
            color: "#888888".to_string(),
            igdb_id,
        });
    }
    // Rung 4: nothing named it, but it looks like a game build. Record the
    // session anyway — "ALL games they play we know about" is only true if an
    // unidentified game still produces a session instead of vanishing. The
    // identity is provisional and upgrades in place once a mapping lands or
    // the user confirms.
    if looks_like_a_game(p) {
        let name = provisional_name(p);
        note_unresolved(&p.exe, &p.path, &name);
        return Some(Matched {
            game_id: provisional_game_id(&p.exe),
            short_name: crate::library::derive_short_name(&name),
            game_name: name,
            color: "#888888".to_string(),
            igdb_id: None,
        });
    }
    None
}

/// Drop games whose process has presented no window for too long.
///
/// Resolution answers *which* game a process is. It does not answer whether the
/// game is being played, because the curated table and the library scan both
/// match on identity alone. A process that resolves but shows no window is a
/// tray resident or a companion binary, not a session.
///
/// `seen` carries the consecutive windowless count per pid across scans, and is
/// pruned here so it cannot grow with dead pids.
fn drop_windowless(
    detected: Vec<ActiveGame>,
    processes: &[RawGameProcess],
    seen: &mut HashMap<u32, u32>,
) -> Vec<ActiveGame> {
    let mut kept = Vec::with_capacity(detected.len());
    let mut live_pids: HashSet<u32> = HashSet::new();
    for game in detected {
        live_pids.insert(game.pid);
        let has_window = processes
            .iter()
            .find(|p| p.pid == game.pid)
            .is_some_and(|p| !p.window_title.trim().is_empty());
        if has_window {
            seen.remove(&game.pid);
            kept.push(game);
            continue;
        }
        let misses = seen.entry(game.pid).or_insert(0);
        *misses += 1;
        if *misses <= WINDOWLESS_GRACE_SCANS {
            kept.push(game);
        } else if *misses == WINDOWLESS_GRACE_SCANS + 1 {
            log::info!(
                "[game-sensor] {} (pid={}) has no window; ending its session",
                game.game_name,
                game.pid
            );
        }
    }
    seen.retain(|pid, _| live_pids.contains(pid));
    kept
}

/// Every running process the database recognises as a game.
///
/// v1 returned only one, so a second game running alongside was invisible and
/// alt-tabbing between two looked like the first had exited. `now` is passed in
/// rather than read here so a single scan timestamps consistently.
fn detect_games(library: &LibraryIndex, processes: &[RawGameProcess], now: i64) -> Vec<ActiveGame> {
    let head = catalogue_head();
    processes
        .iter()
        .filter_map(|p| {
            let matched = resolve_process(head, library, p)?;
            Some(ActiveGame {
                game_id: matched.game_id,
                game_name: matched.game_name,
                short_name: matched.short_name,
                color: matched.color,
                igdb_id: matched.igdb_id,
                exe: p.exe.clone(),
                exe_path: p.path.clone(),
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

    // --- Identity reconciliation ------------------------------------------

    /// A launcher-discovered game must answer to its catalogue id when the
    /// catalogue knows the name.
    ///
    /// Fortnite runs several processes. The game itself resolves through the
    /// curated exe table as `fortnite`. Its EasyAntiCheat companion sits under
    /// the Epic install directory and used to resolve as
    /// `epic-4fe75bbc5a674f4f9b356b5c90567da5`. One game answered to two ids,
    /// so its hours split across two `user_game_stats` keys.
    #[test]
    fn epic_library_game_resolves_to_its_catalogue_id() {
        let Some(head) = catalogue_head() else {
            return; // bundled catalogue unavailable in this build
        };
        let Some(expected) = head.by_name("Fortnite") else {
            return; // the head does not carry Fortnite; nothing to reconcile
        };

        let install_dir = std::path::PathBuf::from(r"D:\EpicGames\Fortnite");
        let library = LibraryIndex::from_entries(vec![crate::library::LibraryEntry {
            source: crate::library::LibrarySource::Epic,
            external_id: "4fe75bbc5a674f4f9b356b5c90567da5".into(),
            name: "Fortnite".into(),
            install_dir: install_dir.clone(),
        }]);

        let mut p = make_process("FortniteClient-Win64-Shipping_EAC_EOS.exe", 100, false);
        p.path = r"D:\EpicGames\Fortnite\FortniteGame\Binaries\Win64\FortniteClient-Win64-Shipping_EAC_EOS.exe".into();

        let m = resolve_process(Some(head), &library, &p)
            .expect("a process under an install dir resolves");
        assert_eq!(
            m.game_id, expected.game_id,
            "library-discovered Fortnite must use the catalogue id, not the Epic id"
        );
        assert!(
            !m.game_id.starts_with("epic-"),
            "got the launcher id {}, which splits stats from the curated id",
            m.game_id
        );
    }

    /// A name carried by two catalogue entries must not be guessed at.
    ///
    /// The bundled head holds 17 such names, each a remake and its original:
    /// `prey`, `dead space`, `tomb raider`, `resident evil 2` and more. Filing
    /// a session under the wrong one of a pair is worse than keeping the
    /// launcher id, which is at least exact.
    #[test]
    fn ambiguous_catalogue_name_keeps_the_launcher_id() {
        let Some(head) = catalogue_head() else {
            return;
        };
        let ambiguous = ["Prey", "Dead Space", "Tomb Raider", "Resident Evil 2"]
            .into_iter()
            .find(|n| head.all_by_name(n).len() > 1)
            .expect("the head carries at least one duplicated name");

        let library = LibraryIndex::from_entries(vec![crate::library::LibraryEntry {
            source: crate::library::LibrarySource::Epic,
            external_id: "dupe123".into(),
            name: ambiguous.to_string(),
            install_dir: std::path::PathBuf::from(r"D:\EpicGames\Ambiguous"),
        }]);
        let mut p = make_process("game.exe", 102, false);
        p.path = r"D:\EpicGames\Ambiguous\game.exe".into();

        let m = resolve_process(Some(head), &library, &p).expect("resolves through the library");
        assert_eq!(
            m.game_id, "epic-dupe123",
            "an ambiguous name ({ambiguous}) must not pick one of the pair"
        );
    }

    /// A game the catalogue has never heard of keeps the launcher id, which is
    /// still stable and still names the game.
    #[test]
    fn unknown_library_game_keeps_the_launcher_id() {
        let library = LibraryIndex::from_entries(vec![crate::library::LibraryEntry {
            source: crate::library::LibrarySource::Epic,
            external_id: "abc123".into(),
            name: "Some Unreleased Indie Thing".into(),
            install_dir: std::path::PathBuf::from(r"D:\EpicGames\Indie"),
        }]);
        let mut p = make_process("indie.exe", 101, false);
        p.path = r"D:\EpicGames\Indie\indie.exe".into();

        let m =
            resolve_process(catalogue_head(), &library, &p).expect("resolves through the library");
        assert_eq!(m.game_id, "epic-abc123");
        assert_eq!(m.game_name, "Some Unreleased Indie Thing");
    }

    const NOW: i64 = 1_700_000_000_000;

    fn active_map(games: Vec<ActiveGame>) -> HashMap<u32, ActiveGame> {
        games.into_iter().map(|g| (g.pid, g)).collect()
    }

    #[test]
    fn detect_no_processes() {
        assert!(detect_games(&LibraryIndex::default(), &[], NOW).is_empty());
    }

    #[test]
    fn detect_single_match() {
        let procs = vec![make_process("cs2.exe", 1234, false)];
        let found = detect_games(&LibraryIndex::default(), &procs, NOW);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].game_id, "counter-strike-2");
    }

    #[test]
    fn detect_unknown_exe_filtered() {
        let procs = vec![make_process("notepad.exe", 999, false)];
        assert!(detect_games(&LibraryIndex::default(), &procs, NOW).is_empty());
    }

    #[test]
    fn detect_returns_every_running_game() {
        // v1 returned Option and lost the second game entirely; both sessions
        // must now be tracked.
        let procs = vec![
            make_process("cs2.exe", 1234, false),
            make_process("dota2.exe", 5678, true),
        ];
        let found = detect_games(&LibraryIndex::default(), &procs, NOW);
        assert_eq!(found.len(), 2);
        let mut ids: Vec<&str> = found.iter().map(|g| g.game_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["counter-strike-2", "dota-2"]);
    }

    #[test]
    fn detect_uses_real_process_start_time() {
        let mut proc = make_process("cs2.exe", 1234, false);
        proc.started_at_ms = NOW - 3 * 3_600_000; // running for three hours
        let found = detect_games(&LibraryIndex::default(), &[proc], NOW);
        // The session must date from when the game started, not from the scan
        // that first noticed it — this is the "played 4hrs" honesty fix.
        assert_eq!(found[0].started_at, NOW - 3 * 3_600_000);
        assert_eq!(found[0].started_at_ms, NOW - 3 * 3_600_000);
    }

    #[test]
    fn detect_falls_back_to_scan_time_when_start_unknown() {
        let proc = make_process("cs2.exe", 1234, false); // started_at_ms = 0
        let found = detect_games(&LibraryIndex::default(), &[proc], NOW);
        // Never 0 — that would date the session to 1970 and report a 55-year
        // session. The scan time is the honest floor.
        assert_eq!(found[0].started_at, NOW);
        assert_eq!(found[0].started_at_ms, 0);
    }

    #[test]
    fn primary_prefers_foreground_over_fullscreen() {
        let mut fullscreen = make_process("dota2.exe", 5678, true);
        fullscreen.started_at_ms = NOW - 1000;
        let mut focused = make_process("cs2.exe", 1234, false);
        focused.is_foreground = true;
        focused.started_at_ms = NOW - 500;
        let procs = vec![fullscreen, focused];
        let active = active_map(detect_games(&LibraryIndex::default(), &procs, NOW));
        // v1 sorted on is_fullscreen only and ignored is_foreground entirely,
        // so the game you were actually looking at lost to a backgrounded one.
        assert_eq!(pick_primary(&active, &procs), Some(1234));
    }

    #[test]
    fn primary_falls_back_to_fullscreen_then_oldest() {
        let procs = vec![
            make_process("cs2.exe", 1234, false),
            make_process("dota2.exe", 5678, true),
        ];
        let active = active_map(detect_games(&LibraryIndex::default(), &procs, NOW));
        assert_eq!(pick_primary(&active, &procs), Some(5678));

        // Neither focused nor fullscreen: the longest-running wins, and the
        // answer is stable across calls rather than hash-order dependent.
        let plain = vec![
            make_process("cs2.exe", 1234, false),
            make_process("dota2.exe", 5678, false),
        ];
        let mut games = detect_games(&LibraryIndex::default(), &plain, NOW);
        games[0].started_at = NOW - 10_000;
        games[1].started_at = NOW - 90_000;
        let active = active_map(games);
        assert_eq!(pick_primary(&active, &plain), Some(5678));
        assert_eq!(pick_primary(&active, &plain), Some(5678));
    }

    #[test]
    fn library_resolves_games_the_curated_table_never_heard_of() {
        // The tail mechanism: no exe mapping exists for this game anywhere,
        // yet it resolves — with a real name — purely from the install path.
        let library = LibraryIndex::from_entries(vec![crate::library::LibraryEntry {
            source: crate::library::LibrarySource::Steam,
            external_id: "1145360".to_string(),
            name: "Hades".to_string(),
            install_dir: std::path::PathBuf::from(r"D:\SteamLibrary\steamapps\common\Hades"),
        }]);
        let mut p = make_process("Hades.exe", 4242, true);
        p.path = r"D:\SteamLibrary\steamapps\common\Hades\x64\Hades.exe".into();

        // Without the library index the game is still recorded — the
        // provisional rung guarantees a session — but only under a guessed id
        // and whatever the window happened to be called.
        let guessed = detect_games(&LibraryIndex::default(), &[p.clone()], NOW);
        assert_eq!(guessed.len(), 1);
        assert_eq!(guessed[0].game_id, "local-hades");

        // With the library index the Steam appid is known, and the bundled
        // appid index maps it onto IGDB — so the game arrives with full
        // catalogue identity rather than a launcher-scoped id.
        let found = detect_games(&library, &[p], NOW);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].game_name, "Hades");
        assert_eq!(
            found[0].igdb_id,
            Some(113112),
            "a Steam appid must resolve to its IGDB id through the bundled index"
        );
        assert!(
            !found[0].game_id.starts_with("steam-"),
            "a catalogued game keeps its catalogue id, not the launcher's: {}",
            found[0].game_id
        );
    }

    #[test]
    fn curated_mapping_outranks_the_library() {
        // Both rungs can claim the same process. The curated entry wins because
        // it carries the stable game_id that telemetry and stored stats key on;
        // resolving CS2 as "steam-730" would orphan them.
        let library = LibraryIndex::from_entries(vec![crate::library::LibraryEntry {
            source: crate::library::LibrarySource::Steam,
            external_id: "730".to_string(),
            name: "Counter-Strike 2".to_string(),
            install_dir: std::path::PathBuf::from(
                r"C:\Steam\steamapps\common\Counter-Strike Global Offensive",
            ),
        }]);
        let mut p = make_process("cs2.exe", 1234, true);
        p.path =
            r"C:\Steam\steamapps\common\Counter-Strike Global Offensive\game\bin\cs2.exe".into();

        let found = detect_games(&library, &[p], NOW);
        assert_eq!(found[0].game_id, "counter-strike-2");
        assert_eq!(found[0].igdb_id, Some(242408));
    }

    /// Live check against whatever Steam is actually installed on this machine.
    /// Ignored by default — it asserts nothing about a specific game because
    /// CI has no Steam, but run locally it is the fastest way to confirm the
    /// manifest parsing works against real files:
    ///   cargo test -p mello-core --lib scan_real_steam_library -- --ignored --nocapture
    /// Resolution report for whatever is actually installed on this machine.
    ///
    /// Ignored by default (CI has no launchers). Run it after installing a
    /// game to check the ladder resolves it — the curated exe names were
    /// written from knowledge, and a wrong one fails *silently*: the game
    /// simply never resolves, with no error anywhere.
    ///
    ///   cargo test -p mello-core --lib resolution_report -- --ignored --nocapture
    #[test]
    #[ignore = "requires real game installs"]
    fn resolution_report() {
        let library = LibraryIndex::scan();
        let head = catalogue_head().expect("bundled catalogue");

        // Launcher roots worth probing. Steam is covered by the library scan;
        // these are the ones curated mappings have to carry alone.
        let roots = [
            r"C:\Program Files (x86)\Hearthstone",
            r"C:\Program Files (x86)\World of Warcraft",
            r"C:\Program Files (x86)\Overwatch",
            r"C:\Program Files (x86)\Diablo IV",
            r"C:\Program Files (x86)\StarCraft II",
            r"C:\Riot Games",
            r"C:\Program Files\Epic Games",
            r"C:\Program Files\Rockstar Games",
        ];

        println!("\n--- installed libraries ---");
        for e in library.iter() {
            println!("  {:<28} {}", e.game_id(), e.name);
        }

        println!("\n--- executables under known launcher roots ---");
        let mut probed = 0;
        for root in roots {
            let path = std::path::Path::new(root);
            if !path.exists() {
                continue;
            }
            for exe in find_executables(path, 4) {
                let name = exe
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let full = exe.to_string_lossy().to_string();
                let p = RawGameProcess {
                    pid: 0,
                    name: name.clone(),
                    exe: name.clone(),
                    is_fullscreen: false,
                    path: full.clone(),
                    window_title: name.clone(),
                    is_foreground: false,
                    started_at_ms: 0,
                };
                if let Some(m) = resolve_process(Some(head), &library, &p) {
                    probed += 1;
                    println!("  {:<38} -> {} ({})", name, m.game_id, m.game_name);
                }
            }
        }
        println!("\n{probed} executable(s) resolved under launcher roots");
    }

    /// Shallow recursive executable search, bounded so a deep game install
    /// cannot turn a diagnostic into a full-disk walk.
    fn find_executables(dir: &std::path::Path, depth: usize) -> Vec<std::path::PathBuf> {
        if depth == 0 {
            return Vec::new();
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(find_executables(&path, depth - 1));
            } else if path.extension().and_then(|e| e.to_str()) == Some("exe") {
                out.push(path);
            }
        }
        out
    }

    #[test]
    #[ignore = "requires a real Steam install"]
    fn scan_real_steam_library() {
        let index = LibraryIndex::scan();
        println!("installed games found: {}", index.len());
        for e in index.iter() {
            println!(
                "  {:<10} {:<38} {:<10} {}",
                e.game_id(),
                e.name,
                e.short_name(),
                e.install_dir.display()
            );
        }
        assert!(
            !index.is_empty(),
            "no games found — is Steam installed with at least one game?"
        );
    }

    // ------------------------------------------------------------------
    // Classifier + provisional tracking for unrecognised games
    // ------------------------------------------------------------------

    /// A fullscreen, windowed process from a plausible game install.
    fn game_like(exe: &str) -> RawGameProcess {
        let mut p = make_process(exe, 7777, true);
        p.path = format!("D:\\Games\\Indie\\{exe}");
        p.window_title = exe.trim_end_matches(".exe").to_string();
        p
    }

    #[test]
    fn unrecognised_fullscreen_game_is_tracked_provisionally() {
        // The point of "ALL games they play we know about": a game no rung can
        // name still produces a session instead of vanishing.
        let found = detect_games(
            &LibraryIndex::default(),
            &[game_like("night stones.exe")],
            NOW,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].game_id, "local-night-stones");
        assert_eq!(found[0].game_name, "night stones");
    }

    #[test]
    fn provisional_ids_agree_across_users() {
        // Two crew members running the same unknown game must land on the same
        // id, or their sessions can never aggregate.
        assert_eq!(
            provisional_game_id("Night Stones.exe"),
            provisional_game_id("night stones.exe")
        );
        assert_eq!(
            provisional_game_id("Night Stones.exe"),
            "local-night-stones"
        );
        assert_eq!(provisional_game_id("....exe"), "local-game");
    }

    #[test]
    fn ordinary_desktop_apps_are_not_tracked() {
        // The plan originally had the classifier gate prompting only, which
        // would have filed "played Notepad for 4 hours" as a session.
        let mut notepad = make_process("notepad.exe", 10, false);
        notepad.path = "C:\\Windows\\System32\\notepad.exe".into();
        notepad.window_title = "Untitled - Notepad".into();
        notepad.is_foreground = true;

        let found = detect_games(&LibraryIndex::default(), &[notepad], NOW);
        assert!(
            found.is_empty(),
            "a focused text editor is not a game session"
        );
    }

    #[test]
    fn denylisted_and_windowless_processes_never_qualify() {
        let mut browser = game_like("chrome.exe");
        browser.path = "C:\\Program Files\\Google\\Chrome\\chrome.exe".into();
        assert!(!looks_like_a_game(&browser));

        let mut electron = game_like("someapp.exe");
        electron.path = "C:\\Users\\bob\\AppData\\Local\\Programs\\App\\someapp.exe".into();
        assert!(!looks_like_a_game(&electron));

        let mut headless = game_like("service.exe");
        headless.window_title = String::new();
        assert!(!looks_like_a_game(&headless));

        let mut pathless = game_like("mystery.exe");
        pathless.path = String::new();
        assert!(!looks_like_a_game(&pathless));
    }

    #[test]
    fn launchers_beside_a_game_are_not_themselves_games() {
        // Found against a real Hearthstone install: the engine-marker check is
        // directory-scoped, so every companion binary inherits the game's Unity
        // signature. Without this guard, "Hearthstone Beta Launcher.exe" became
        // its own tracked game alongside the real one.
        for exe in [
            "Hearthstone Beta Launcher.exe",
            "Battle.net Launcher.exe",
            "EpicGamesLauncher.exe",
            "GameUpdater.exe",
            "LeagueCrashHandler64.exe",
            "RiotClientServices.exe",
            "UnityCrashHandler64.exe",
            "vc_redist_setup.exe",
            "DedicatedServer.exe",
            "crash_handler.exe",
        ] {
            let mut p = game_like(exe);
            p.is_fullscreen = true; // even fullscreen must not save it
            assert!(
                !looks_like_a_game(&p),
                "{exe} is a companion binary, not a game"
            );
        }
    }

    #[test]
    fn a_game_whose_name_contains_an_auxiliary_word_still_qualifies() {
        // Suffix matching, not substring: these must survive the guard.
        for exe in [
            "agent47.exe",        // contains "agent"
            "serverfarm.exe",     // starts with "server"
            "configurator5.exe",  // contains "config"
            "TheUpdaterGame.exe", // contains "updater" mid-name
        ] {
            let p = game_like(exe);
            assert!(looks_like_a_game(&p), "{exe} should still be a candidate");
        }
    }

    #[test]
    fn unreal_shipping_binaries_qualify_without_being_fullscreen() {
        // Unreal names its shipping binary distinctively, so a windowed UE
        // game is recognisable with no filesystem probe at all.
        let mut windowed = make_process("SomeIndie-Win64-Shipping.exe", 99, false);
        windowed.path =
            "D:\\Games\\SomeIndie\\Binaries\\Win64\\SomeIndie-Win64-Shipping.exe".into();
        windowed.window_title = "Some Indie".into();
        assert!(looks_like_a_game(&windowed));
    }

    #[test]
    fn provisional_name_prefers_the_window_title() {
        let mut p = game_like("ns_shipping.exe");
        p.window_title = "Night Stones".into();
        assert_eq!(provisional_name(&p), "Night Stones");

        // No usable title: prettify the executable stem instead of showing
        // "ns_shipping.exe" in the crew feed.
        p.window_title = "   ".into();
        assert_eq!(provisional_name(&p), "Ns Shipping");
    }

    #[test]
    fn named_rungs_outrank_provisional_tracking() {
        // A curated game that happens to be fullscreen must keep its stable id
        // rather than falling through to "local-cs2".
        let mut cs = make_process("cs2.exe", 1, true);
        cs.path = "C:\\Steam\\steamapps\\common\\CSGO\\cs2.exe".into();
        cs.window_title = "Counter-Strike 2".into();
        let found = detect_games(&LibraryIndex::default(), &[cs], NOW);
        assert_eq!(found[0].game_id, "counter-strike-2");
    }

    // --- Windowless resolved processes -----------------------------------

    fn windowed(exe: &str, pid: u32, title: &str) -> RawGameProcess {
        let mut p = make_process(exe, pid, false);
        p.window_title = title.to_string();
        p
    }

    fn game_named(pid: u32, id: &str) -> ActiveGame {
        ActiveGame {
            game_id: id.into(),
            game_name: id.into(),
            short_name: "G".into(),
            color: "#888888".into(),
            exe: format!("{id}.exe"),
            exe_path: format!(r"C:\Games\{id}.exe"),
            pid,
            igdb_id: None,
            started_at: NOW,
            started_at_ms: NOW,
            foreground_ms: 0,
        }
    }

    /// Roblox drops to the tray on quit. `RobloxPlayerBeta.exe` stays alive
    /// with no window and the curated exe table still resolves it, so the
    /// session ran forever.
    #[test]
    fn a_windowless_game_is_dropped_after_the_grace_period() {
        let mut seen = HashMap::new();
        let proc = make_process("robloxplayerbeta.exe", 10, false);
        let proc = RawGameProcess {
            window_title: String::new(),
            ..proc
        };
        // Grace scans keep it, so one bad read cannot split a session.
        for scan in 1..=WINDOWLESS_GRACE_SCANS {
            let kept = drop_windowless(
                vec![game_named(10, "roblox")],
                std::slice::from_ref(&proc),
                &mut seen,
            );
            assert_eq!(
                kept.len(),
                1,
                "dropped on scan {scan}, inside the grace period"
            );
        }
        let kept = drop_windowless(vec![game_named(10, "roblox")], &[proc], &mut seen);
        assert!(
            kept.is_empty(),
            "a tray resident must not hold a session open"
        );
    }

    #[test]
    fn a_windowed_game_is_always_kept_and_resets_the_count() {
        let mut seen = HashMap::new();
        let blind = RawGameProcess {
            window_title: String::new(),
            ..make_process("game.exe", 11, false)
        };
        drop_windowless(vec![game_named(11, "g")], &[blind], &mut seen);
        assert_eq!(seen.get(&11), Some(&1), "a miss is counted");

        let seeing = windowed("game.exe", 11, "The Game");
        let kept = drop_windowless(vec![game_named(11, "g")], &[seeing], &mut seen);
        assert_eq!(kept.len(), 1);
        assert!(!seen.contains_key(&11), "a window resets the count");
    }

    #[test]
    fn windowless_counts_do_not_accumulate_for_dead_pids() {
        let mut seen = HashMap::new();
        let blind = RawGameProcess {
            window_title: String::new(),
            ..make_process("game.exe", 12, false)
        };
        drop_windowless(vec![game_named(12, "g")], &[blind], &mut seen);
        assert_eq!(seen.len(), 1);
        // The game exits entirely; nothing is detected for that pid any more.
        drop_windowless(Vec::new(), &[], &mut seen);
        assert!(seen.is_empty(), "counts must be pruned with the process");
    }

    #[test]
    fn primary_is_none_without_games() {
        assert_eq!(pick_primary(&HashMap::new(), &[]), None);
    }
}
