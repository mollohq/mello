//! Crash-safe record of the game sessions that were open when the client last
//! ran.
//!
//! Without this, closing Mello mid-game silently discards the session: the
//! sensor starts from an empty table, never sees a `Stopped`, and the night's
//! play is never recorded. On restart the sensor reconciles this file against
//! the live process table — a session whose process is still running resumes
//! with its original start time, one whose process is gone is closed out at
//! `last_seen_ms` and reported normally.
//!
//! A pid alone is not identity: Windows recycles pids aggressively, so an
//! unrelated process can inherit one within a single reboot. `(pid,
//! started_at_ms)` is what actually identifies a process, and both halves must
//! match before a session is resumed.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedSession {
    pub pid: u32,
    /// Process creation time; pairs with `pid` to identify the process.
    /// 0 means libmello could not read it, which makes the session
    /// unresumable — see [`resume_matches`].
    pub started_at_ms: i64,
    pub game_id: String,
    pub game_name: String,
    pub short_name: String,
    pub color: String,
    pub exe: String,
    /// Wall-clock time of the last scan that saw this process alive. Used as
    /// the end timestamp when the process is gone by the time we restart.
    pub last_seen_ms: i64,
    /// Milliseconds this game held the foreground, accumulated across scans.
    #[serde(default)]
    pub foreground_ms: i64,
}

/// Whether a persisted session may be resumed by a live process.
///
/// Requires both halves of the identity to match *and* to be known. A zero
/// `started_at_ms` on either side means we cannot tell the original process
/// from a pid-recycled impostor, so we refuse to resume rather than risk
/// attributing someone's next session to the wrong game.
pub fn resume_matches(persisted: &PersistedSession, live_pid: u32, live_started_at: i64) -> bool {
    persisted.pid == live_pid
        && persisted.started_at_ms != 0
        && live_started_at != 0
        && persisted.started_at_ms == live_started_at
}

fn store_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("mello").join("active_sessions.json"))
}

/// Overwrite the store with the currently open sessions. Best-effort: a failed
/// write costs us restart recovery, never the live session.
pub fn save(sessions: &[PersistedSession]) {
    let Some(path) = store_path() else {
        return;
    };
    if sessions.is_empty() {
        // Nothing open — drop the file so a stale one cannot be replayed.
        let _ = std::fs::remove_file(&path);
        return;
    }
    let Ok(json) = serde_json::to_string(sessions) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, json) {
        log::warn!("[game-sensor] could not persist active sessions: {e}");
    }
}

/// Read and consume the store. Removing it on load means a crash during
/// reconciliation cannot replay the same sessions forever.
pub fn take() -> Vec<PersistedSession> {
    let Some(path) = store_path() else {
        return Vec::new();
    };
    let Ok(json) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let _ = std::fs::remove_file(&path);
    match serde_json::from_str(&json) {
        Ok(sessions) => sessions,
        Err(e) => {
            log::warn!("[game-sensor] discarding unreadable session store: {e}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(pid: u32, started: i64) -> PersistedSession {
        PersistedSession {
            pid,
            started_at_ms: started,
            game_id: "counter-strike-2".into(),
            game_name: "Counter-Strike 2".into(),
            short_name: "CS2".into(),
            color: "#DE9B35".into(),
            exe: "cs2.exe".into(),
            last_seen_ms: 1_000,
            foreground_ms: 0,
        }
    }

    #[test]
    fn resumes_when_pid_and_start_time_both_match() {
        assert!(resume_matches(&session(1234, 500), 1234, 500));
    }

    #[test]
    fn refuses_recycled_pid() {
        // Same pid, different process: the classic pid-reuse trap. Resuming
        // here would attribute a stranger's process to this game session.
        assert!(!resume_matches(&session(1234, 500), 1234, 900));
    }

    #[test]
    fn refuses_when_start_time_unknown() {
        // libmello could not read the creation time on one side or the other,
        // so pid is all we have — not enough to be sure.
        assert!(!resume_matches(&session(1234, 0), 1234, 500));
        assert!(!resume_matches(&session(1234, 500), 1234, 0));
        assert!(!resume_matches(&session(1234, 0), 1234, 0));
    }

    #[test]
    fn refuses_different_pid() {
        assert!(!resume_matches(&session(1234, 500), 9999, 500));
    }

    #[test]
    fn roundtrips_through_json() {
        let sessions = vec![session(1, 100), session(2, 200)];
        let json = serde_json::to_string(&sessions).unwrap();
        let back: Vec<PersistedSession> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sessions);
    }

    #[test]
    fn foreground_ms_defaults_for_older_stores() {
        // A store written before foreground accounting existed must still load.
        let json = r##"[{"pid":1,"started_at_ms":100,"game_id":"g","game_name":"G",
            "short_name":"G","color":"#fff","exe":"g.exe","last_seen_ms":5}]"##;
        let back: Vec<PersistedSession> = serde_json::from_str(json).unwrap();
        assert_eq!(back[0].foreground_ms, 0);
    }
}
