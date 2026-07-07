//! Path of Exile `Client.txt` adapter (log tailer).
//!
//! PoE writes a plain-text client log with zone entries, level-ups, and
//! deaths — reading it is long-standing GGG-tolerated tracker behavior. PoE
//! is sessionful rather than match-based, so this adapter accumulates a *run
//! summary* while the game runs and flushes one `MatchEnded` with the `run`
//! slot filled when the game exits (`reset()`, which the client calls before
//! folding the session — the result is `Incomplete`/not streak-eligible, so
//! it never moves a record; it feeds the "run stats" ask).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use super::{
    BuildInfo, GameTelemetryAdapter, MatchResult, Outcome, Performance, RunInfo, SourceQuality,
    TelemetryError, TelemetryEvent,
};

const GAME_ID: &str = "path-of-exile";

#[derive(Default, Clone)]
struct PoeRun {
    zones_entered: u32,
    last_zone: String,
    /// "Name (Class)" from the latest level-up line.
    character: String,
    level: u32,
    deaths: u32,
    started_ms: i64,
}

impl PoeRun {
    fn has_activity(&self) -> bool {
        self.zones_entered > 0 || self.level > 0 || self.deaths > 0
    }
}

pub struct PoeLogAdapter {
    run: Arc<Mutex<PoeRun>>,
    running: Arc<AtomicBool>,
    /// Kept from `start()` so `reset()` can flush the final run summary.
    tx: Mutex<Option<Sender<TelemetryEvent>>>,
}

impl PoeLogAdapter {
    pub fn new() -> Self {
        Self {
            run: Arc::new(Mutex::new(PoeRun::default())),
            running: Arc::new(AtomicBool::new(false)),
            tx: Mutex::new(None),
        }
    }
}

impl Default for PoeLogAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GameTelemetryAdapter for PoeLogAdapter {
    fn game_id(&self) -> &str {
        GAME_ID
    }

    fn info(&self) -> super::AdapterInfo {
        super::AdapterInfo {
            game_name: "Path of Exile",
            writes_files: false,
            note: "Reads the game's client log for zones, level-ups, and deaths while you play. Nothing is installed.",
            account_link: None,
        }
    }

