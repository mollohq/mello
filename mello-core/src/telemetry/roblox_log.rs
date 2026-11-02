//! Roblox player log adapter (log tailer), spec 18 §3.11.
//!
//! The Roblox player is one process and one window for three states: the
//! tray-resident launcher, the home screen, and an experience being played.
//! The window cannot tell them apart, so a window-based session would run
//! through all three. The player's own log can: it writes a join line with
//! the place id when an experience starts and a disconnect line when it
//! ends. This adapter gates the Roblox session on those lines (spec 18
//! §2.4): the session opens at the join and closes at the leave while the
//! process and its window stay up.
//!
//! Nothing is installed. Roblox writes the log on its own, one file per
//! player process, under `%LOCALAPPDATA%\Roblox\logs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use super::log_tail::{spawn_tail_from, Start};
use super::{GameTelemetryAdapter, TelemetryError, TelemetryEvent};

const GAME_ID: &str = "roblox";
/// `[FLog::Output] ! Joining game '<uuid>' place 107778070777162 at 10.34.5.217`
const JOIN_MARKER: &str = "! Joining game '";
const PLACE_MARKER: &str = " place ";
/// The experience ends with both of these, a second apart. The first one
/// closes the session; the second finds nothing open.
const LEAVE_MARKERS: &[&str] = &[
    "Disconnected from server for reason",
    "[DFLog::NetworkClient] Client:Disconnect",
];

/// What one log line says about the experience.
#[derive(Debug, PartialEq, Eq)]
enum Mark {
    Joined { place_id: String, ts: i64 },
    Left { ts: i64 },
}

#[derive(Default)]
struct RobloxState {
    /// Place id of the experience the player is in, and when it was joined.
    in_experience: Option<(String, i64)>,
    /// True until the existing log has been read. History folds into the
    /// state without events; the current state is announced once at the end.
    /// Replaying the events instead would open and close sessions that
    /// happened while the client was not running.
    replaying: bool,
}

pub struct RobloxLogAdapter {
    running: Arc<AtomicBool>,
    state: Arc<Mutex<RobloxState>>,
}

impl RobloxLogAdapter {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(RobloxState::default())),
        }
    }
}

impl Default for RobloxLogAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GameTelemetryAdapter for RobloxLogAdapter {
    fn game_id(&self) -> &str {
        GAME_ID
    }

    fn info(&self) -> super::AdapterInfo {
        super::AdapterInfo {
            game_name: "Roblox",
            writes_files: false,
            note: "Reads the Roblox player's own log to tell an experience from the home screen and the tray. Nothing is installed.",
            account_link: None,
        }
    }

    fn gates_session(&self) -> bool {
        true
    }

