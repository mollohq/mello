//! StarCraft II Client API adapter (local poll).
//!
//! Every running SC2 client serves an unauthenticated loopback API on
//! `127.0.0.1:6119`: `/ui` lists the active menu screens (empty while in a
//! game) and `/game` lists the players of the current/last game with their
//! `result` once decided. We poll both and turn the "in game → back to menus"
//! transition into a `MatchEnded`.
//!
//! Attribution limit: the API doesn't say which participant is the local
//! player. With exactly one `user`-type player (vs AI) the result is theirs;
//! ladder games with two `user` players are recorded played-only
//! (`Incomplete`) until cross-game name tracking exists.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use super::{
    BuildInfo, GameTelemetryAdapter, MatchResult, Outcome, SourceQuality, TelemetryError,
    TelemetryEvent,
};

const GAME_ID: &str = "starcraft-2";
const UI_ENDPOINT: &str = "http://127.0.0.1:6119/ui";
const GAME_ENDPOINT: &str = "http://127.0.0.1:6119/game";
const POLL_INTERVAL_MS: u64 = 2_000;
const SLEEP_SLICE_MS: u64 = 100;

#[derive(Default)]
struct Sc2State {
    in_game: bool,
    is_replay: bool,
    mode: String,
}

pub struct Sc2ClientAdapter {
    state: Arc<Mutex<Sc2State>>,
    running: Arc<AtomicBool>,
}

impl Sc2ClientAdapter {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(Sc2State::default())),
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Default for Sc2ClientAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GameTelemetryAdapter for Sc2ClientAdapter {
    fn game_id(&self) -> &str {
        GAME_ID
    }

    fn info(&self) -> super::AdapterInfo {
        super::AdapterInfo {
            game_name: "StarCraft II",
            writes_files: false,
            note: "Reads match state from the game's own local API while you play. Nothing is installed.",
            account_link: None,
        }
    }

    fn ensure_installed(&self, _token: &str, _port: u16) -> Result<(), TelemetryError> {
        // Nothing to install: the client API is always on.
        Ok(())
    }

    fn start(&self, tx: Sender<TelemetryEvent>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.state.lock().expect("sc2 telemetry state poisoned") = Sc2State::default();

        let running = self.running.clone();
        let state = self.state.clone();
        let spawn = std::thread::Builder::new()
            .name("sc2-client-poll".into())
            .spawn(move || poll_loop(&running, &state, &tx));
        match spawn {
            Ok(_) => log::info!("[telemetry] sc2 client api poller started"),
            Err(e) => {
                log::warn!("[telemetry] sc2 poll thread failed to spawn: {e}");
                self.running.store(false, Ordering::SeqCst);
            }
        }
    }

    fn reset(&self) {
        self.running.store(false, Ordering::SeqCst);
        *self.state.lock().expect("sc2 telemetry state poisoned") = Sc2State::default();
    }
}

fn poll_loop(running: &AtomicBool, state: &Mutex<Sc2State>, tx: &Sender<TelemetryEvent>) {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[telemetry] sc2 poll client build failed: {e}");
            running.store(false, Ordering::SeqCst);
            return;
        }
    };

    while running.load(Ordering::SeqCst) {
        let fetch = |url: &str| {
            client
                .get(url)
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.text())
                .ok()
        };
        if let (Some(ui), Some(game)) = (fetch(UI_ENDPOINT), fetch(GAME_ENDPOINT)) {
            for ev in digest(state, &ui, &game) {
                if tx.send(ev).is_err() {
                    log::info!("[telemetry] sc2 poll receiver dropped, stopping");
                    running.store(false, Ordering::SeqCst);
                    return;
                }
            }
        }

        let mut slept = 0;
        while slept < POLL_INTERVAL_MS {
            if !running.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(SLEEP_SLICE_MS));
            slept += SLEEP_SLICE_MS;
        }
    }
}

/// Digest one `/ui` + `/game` snapshot pair. Pure state machine so tests can
/// drive it without the network.
fn digest(state: &Mutex<Sc2State>, ui_body: &str, game_body: &str) -> Vec<TelemetryEvent> {
    let ui: UiInfo = match serde_json::from_str(ui_body) {
        Ok(u) => u,
        Err(_) => return Vec::new(),
    };
    let game: GameInfo = match serde_json::from_str(game_body) {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };

    let in_game_now = ui.active_screens.is_empty();
    let mut st = state.lock().expect("sc2 telemetry state poisoned");

    if in_game_now && !st.in_game {
        if game.players.is_empty() {
            return Vec::new(); // loading screen edge; wait for a real snapshot
        }
        st.in_game = true;
        st.is_replay = game.is_replay;
        st.mode = derive_mode(&game.players);
        if st.is_replay {
            return Vec::new();
        }
        return vec![TelemetryEvent::MatchStarted {
            mode: st.mode.clone(),
            map: String::new(),
        }];
    }

    if !in_game_now && st.in_game {
        let was_replay = st.is_replay;
        let mode = std::mem::take(&mut st.mode);
        st.in_game = false;
        st.is_replay = false;
        if was_replay {
            return Vec::new();
        }
        return vec![TelemetryEvent::MatchEnded(Box::new(conclude(
            &mode,
            &game.players,
        )))];
    }

    Vec::new()
}