    fn detect_install(&self) -> Option<bool> {
        #[cfg(windows)]
        {
            Some(client_log_path().is_some())
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    fn ensure_installed(&self, _token: &str, _port: u16) -> Result<(), TelemetryError> {
        // Nothing to install: the client log is always written.
        Ok(())
    }

    fn start(&self, tx: Sender<TelemetryEvent>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.run.lock().expect("poe run state poisoned") = PoeRun {
            started_ms: now_ms(),
            ..PoeRun::default()
        };
        *self.tx.lock().expect("poe tx poisoned") = Some(tx);

        let run = self.run.clone();
        super::log_tail::spawn_tail(
            "poe-client-log",
            self.running.clone(),
            client_log_path,
            move |line| {
                let mut r = run.lock().expect("poe run state poisoned");
                parse_line(&mut r, line);
            },
        );
        log::info!("[telemetry] poe client log tailer started");
    }

    fn reset(&self) {
        self.running.store(false, Ordering::SeqCst);
        let run = std::mem::take(&mut *self.run.lock().expect("poe run state poisoned"));
        let tx = self.tx.lock().expect("poe tx poisoned").take();
        if let (Some(tx), true) = (tx, run.has_activity()) {
            let _ = tx.send(TelemetryEvent::MatchEnded(Box::new(summarize(&run))));
        }
    }
}

/// Fold one client-log line into the run summary. Interesting lines all live
/// after the `] : ` marker (the game's chat/notification channel).
fn parse_line(run: &mut PoeRun, line: &str) {
    let Some(msg) = line.split("] : ").nth(1) else {
        return;
    };

    if let Some(zone) = msg
        .strip_prefix("You have entered ")
        .and_then(|z| z.strip_suffix('.'))
    {
        run.zones_entered += 1;
        run.last_zone = zone.to_string();
        return;
    }

    // "<Name> (<Class>) is now level <N>"
    if let Some((who, rest)) = msg.split_once(" is now level ") {
        if let Ok(level) = rest.trim_end_matches('.').trim().parse::<u32>() {
            run.character = who.to_string();
            run.level = run.level.max(level);
        }
        return;
    }

    if msg.ends_with("has been slain.") || msg.contains("has been slain") {
        run.deaths += 1;
    }
}

fn summarize(run: &PoeRun) -> MatchResult {
    let duration_sec = ((now_ms() - run.started_ms).max(0) / 1000) as u32;
    MatchResult {
        game_id: GAME_ID.to_string(),
        mode: "run".to_string(),
        map: run.last_zone.clone(),
        // A session, not a contest: never a W/L, never streaked.
        result: Outcome::Incomplete,
        streak_eligible: false,
        own_score: 0,
        opp_score: 0,
        performance: (run.deaths > 0).then(|| Performance {
            deaths: Some(run.deaths),
            ..Performance::default()
        }),
        build: (!run.character.is_empty()).then(|| BuildInfo {
            character: Some(if run.level > 0 {
                format!("{}, level {}", run.character, run.level)
            } else {
                run.character.clone()
            }),
            ..BuildInfo::default()
        }),
        run: Some(RunInfo {
            stage_reached: (!run.last_zone.is_empty()).then(|| run.last_zone.clone()),
            difficulty: None,
            duration_sec: Some(duration_sec),
        }),
        source: SourceQuality::Live,
        ts: now_ms(),
    }
}

/// `…/Path of Exile/logs/Client.txt` in any Steam library (Steam install
/// only for v1; the standalone client quietly contributes nothing).
#[cfg(windows)]
fn client_log_path() -> Option<std::path::PathBuf> {
    super::steam::find_app_subdir("Path of Exile", &["logs"], "poe logs not found")
        .ok()
        .map(|dir| dir.join("Client.txt"))
        .filter(|p| p.is_file())
}

#[cfg(not(windows))]
fn client_log_path() -> Option<std::path::PathBuf> {
    None
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

    const PREFIX: &str = "2026/07/03 12:00:00 123456789 3ef61a55 [INFO Client 20744]";

    #[test]
    fn run_accumulates_zones_levels_deaths() {
        let mut run = PoeRun::default();
        parse_line(
            &mut run,
            &format!("{PREFIX} : You have entered Lioneye's Watch."),
        );
        parse_line(
            &mut run,
            &format!("{PREFIX} : Balormoor (Witch) is now level 12"),
        );
        parse_line(&mut run, &format!("{PREFIX} : You have entered The Coast."));
        parse_line(&mut run, &format!("{PREFIX} : Balormoor has been slain."));
        parse_line(
            &mut run,
            &format!("{PREFIX} : Balormoor (Witch) is now level 13"),
        );
        // Noise is ignored.
        parse_line(&mut run, &format!("{PREFIX} : HeyChat: hello"));
        parse_line(&mut run, "malformed line without marker");

        assert_eq!(run.zones_entered, 2);
        assert_eq!(run.last_zone, "The Coast");
        assert_eq!(run.level, 13);
        assert_eq!(run.character, "Balormoor (Witch)");
        assert_eq!(run.deaths, 1);
    }

    #[test]
    fn summary_fills_run_slot() {
        let mut run = PoeRun {
            started_ms: now_ms() - 90_000,
            ..PoeRun::default()
        };
        parse_line(
            &mut run,
            &format!("{PREFIX} : You have entered The Twilight Strand."),
        );
        parse_line(
            &mut run,
            &format!("{PREFIX} : Balormoor (Witch) is now level 2"),
        );

        let m = summarize(&run);
        assert_eq!(m.result, Outcome::Incomplete);
        assert!(!m.streak_eligible);
        let info = m.run.as_ref().expect("run info");
        assert_eq!(info.stage_reached.as_deref(), Some("The Twilight Strand"));
        assert!(info.duration_sec.unwrap_or(0) >= 90);
        assert_eq!(
            m.build.as_ref().and_then(|b| b.character.as_deref()),
            Some("Balormoor (Witch), level 2")
        );
    }

    #[test]
    fn level_downgrade_is_ignored() {
        let mut run = PoeRun::default();
        parse_line(
            &mut run,
            &format!("{PREFIX} : Balormoor (Witch) is now level 90"),
        );
        parse_line(
            &mut run,
            &format!("{PREFIX} : AltChar (Duelist) is now level 3"),
        );
        assert_eq!(run.level, 90);
    }
}
