//! Hearthstone `Power.log` adapter (log tailer).
//!
//! Blizzard-tolerated deck-tracker mechanism: `ensure_installed` drops the
//! `log.config` that enables Power logging, and the tailer follows the newest
//! session log. Outcomes come from `PLAYSTATE` tag changes; the local player
//! is identified the way trackers do it — the side whose hand cards are
//! *revealed* (non-empty card id in a `zone=HAND` `FULL_ENTITY`/`SHOW_ENTITY`
//! packet) can only be the friendly player.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use super::{
    GameTelemetryAdapter, MatchResult, Outcome, SourceQuality, TelemetryError, TelemetryEvent,
};

const GAME_ID: &str = "hearthstone";

#[derive(Default)]
struct HsGame {
    started: bool,
    friendly_player_id: Option<u8>,
    /// PlayerID → PlayerName from `DebugPrintGame` lines.
    names: HashMap<u8, String>,
    result_emitted: bool,
}

pub struct HearthstoneAdapter {
    running: Arc<AtomicBool>,
}

impl HearthstoneAdapter {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Default for HearthstoneAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GameTelemetryAdapter for HearthstoneAdapter {
    fn game_id(&self) -> &str {
        GAME_ID
    }

    fn info(&self) -> super::AdapterInfo {
        super::AdapterInfo {
            game_name: "Hearthstone",
            writes_files: true,
            note: "Enables Hearthstone's built-in match logging (the same mechanism deck trackers use) and reads the log while you play.",
            account_link: None,
        }
    }