fn derive_mode(players: &[PlayerInfo]) -> String {
    let users = players.iter().filter(|p| p.kind == "user").count();
    if users <= 1 {
        "vs AI".to_string()
    } else {
        let half = players.len() / 2;
        format!("{half}v{}", players.len() - half)
    }
}

fn conclude(mode: &str, players: &[PlayerInfo]) -> MatchResult {
    let users: Vec<&PlayerInfo> = players.iter().filter(|p| p.kind == "user").collect();
    let (result, streak_eligible, build) = match users.as_slice() {
        [me] => {
            let outcome = match me.result.as_str() {
                "Victory" => Outcome::Win,
                "Defeat" => Outcome::Loss,
                "Tie" => Outcome::Draw,
                _ => Outcome::Incomplete,
            };
            let build = (!me.race.is_empty()).then(|| BuildInfo {
                character: Some(race_name(&me.race).to_string()),
                ..BuildInfo::default()
            });
            // vs AI never moves a streak.
            (outcome, false, build)
        }
        // Multiple human players: the API can't tell us which one is local,
        // so record the match played-only rather than guessing.
        _ => (Outcome::Incomplete, false, None),
    };

    MatchResult {
        game_id: GAME_ID.to_string(),
        mode: mode.to_string(),
        map: String::new(),
        result,
        streak_eligible,
        own_score: 0,
        opp_score: 0,
        performance: None,
        build,
        run: None,
        source: SourceQuality::Live,
        ts: now_ms(),
    }
}

/// The API abbreviates races ("Terr", "Prot"); expand for display.
fn race_name(raw: &str) -> &str {
    match raw {
        "Terr" => "Terran",
        "Prot" => "Protoss",
        "Zerg" => "Zerg",
        "random" => "Random",
        other => other,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ---------------------------------------------------------------------------
// Client API payloads (subset).
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct UiInfo {
    #[serde(default, rename = "activeScreens")]
    active_screens: Vec<String>,
}

#[derive(Deserialize, Default)]
struct GameInfo {
    #[serde(default, rename = "isReplay")]
    is_replay: bool,
    #[serde(default)]
    players: Vec<PlayerInfo>,
}

#[derive(Deserialize, Default)]
struct PlayerInfo {
    /// "user" or "computer".
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    race: String,
    /// "Victory" / "Defeat" / "Tie" / "Undecided".
    #[serde(default)]
    result: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MENUS: &str = r#"{"activeScreens": ["ScreenHome/ScreenHome"]}"#;
    const IN_GAME: &str = r#"{"activeScreens": []}"#;

    fn game(is_replay: bool, players: &str) -> String {
        format!(r#"{{"isReplay": {is_replay}, "displayTime": 100.0, "players": [{players}]}}"#)
    }

    const VS_AI_RUNNING: &str = r#"{"id": 1, "type": "user", "race": "Terr", "result": "Undecided"},
        {"id": 2, "type": "computer", "race": "random", "result": "Undecided"}"#;
    const VS_AI_WON: &str = r#"{"id": 1, "type": "user", "race": "Terr", "result": "Victory"},
        {"id": 2, "type": "computer", "race": "random", "result": "Defeat"}"#;
    const LADDER_DONE: &str = r#"{"id": 1, "type": "user", "race": "Zerg", "result": "Victory"},
        {"id": 2, "type": "user", "race": "Prot", "result": "Defeat"}"#;

    #[test]
    fn vs_ai_win_flow() {
        let state = Mutex::new(Sc2State::default());

        // Menus first: nothing.
        assert!(digest(&state, MENUS, &game(false, "")).is_empty());

        let evs = digest(&state, IN_GAME, &game(false, VS_AI_RUNNING));
        assert!(matches!(&evs[0], TelemetryEvent::MatchStarted { mode, .. } if mode == "vs AI"));

        // Mid-game polls are quiet.
        assert!(digest(&state, IN_GAME, &game(false, VS_AI_RUNNING)).is_empty());

        let evs = digest(&state, MENUS, &game(false, VS_AI_WON));
        let ended = match &evs[0] {
            TelemetryEvent::MatchEnded(m) => m,
            other => panic!("expected MatchEnded, got {other:?}"),
        };
        assert_eq!(ended.result, Outcome::Win);
        assert!(!ended.streak_eligible);
        assert_eq!(
            ended.build.as_ref().and_then(|b| b.character.as_deref()),
            Some("Terran")
        );
    }

    #[test]
    fn ladder_game_is_played_only() {
        let state = Mutex::new(Sc2State::default());
        digest(&state, IN_GAME, &game(false, LADDER_DONE));
        let evs = digest(&state, MENUS, &game(false, LADDER_DONE));
        let ended = match &evs[0] {
            TelemetryEvent::MatchEnded(m) => m,
            other => panic!("expected MatchEnded, got {other:?}"),
        };
        assert_eq!(ended.result, Outcome::Incomplete);
        assert!(!ended.streak_eligible);
        assert_eq!(ended.mode, "1v1");
    }

    #[test]
    fn replays_are_ignored_entirely() {
        let state = Mutex::new(Sc2State::default());
        assert!(digest(&state, IN_GAME, &game(true, LADDER_DONE)).is_empty());
        assert!(digest(&state, MENUS, &game(true, LADDER_DONE)).is_empty());
    }

    #[test]
    fn garbage_payloads_are_quiet() {
        let state = Mutex::new(Sc2State::default());
        assert!(digest(&state, "nope", "{}").is_empty());
        assert!(digest(&state, "{}", "nope").is_empty());
    }
}
