//! Legends of Runeterra local Game Client API adapter.
//!
//! Active source: the LoR client hosts `http://127.0.0.1:21337` (enabled by
//! default, plain HTTP). `/game-result` returns `{ GameID, LocalPlayerWon }`
//! for the most recently completed game — `GameID` resets per client launch
//! and increments per game — and `/static-decklist` returns the active deck
//! code during a match. We poll both: a `GameID` bump emits `MatchEnded`,
//! and the last seen deck code fills the `build` slot (the alpha-user ask
//! for card games).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use super::{
    BuildInfo, GameTelemetryAdapter, MatchResult, Outcome, SourceQuality, TelemetryError,
    TelemetryEvent,
};

const GAME_ID: &str = "legends-of-runeterra";
const RESULT_ENDPOINT: &str = "http://127.0.0.1:21337/game-result";
const DECK_ENDPOINT: &str = "http://127.0.0.1:21337/static-decklist";
const POLL_INTERVAL_MS: u64 = 3_000;
const SLEEP_SLICE_MS: u64 = 100;

#[derive(Default)]
struct LorState {
    /// GameID seen on the first successful poll; results are only emitted for
    /// ids above this baseline (old results linger across games).
    baseline: Option<i64>,
    last_emitted: Option<i64>,
    /// Deck code captured from the most recent active game.
    deck_code: String,
    match_started: bool,
}

pub struct LorAdapter {
    state: Arc<Mutex<LorState>>,
    running: Arc<AtomicBool>,
}

impl LorAdapter {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(LorState::default())),
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Default for LorAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GameTelemetryAdapter for LorAdapter {
    fn game_id(&self) -> &str {
        GAME_ID
    }

    fn info(&self) -> super::AdapterInfo {
        super::AdapterInfo {
            game_name: "Legends of Runeterra",
            writes_files: false,
            note: "Reads game results and decklists from the game's own local API while you play. Nothing is installed.",
            account_link: None,
        }
    }

    fn ensure_installed(&self, _token: &str, _port: u16) -> Result<(), TelemetryError> {
        // Nothing to install: the Game Client API is on by default.
        Ok(())
    }

    fn start(&self, tx: Sender<TelemetryEvent>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.state.lock().expect("lor telemetry state poisoned") = LorState::default();

        let running = self.running.clone();
        let state = self.state.clone();
        let spawn = std::thread::Builder::new()
            .name("lor-local-poll".into())
            .spawn(move || poll_loop(&running, &state, &tx));
        match spawn {
            Ok(_) => log::info!("[telemetry] lor game client poller started"),
            Err(e) => {
                log::warn!("[telemetry] lor poll thread failed to spawn: {e}");
                self.running.store(false, Ordering::SeqCst);
            }
        }
    }

    fn reset(&self) {
        self.running.store(false, Ordering::SeqCst);
        *self.state.lock().expect("lor telemetry state poisoned") = LorState::default();
    }
}

