use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::game_db::GameDatabase;

const GAME_SCAN_INTERVAL: Duration = Duration::from_secs(5);
const MAX_PROCESSES: usize = 512;

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
    /// for game events.
    pub fn start(
        ctx: *mut mello_sys::MelloContext,
        db: GameDatabase,
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

fn scan_loop(ctx: &SendCtx, db: &GameDatabase, tx: &Sender<GameEvent>) {
    let mut previous: Option<ActiveGame> = None;
    log::info!(
        "[game-sensor] scan loop started (interval={:?})",
        GAME_SCAN_INTERVAL
    );

    loop {
        std::thread::sleep(GAME_SCAN_INTERVAL);

        let detected = scan_once(ctx.0, db);

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

fn scan_once(ctx: *mut mello_sys::MelloContext, db: &GameDatabase) -> Option<ActiveGame> {
    let processes = enumerate_game_processes(ctx);
    pick_primary_game(db, &processes)
}

struct RawGameProcess {
    pid: u32,
    #[allow(dead_code)]
    name: String,
    exe: String,
    is_fullscreen: bool,
}

fn enumerate_game_processes(ctx: *mut mello_sys::MelloContext) -> Vec<RawGameProcess> {
    let mut buf = vec![
        mello_sys::MelloGameProcess {
            pid: 0,
            name: [0i8; 128],
            exe: [0i8; 260],
            is_fullscreen: false,
        };
        MAX_PROCESSES
    ];

    let count = unsafe {
        mello_sys::mello_enumerate_games(ctx, buf.as_mut_ptr(), MAX_PROCESSES as i32)
    };

    let mut out = Vec::new();
    for gp in buf.iter().take(count.max(0) as usize) {
        let name = unsafe { std::ffi::CStr::from_ptr(gp.name.as_ptr()) }
            .to_string_lossy()
            .to_string();
        let exe = unsafe { std::ffi::CStr::from_ptr(gp.exe.as_ptr()) }
            .to_string_lossy()
            .to_string();
        out.push(RawGameProcess {
            pid: gp.pid,
            name,
            exe,
            is_fullscreen: gp.is_fullscreen,
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
}