    fn detect_install(&self) -> Option<bool> {
        #[cfg(windows)]
        {
            Some(roblox_dir().is_some_and(|d| d.join("Versions").is_dir()))
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    fn ensure_installed(&self, _token: &str, _port: u16) -> Result<(), TelemetryError> {
        Ok(())
    }

    fn start(&self, tx: Sender<TelemetryEvent>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.state.lock().expect("roblox state poisoned") = RobloxState {
            in_experience: None,
            replaying: true,
        };

        let state = self.state.clone();
        let line_tx = tx.clone();
        let caught_up_state = self.state.clone();
        spawn_tail_from(
            "roblox-player-log",
            self.running.clone(),
            Start::Beginning,
            player_log_path,
            move |line| {
                let mut s = state.lock().expect("roblox state poisoned");
                fold_line(&mut s, line, &line_tx);
            },
            move || {
                let mut s = caught_up_state.lock().expect("roblox state poisoned");
                s.replaying = false;
                match &s.in_experience {
                    Some((place_id, ts)) => {
                        log::info!(
                            "[telemetry] roblox log caught up: in experience {place_id} since {ts}"
                        );
                        let _ = tx.send(TelemetryEvent::ActivityStarted {
                            game_id: GAME_ID.to_string(),
                            activity_id: place_id.clone(),
                            ts: *ts,
                        });
                    }
                    None => log::info!("[telemetry] roblox log caught up: not in an experience"),
                }
            },
        );
        log::info!("[telemetry] roblox player log tailer started");
    }

    fn reset(&self) {
        self.running.store(false, Ordering::SeqCst);
        *self.state.lock().expect("roblox state poisoned") = RobloxState::default();
    }
}

/// Fold one log line into the state and, once history is read, announce
/// the change. A join while already in an experience is a teleport: the
/// session stays open and the activity id moves.
fn fold_line(state: &mut RobloxState, line: &str, tx: &Sender<TelemetryEvent>) {
    match parse_line(line, now_ms()) {
        Some(Mark::Joined { place_id, ts }) => {
            state.in_experience = Some((place_id.clone(), ts));
            if !state.replaying {
                log::info!("[telemetry] roblox: joined experience {place_id}");
                let _ = tx.send(TelemetryEvent::ActivityStarted {
                    game_id: GAME_ID.to_string(),
                    activity_id: place_id,
                    ts,
                });
            }
        }
        Some(Mark::Left { ts }) => {
            if state.in_experience.take().is_some() && !state.replaying {
                log::info!("[telemetry] roblox: left the experience");
                let _ = tx.send(TelemetryEvent::ActivityEnded {
                    game_id: GAME_ID.to_string(),
                    ts,
                });
            }
        }
        None => {}
    }
}

/// Read the join and leave markers. `fallback_ts` stands in when the line
/// carries no parsable timestamp.
fn parse_line(line: &str, fallback_ts: i64) -> Option<Mark> {
    if let Some(rest) = line.split(JOIN_MARKER).nth(1) {
        let place_id: String = rest
            .split(PLACE_MARKER)
            .nth(1)?
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if place_id.is_empty() {
            return None;
        }
        return Some(Mark::Joined {
            place_id,
            ts: line_ts(line).unwrap_or(fallback_ts),
        });
    }
    if LEAVE_MARKERS.iter().any(|m| line.contains(m)) {
        return Some(Mark::Left {
            ts: line_ts(line).unwrap_or(fallback_ts),
        });
    }
    None
}

/// Each line starts with an RFC 3339 timestamp and a comma:
/// `2026-11-02T14:11:01.704Z,1263.704224,8204,6 [FLog::Output] ...`.
fn line_ts(line: &str) -> Option<i64> {
    let stamp = line.split(',').next()?;
    chrono::DateTime::parse_from_rfc3339(stamp)
        .ok()
        .map(|t| t.timestamp_millis())
}

#[cfg(windows)]
fn roblox_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| std::path::PathBuf::from(base).join("Roblox"))
}

/// The newest player log. Roblox opens one per player process
/// (`<version>_<time>_Player_<id>_last.log`); the crash handler writes its
/// own beside it.
#[cfg(windows)]
fn player_log_path() -> Option<std::path::PathBuf> {
    newest_player_log(&roblox_dir()?.join("logs"))
}

#[cfg(not(windows))]
fn player_log_path() -> Option<std::path::PathBuf> {
    None
}