fn poll_loop(running: &AtomicBool, state: &Mutex<LorState>, tx: &Sender<TelemetryEvent>) {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[telemetry] lor poll client build failed: {e}");
            running.store(false, Ordering::SeqCst);
            return;
        }
    };

    while running.load(Ordering::SeqCst) {
        let result = client
            .get(RESULT_ENDPOINT)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.text())
            .ok();
        let deck = client
            .get(DECK_ENDPOINT)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.text())
            .ok();

        for ev in digest(state, result.as_deref(), deck.as_deref()) {
            if tx.send(ev).is_err() {
                log::info!("[telemetry] lor poll receiver dropped, stopping");
                running.store(false, Ordering::SeqCst);
                return;
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

/// Digest one poll round (`/game-result` + `/static-decklist` bodies) into
/// telemetry events. Pure state machine so tests can drive it directly.
fn digest(
    state: &Mutex<LorState>,
    result_body: Option<&str>,
    deck_body: Option<&str>,
) -> Vec<TelemetryEvent> {
    let mut st = state.lock().expect("lor telemetry state poisoned");
    let mut events = Vec::new();

    // An active deck means a game is running; remember the code for the
    // eventual result and surface the match start once.
    if let Some(deck) = deck_body.and_then(|b| serde_json::from_str::<DeckList>(b).ok()) {
        if !deck.deck_code.is_empty() {
            st.deck_code = deck.deck_code;
            if !st.match_started {
                st.match_started = true;
                events.push(TelemetryEvent::MatchStarted {
                    mode: "match".to_string(),
                    map: String::new(),
                });
            }
        }
    }

    let Some(result) = result_body.and_then(|b| serde_json::from_str::<GameResult>(b).ok()) else {
        return events;
    };

    // First successful poll only sets the baseline: the endpoint reports the
    // *previous* game's result, which isn't from this session.
    let Some(baseline) = st.baseline else {
        st.baseline = Some(result.game_id);
        st.last_emitted = Some(result.game_id);
        return events;
    };

    if result.game_id > baseline && st.last_emitted != Some(result.game_id) {
        st.last_emitted = Some(result.game_id);
        st.match_started = false;
        let deck_code = std::mem::take(&mut st.deck_code);
        let build = (!deck_code.is_empty()).then(|| BuildInfo {
            deck_code: Some(deck_code),
            ..BuildInfo::default()
        });
        events.push(TelemetryEvent::MatchEnded(Box::new(MatchResult {
            game_id: GAME_ID.to_string(),
            mode: "match".to_string(),
            map: String::new(),
            result: if result.local_player_won {
                Outcome::Win
            } else {
                Outcome::Loss
            },
            // The API doesn't expose the queue (ranked/normal/AI); all
            // completed games count in v1.
            streak_eligible: true,
            own_score: 0,
            opp_score: 0,
            performance: None,
            build,
            run: None,
            source: SourceQuality::Live,
            ts: now_ms(),
        })));
    }

    events
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[derive(Deserialize, Default)]
struct GameResult {
    #[serde(default, rename = "GameID")]
    game_id: i64,
    #[serde(default, rename = "LocalPlayerWon")]
    local_player_won: bool,
}

#[derive(Deserialize, Default)]
struct DeckList {
    #[serde(default, rename = "DeckCode")]
    deck_code: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: i64, won: bool) -> String {
        format!(r#"{{ "GameID": {id}, "LocalPlayerWon": {won} }}"#)
    }

    const DECK: &str = r#"{ "DeckCode": "CEBAIAIFB4WDANQIAEAQGDAUDAQSIJZUAIAQCBIFAEAQCBAA", "CardsInDeck": { "01SI015": 3 } }"#;

    #[test]
    fn baseline_result_is_not_emitted() {
        let state = Mutex::new(LorState::default());
        // First poll sees a stale result from before this session.
        assert!(digest(&state, Some(&result(4, true)), None).is_empty());
        // Same id later: still nothing.
        assert!(digest(&state, Some(&result(4, true)), None).is_empty());
    }

    #[test]
    fn game_id_bump_emits_result_with_deck() {
        let state = Mutex::new(LorState::default());
        digest(&state, Some(&result(4, false)), None);

        // A deck appears (game running) → MatchStarted.
        let evs = digest(&state, Some(&result(4, false)), Some(DECK));
        assert!(matches!(evs[0], TelemetryEvent::MatchStarted { .. }));

        // GameID bumps with a win.
        let evs = digest(&state, Some(&result(5, true)), None);
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .expect("expected MatchEnded");
        assert_eq!(ended.result, Outcome::Win);
        assert_eq!(
            ended.build.as_ref().and_then(|b| b.deck_code.as_deref()),
            Some("CEBAIAIFB4WDANQIAEAQGDAUDAQSIJZUAIAQCBIFAEAQCBAA")
        );

        // Trailing polls with the same id don't re-emit.
        assert!(digest(&state, Some(&result(5, true)), None).is_empty());
    }

    #[test]
    fn loss_maps_to_loss() {
        let state = Mutex::new(LorState::default());
        digest(&state, Some(&result(0, true)), None);
        let evs = digest(&state, Some(&result(1, false)), None);
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .expect("expected MatchEnded");
        assert_eq!(ended.result, Outcome::Loss);
        assert!(ended.build.is_none());
    }

    #[test]
    fn unreachable_api_is_quiet() {
        let state = Mutex::new(LorState::default());
        assert!(digest(&state, None, None).is_empty());
        assert!(digest(&state, Some("not json"), Some("nope")).is_empty());
    }
}