    fn detect_install(&self) -> Option<bool> {
        #[cfg(windows)]
        {
            Some(hearthstone_install_dir().is_some())
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    fn ensure_installed(&self, _token: &str, _port: u16) -> Result<(), TelemetryError> {
        install_log_config()
    }

    fn start(&self, tx: Sender<TelemetryEvent>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let state = Mutex::new(HsGame::default());
        super::log_tail::spawn_tail(
            "hearthstone-power-log",
            self.running.clone(),
            power_log_path,
            move |line| {
                let mut st = state.lock().expect("hs state poisoned");
                for ev in parse_line(&mut st, line) {
                    let _ = tx.send(ev);
                }
            },
        );
        log::info!("[telemetry] hearthstone power log tailer started");
    }

    fn reset(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

/// Fold one Power.log line into the per-game state, emitting events. Only
/// `GameState.*` lines are used (`PowerTaskList.*` duplicates the same data
/// delayed by animations).
fn parse_line(st: &mut HsGame, line: &str) -> Vec<TelemetryEvent> {
    if !line.contains("GameState.") {
        return Vec::new();
    }

    if line.contains("CREATE_GAME") {
        *st = HsGame {
            started: true,
            ..HsGame::default()
        };
        return vec![TelemetryEvent::MatchStarted {
            mode: "match".to_string(),
            map: String::new(),
        }];
    }
    if !st.started {
        return Vec::new();
    }

    // "GameState.DebugPrintGame() - PlayerID=1, PlayerName=Foo#1234"
    if let Some(rest) = line.split("PlayerID=").nth(1) {
        if let Some((id, name_part)) = rest.split_once(", PlayerName=") {
            if let Ok(id) = id.trim().parse::<u8>() {
                let name = name_part.trim();
                if !name.is_empty() && name != "UNKNOWN HUMAN PLAYER" {
                    st.names.insert(id, name.to_string());
                }
            }
        }
        return Vec::new();
    }

    // Friendly-player detection: a hand card with a revealed (non-empty) card
    // id can only belong to the local player.
    if st.friendly_player_id.is_none()
        && (line.contains("FULL_ENTITY") || line.contains("SHOW_ENTITY"))
        && line.contains("zone=HAND")
    {
        let revealed = bracket_field(line, "cardId=").is_some_and(|v| !v.is_empty());
        if revealed {
            if let Some(player) = bracket_field(line, "player=").and_then(|v| v.parse::<u8>().ok())
            {
                st.friendly_player_id = Some(player);
            }
        }
        return Vec::new();
    }

    // "TAG_CHANGE Entity=Foo#1234 tag=PLAYSTATE value=WON"
    if line.contains("tag=PLAYSTATE") && !st.result_emitted {
        let entity = line
            .split("Entity=")
            .nth(1)
            .and_then(|r| r.split(" tag=").next())
            .unwrap_or("")
            .trim();
        let value = line
            .split("value=")
            .nth(1)
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("");

        let friendly_name = st
            .friendly_player_id
            .and_then(|id| st.names.get(&id))
            .cloned();
        let outcome = match (value, friendly_name.as_deref()) {
            ("TIED", _) => Some(Outcome::Draw),
            ("WON", Some(name)) if name == entity => Some(Outcome::Win),
            // Someone else won a two-player game we can attribute → our loss.
            ("WON", Some(_)) => Some(Outcome::Loss),
            ("WON", None) => Some(Outcome::Incomplete), // couldn't identify our side
            _ => None, // LOST/CONCEDED lines are redundant with the WON line
        };
        if let Some(result) = outcome {
            st.result_emitted = true;
            st.started = false;
            return vec![TelemetryEvent::MatchEnded(Box::new(MatchResult {
                game_id: GAME_ID.to_string(),
                mode: "match".to_string(),
                map: String::new(),
                result,
                // The log doesn't expose the queue (ranked/casual/solo);
                // all completed games count in v1, matching the LoR decision.
                streak_eligible: true,
                own_score: 0,
                opp_score: 0,
                performance: None,
                build: None,
                run: None,
                source: SourceQuality::Live,
                ts: now_ms(),
            }))];
        }
    }

    Vec::new()
}

/// Extract a `key=value` field from inside a `[...]` entity descriptor, where
/// values run until the next space or closing bracket.
fn bracket_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split(key)
        .nth(1)
        .map(|rest| rest.split([' ', ']']).next().unwrap_or(""))
}

// ---------------------------------------------------------------------------
// Install & path discovery (Windows-first, like the other adapters).
// ---------------------------------------------------------------------------

/// The `log.config` that turns on Power logging. Written only when the Power
/// section is absent so a user's own logging config is never clobbered.
#[cfg(windows)]
const LOG_CONFIG_SECTION: &str =
    "\n[Power]\nLogLevel=1\nFilePrinting=True\nConsolePrinting=False\nScreenPrinting=False\nVerbose=True\n";

#[cfg(windows)]
fn install_log_config() -> Result<(), TelemetryError> {
    let local = std::env::var("LOCALAPPDATA")
        .map_err(|_| TelemetryError::GameNotFound("LOCALAPPDATA not set".into()))?;
    let dir = std::path::PathBuf::from(local)
        .join("Blizzard")
        .join("Hearthstone");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("log.config");

    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current.contains("[Power]") {
        log::debug!("[telemetry] hearthstone log.config already has a Power section");
        return Ok(());
    }
    std::fs::write(&path, format!("{current}{LOG_CONFIG_SECTION}"))?;
    log::info!(
        "[telemetry] installed hearthstone log.config at {}",
        path.display()
    );
    Ok(())
}

#[cfg(not(windows))]
fn install_log_config() -> Result<(), TelemetryError> {
    Err(TelemetryError::Unsupported)
}

/// Newest `Logs/Hearthstone_*/Power.log` under the install dir (modern builds
/// use per-session log directories), falling back to the legacy flat path.
#[cfg(windows)]
fn power_log_path() -> Option<std::path::PathBuf> {
    let install = hearthstone_install_dir()?;
    let logs = install.join("Logs");

    let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    if let Ok(entries) = std::fs::read_dir(&logs) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("Power.log");
            if !candidate.is_file() {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
                newest = Some((modified, candidate));
            }
        }
    }
    newest
        .map(|(_, p)| p)
        .or_else(|| Some(logs.join("Power.log")).filter(|p| p.is_file()))
}

#[cfg(windows)]
fn hearthstone_install_dir() -> Option<std::path::PathBuf> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let from_registry: Option<String> = hklm
        .open_subkey(
            "SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Hearthstone",
        )
        .ok()
        .and_then(|k| k.get_value("InstallLocation").ok());
    let candidates = [
        from_registry.unwrap_or_default(),
        "C:\\Program Files (x86)\\Hearthstone".to_string(),
    ];
    candidates
        .iter()
        .filter(|c| !c.is_empty())
        .map(std::path::PathBuf::from)
        .find(|p| p.is_dir())
}

#[cfg(not(windows))]
fn power_log_path() -> Option<std::path::PathBuf> {
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

    fn play(st: &mut HsGame, lines: &[&str]) -> Vec<TelemetryEvent> {
        lines.iter().flat_map(|l| parse_line(st, l)).collect()
    }

    const CREATE: &str = "D 21:35:41.1234567 GameState.DebugPrintPower() - CREATE_GAME";
    const P1: &str = "D 21:35:41.2 GameState.DebugPrintGame() - PlayerID=1, PlayerName=Mello#2137";
    const P2: &str = "D 21:35:41.3 GameState.DebugPrintGame() - PlayerID=2, PlayerName=Rival#1111";
    // Friendly hand card revealed for player 1.
    const HAND: &str = "D 21:35:45.0 GameState.DebugPrintPower() -     FULL_ENTITY - Updating [entityName=Nerubian Prophet id=64 zone=HAND zonePos=3 cardId=OG_138 player=1] CardID=OG_138";
    // Opponent hand cards are hidden (empty cardId) — must not classify.
    const HIDDEN: &str = "D 21:35:45.1 GameState.DebugPrintPower() -     FULL_ENTITY - Updating [entityName=UNKNOWN ENTITY [cardType=INVALID] id=68 zone=HAND zonePos=1 cardId= player=2] CardID=";

    #[test]
    fn win_attributed_to_friendly_player() {
        let mut st = HsGame::default();
        let evs = play(&mut st, &[CREATE, P1, P2, HIDDEN, HAND]);
        assert!(matches!(evs[0], TelemetryEvent::MatchStarted { .. }));
        assert_eq!(st.friendly_player_id, Some(1));

        let evs = play(
            &mut st,
            &["D 21:50:00.0 GameState.DebugPrintPower() - TAG_CHANGE Entity=Mello#2137 tag=PLAYSTATE value=WON"],
        );
        let ended = match &evs[0] {
            TelemetryEvent::MatchEnded(m) => m,
            other => panic!("expected MatchEnded, got {other:?}"),
        };
        assert_eq!(ended.result, Outcome::Win);
    }

    #[test]
    fn opponent_win_is_our_loss() {
        let mut st = HsGame::default();
        play(&mut st, &[CREATE, P1, P2, HAND]);
        let evs = play(
            &mut st,
            &["D 21:50:00.0 GameState.DebugPrintPower() - TAG_CHANGE Entity=Rival#1111 tag=PLAYSTATE value=WON"],
        );
        let ended = match &evs[0] {
            TelemetryEvent::MatchEnded(m) => m,
            other => panic!("expected MatchEnded, got {other:?}"),
        };
        assert_eq!(ended.result, Outcome::Loss);
    }

    #[test]
    fn unidentified_side_yields_incomplete() {
        let mut st = HsGame::default();
        play(&mut st, &[CREATE, P1, P2]); // never saw a revealed hand card
        let evs = play(
            &mut st,
            &["D 21:50:00.0 GameState.DebugPrintPower() - TAG_CHANGE Entity=Rival#1111 tag=PLAYSTATE value=WON"],
        );
        let ended = match &evs[0] {
            TelemetryEvent::MatchEnded(m) => m,
            other => panic!("expected MatchEnded, got {other:?}"),
        };
        assert_eq!(ended.result, Outcome::Incomplete);
    }

    #[test]
    fn losing_playstate_lines_and_ptl_are_ignored() {
        let mut st = HsGame::default();
        play(&mut st, &[CREATE, P1, P2, HAND]);
        // PowerTaskList duplicates must not double-handle; LOST is redundant.
        let evs = play(
            &mut st,
            &[
                "D 21:50:00.0 PowerTaskList.DebugPrintPower() - TAG_CHANGE Entity=Mello#2137 tag=PLAYSTATE value=WON",
                "D 21:50:00.1 GameState.DebugPrintPower() - TAG_CHANGE Entity=Rival#1111 tag=PLAYSTATE value=LOST",
            ],
        );
        assert!(evs.is_empty());

        // The real GameState WON line still lands, exactly once.
        let evs = play(
            &mut st,
            &[
                "D 21:50:00.2 GameState.DebugPrintPower() - TAG_CHANGE Entity=Mello#2137 tag=PLAYSTATE value=WON",
                "D 21:50:00.3 GameState.DebugPrintPower() - TAG_CHANGE Entity=Mello#2137 tag=PLAYSTATE value=WON",
            ],
        );
        assert_eq!(evs.len(), 1);
    }

    #[test]
    fn tie_maps_to_draw() {
        let mut st = HsGame::default();
        play(&mut st, &[CREATE, P1, P2, HAND]);
        let evs = play(
            &mut st,
            &["D 21:50:00.0 GameState.DebugPrintPower() - TAG_CHANGE Entity=Mello#2137 tag=PLAYSTATE value=TIED"],
        );
        let ended = match &evs[0] {
            TelemetryEvent::MatchEnded(m) => m,
            other => panic!("expected MatchEnded, got {other:?}"),
        };
        assert_eq!(ended.result, Outcome::Draw);
    }
}