// The scan runs on Windows; the test exercises it on every platform.
#[cfg_attr(not(windows), allow(dead_code))]
fn newest_player_log(logs: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(logs).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.contains("_Player_") || name.contains("CrashHandler") || !name.ends_with(".log") {
            continue;
        }
        let Some(modified) = entry.metadata().ok().and_then(|m| m.modified().ok()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
            newest = Some((modified, entry.path()));
        }
    }
    newest.map(|(_, p)| p)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const JOIN: &str = "2026-11-02T14:11:01.704Z,1263.704224,8204,6 [FLog::Output] ! Joining game 'a8c4b9d2-0000-4000-8000-000000000000' place 107778070777162 at 10.34.5.217";
    const LEAVE: &str = "2026-11-02T14:15:18.129Z,1520.129761,5e58,6,Info [DFLog::RbxTransportDummyClient] Disconnected from server for reason: Player: 285 (DisconnectClientInitiated)";
    const LEAVE_2: &str =
        "2026-11-02T14:15:19.662Z,1521.662598,5e58,6,Info [DFLog::NetworkClient] Client:Disconnect";
    const JOIN_MS: i64 = 1_793_628_661_704;
    const LEAVE_MS: i64 = 1_793_628_918_129;

    #[test]
    fn join_line_carries_the_place_and_the_time() {
        assert_eq!(
            parse_line(JOIN, 0),
            Some(Mark::Joined {
                place_id: "107778070777162".into(),
                ts: JOIN_MS
            })
        );
    }

    #[test]
    fn both_leave_lines_mark_the_end() {
        assert_eq!(parse_line(LEAVE, 0), Some(Mark::Left { ts: LEAVE_MS }));
        assert!(matches!(parse_line(LEAVE_2, 0), Some(Mark::Left { .. })));
    }

    #[test]
    fn other_lines_say_nothing() {
        // The SignalR line at startup says "Disconnected" too; it is not a leave.
        let signalr = "2026-11-02T13:50:10.405Z,12.405411,6a28,6 [DFLog::SignalRCoreError] ID: 1 Disconnected - stop() called";
        assert_eq!(parse_line(signalr, 0), None);
        let load = "2026-11-02T14:11:01.704Z,1263.704224,8204,6 [FLog::GameJoinLoadTime] Report game_join_loadtime: placeid:107778070777162";
        assert_eq!(parse_line(load, 0), None);
        assert_eq!(parse_line("", 0), None);
    }

    #[test]
    fn a_line_without_a_timestamp_takes_the_fallback() {
        let bare = "[FLog::Output] ! Joining game 'x' place 42 at 1.2.3.4";
        assert_eq!(
            parse_line(bare, 77),
            Some(Mark::Joined {
                place_id: "42".into(),
                ts: 77
            })
        );
    }

    fn live_state() -> RobloxState {
        RobloxState {
            in_experience: None,
            replaying: false,
        }
    }

    #[test]
    fn a_live_join_then_leave_opens_and_closes_the_activity() {
        let (tx, rx) = mpsc::channel();
        let mut s = live_state();
        fold_line(&mut s, JOIN, &tx);
        fold_line(&mut s, LEAVE, &tx);
        fold_line(&mut s, LEAVE_2, &tx);
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            events,
            vec![
                TelemetryEvent::ActivityStarted {
                    game_id: "roblox".into(),
                    activity_id: "107778070777162".into(),
                    ts: JOIN_MS,
                },
                TelemetryEvent::ActivityEnded {
                    game_id: "roblox".into(),
                    ts: LEAVE_MS,
                },
            ],
            "the second leave line finds nothing open"
        );
        assert!(s.in_experience.is_none());
    }

    #[test]
    fn a_leave_without_a_join_is_quiet() {
        let (tx, rx) = mpsc::channel();
        let mut s = live_state();
        fold_line(&mut s, LEAVE, &tx);
        assert!(rx.try_iter().next().is_none());
    }

    #[test]
    fn a_teleport_moves_the_activity_without_closing_it() {
        let (tx, rx) = mpsc::channel();
        let mut s = live_state();
        fold_line(&mut s, JOIN, &tx);
        let other = JOIN.replace("107778070777162", "999");
        fold_line(&mut s, &other, &tx);
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[1], TelemetryEvent::ActivityStarted { activity_id, .. } if activity_id == "999")
        );
        assert_eq!(
            s.in_experience.as_ref().map(|(p, _)| p.as_str()),
            Some("999")
        );
    }

    /// History read at start folds silently: a past experience must not open
    /// and close a session now.
    #[test]
    fn replayed_history_emits_nothing() {
        let (tx, rx) = mpsc::channel();
        let mut s = RobloxState {
            in_experience: None,
            replaying: true,
        };
        fold_line(&mut s, JOIN, &tx);
        fold_line(&mut s, LEAVE, &tx);
        assert!(rx.try_iter().next().is_none());
        assert!(s.in_experience.is_none());

        fold_line(&mut s, JOIN, &tx);
        assert!(rx.try_iter().next().is_none());
        assert_eq!(
            s.in_experience,
            Some(("107778070777162".to_string(), JOIN_MS)),
            "the state knows the player is in an experience since the join"
        );
    }

    #[test]
    fn the_newest_player_log_is_picked_and_the_crash_handler_skipped() {
        let dir = std::env::temp_dir().join(format!("mello-roblox-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("0.745_20261102T134451Z_Player_885F4_last.log");
        let crash = dir.join("0.745_20261102T135009Z_Player_D2867_CrashHandler_last.log");
        let new = dir.join("0.745_20261102T134958Z_Player_3B071_last.log");
        std::fs::write(&old, "old\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(&new, "new\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(&crash, "crash\n").unwrap();
        assert_eq!(newest_player_log(&dir), Some(new));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
