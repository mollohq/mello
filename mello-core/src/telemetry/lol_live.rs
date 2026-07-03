//! League of Legends Live Client Data API adapter.
//!
//! First *active* source (spec 18 §2.1): during a match the game client hosts
//! `https://127.0.0.1:2999/liveclientdata/allgamedata` (self-signed Riot
//! cert). No install step and no launch option — we poll while the game
//! process is running and derive the outcome from the `GameEnd` event.
//!
//! The poll worker is a plain thread (mirroring the listener pattern):
//! `start()` spawns it, `reset()` stops it via an atomic flag.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use super::{
    BuildInfo, GameTelemetryAdapter, MatchResult, Outcome, Performance, SourceQuality,
    TelemetryError, TelemetryEvent,
};

const GAME_ID: &str = "league-of-legends";
const ENDPOINT: &str = "https://127.0.0.1:2999/liveclientdata/allgamedata";
const POLL_INTERVAL_MS: u64 = 3_000;
/// Stop-check granularity while sleeping between polls.
const SLEEP_SLICE_MS: u64 = 100;

/// Modes without a matchmaking opponent never move a streak.
const NON_STREAK_MODES: [&str; 2] = ["PRACTICETOOL", "TUTORIAL"];

#[derive(Default)]
struct LolState {
    match_active: bool,
    /// Set once the current match's GameEnd was emitted so trailing polls of
    /// the same post-game data don't re-emit.
    match_ended: bool,
}

pub struct LolLiveAdapter {
    state: Arc<Mutex<LolState>>,
    running: Arc<AtomicBool>,
}

impl LolLiveAdapter {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(LolState::default())),
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Default for LolLiveAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GameTelemetryAdapter for LolLiveAdapter {
    fn game_id(&self) -> &str {
        GAME_ID
    }

    fn ensure_installed(&self, _token: &str, _port: u16) -> Result<(), TelemetryError> {
        // Nothing to install: the Live Client API is always on during a match.
        Ok(())
    }

    fn start(&self, tx: Sender<TelemetryEvent>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return; // already polling
        }
        *self.state.lock().expect("lol telemetry state poisoned") = LolState::default();

        let running = self.running.clone();
        let state = self.state.clone();
        let spawn = std::thread::Builder::new()
            .name("lol-live-poll".into())
            .spawn(move || poll_loop(&running, &state, &tx));
        match spawn {
            Ok(_) => log::info!("[telemetry] lol live client poller started"),
            Err(e) => {
                log::warn!("[telemetry] lol poll thread failed to spawn: {e}");
                self.running.store(false, Ordering::SeqCst);
            }
        }
    }

    fn reset(&self) {
        self.running.store(false, Ordering::SeqCst);
        *self.state.lock().expect("lol telemetry state poisoned") = LolState::default();
    }
}

fn poll_loop(running: &AtomicBool, state: &Mutex<LolState>, tx: &Sender<TelemetryEvent>) {
    // The API terminates TLS with Riot's self-signed loopback cert, so normal
    // verification can never pass. Accepting it is scoped to this client,
    // which only ever talks to 127.0.0.1.
    let client = match reqwest::blocking::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[telemetry] lol poll client build failed: {e}");
            running.store(false, Ordering::SeqCst);
            return;
        }
    };

    while running.load(Ordering::SeqCst) {
        let body = client
            .get(ENDPOINT)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.text());
        if let Ok(body) = body {
            // Errors just mean no match is running (lobby, champ select,
            // client closed); keep polling quietly until reset().
            for ev in digest(state, &body) {
                if tx.send(ev).is_err() {
                    log::info!("[telemetry] lol poll receiver dropped, stopping");
                    running.store(false, Ordering::SeqCst);
                    return;
                }
            }
        }

        // Sleep in slices so reset() stops the worker promptly.
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

/// Digest one `allgamedata` snapshot into telemetry events. Pure state
/// machine so tests can drive it without the network.
fn digest(state: &Mutex<LolState>, body: &str) -> Vec<TelemetryEvent> {
    let data: AllGameData = match serde_json::from_str(body) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    // A real snapshot always names the mode; anything else is noise.
    if data.game_data.game_mode.is_empty() {
        return Vec::new();
    }

    let mut st = state.lock().expect("lol telemetry state poisoned");
    let mut events = Vec::new();
    let game_end = data.game_end_result();

    // A snapshot without a GameEnd while nothing is tracked = new match
    // (fresh game, or connecting mid-match after a client restart).
    if game_end.is_none() && !st.match_active {
        st.match_active = true;
        st.match_ended = false;
        events.push(TelemetryEvent::MatchStarted {
            mode: data.game_data.game_mode.clone(),
            map: data.game_data.map_name.clone(),
        });
    }

    if let Some(result) = game_end {
        if st.match_active && !st.match_ended {
            st.match_active = false;
            st.match_ended = true;

            let outcome = match result.as_str() {
                "Win" => Outcome::Win,
                "Lose" => Outcome::Loss,
                _ => Outcome::Incomplete,
            };
            let me = data.active_player_entry();
            let (own, opp) = data.team_kills(me.map(|p| p.team.as_str()).unwrap_or(""));
            let performance = me.map(|p| Performance {
                kills: Some(p.scores.kills.max(0) as u32),
                deaths: Some(p.scores.deaths.max(0) as u32),
                assists: Some(p.scores.assists.max(0) as u32),
                cs: Some(p.scores.creep_score.max(0) as u32),
                ..Performance::default()
            });
            let build = me
                .filter(|p| !p.champion_name.is_empty())
                .map(|p| BuildInfo {
                    character: Some(p.champion_name.clone()),
                    ..BuildInfo::default()
                });
            let streak_eligible = !NON_STREAK_MODES.contains(&data.game_data.game_mode.as_str());

            events.push(TelemetryEvent::MatchEnded(Box::new(MatchResult {
                game_id: GAME_ID.to_string(),
                mode: data.game_data.game_mode.clone(),
                map: data.game_data.map_name.clone(),
                result: outcome,
                streak_eligible,
                own_score: own,
                opp_score: opp,
                performance,
                build,
                run: None,
                source: SourceQuality::Live,
                ts: now_ms(),
            })));
        }
    }

    events
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ---------------------------------------------------------------------------
// Live Client Data payload (subset; everything optional/defaulted so partial
// payloads degrade to "no events" rather than errors).
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct AllGameData {
    #[serde(default, rename = "activePlayer")]
    active_player: ActivePlayer,
    #[serde(default, rename = "allPlayers")]
    all_players: Vec<PlayerEntry>,
    #[serde(default)]
    events: EventsBlock,
    #[serde(default, rename = "gameData")]
    game_data: GameData,
}

impl AllGameData {
    /// The `Result` of the GameEnd event ("Win"/"Lose"), if the game ended.
    fn game_end_result(&self) -> Option<String> {
        self.events
            .events
            .iter()
            .find(|e| e.event_name == "GameEnd")
            .map(|e| e.result.clone())
    }

    /// The active player's scoreboard entry, matched by riot id (with the
    /// legacy summoner-name fallback for older patches).
    fn active_player_entry(&self) -> Option<&PlayerEntry> {
        let name = if !self.active_player.riot_id.is_empty() {
            self.active_player.riot_id.as_str()
        } else {
            self.active_player.summoner_name.as_str()
        };
        if name.is_empty() {
            return None;
        }
        self.all_players
            .iter()
            .find(|p| p.riot_id == name || p.summoner_name == name)
    }

    /// Total champion kills per side, oriented to `own_team` ("ORDER"/"CHAOS");
    /// unknown side falls back to (leading, trailing).
    fn team_kills(&self, own_team: &str) -> (u32, u32) {
        let mut order = 0u32;
        let mut chaos = 0u32;
        for p in &self.all_players {
            let k = p.scores.kills.max(0) as u32;
            match p.team.as_str() {
                "ORDER" => order += k,
                "CHAOS" => chaos += k,
                _ => {}
            }
        }
        match own_team {
            "ORDER" => (order, chaos),
            "CHAOS" => (chaos, order),
            _ => (order.max(chaos), order.min(chaos)),
        }
    }
}

#[derive(Deserialize, Default)]
struct ActivePlayer {
    #[serde(default, rename = "riotId")]
    riot_id: String,
    #[serde(default, rename = "summonerName")]
    summoner_name: String,
}

#[derive(Deserialize, Default)]
struct PlayerEntry {
    #[serde(default, rename = "riotId")]
    riot_id: String,
    #[serde(default, rename = "summonerName")]
    summoner_name: String,
    #[serde(default, rename = "championName")]
    champion_name: String,
    #[serde(default)]
    team: String,
    #[serde(default)]
    scores: Scores,
}

#[derive(Deserialize, Default)]
struct Scores {
    #[serde(default)]
    kills: i32,
    #[serde(default)]
    deaths: i32,
    #[serde(default)]
    assists: i32,
    #[serde(default, rename = "creepScore")]
    creep_score: i32,
}

#[derive(Deserialize, Default)]
struct EventsBlock {
    #[serde(default, rename = "Events")]
    events: Vec<LiveEvent>,
}

#[derive(Deserialize, Default)]
struct LiveEvent {
    #[serde(default, rename = "EventName")]
    event_name: String,
    #[serde(default, rename = "Result")]
    result: String,
}

#[derive(Deserialize, Default)]
struct GameData {
    #[serde(default, rename = "gameMode")]
    game_mode: String,
    #[serde(default, rename = "mapName")]
    map_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(game_mode: &str, game_end: Option<&str>) -> String {
        let events = match game_end {
            Some(result) => format!(
                r#"[ {{ "EventID": 0, "EventName": "GameStart", "EventTime": 0 }},
                     {{ "EventID": 30, "EventName": "GameEnd", "EventTime": 1900.5, "Result": "{result}" }} ]"#
            ),
            None => r#"[ { "EventID": 0, "EventName": "GameStart", "EventTime": 0 } ]"#.into(),
        };
        format!(
            r#"{{
                "activePlayer": {{ "riotId": "Mello#EUW", "summonerName": "Mello" }},
                "allPlayers": [
                    {{ "riotId": "Mello#EUW", "summonerName": "Mello", "championName": "Jinx",
                       "team": "ORDER",
                       "scores": {{ "kills": 11, "deaths": 3, "assists": 8, "creepScore": 187 }} }},
                    {{ "riotId": "Foe#EUW", "summonerName": "Foe", "championName": "Ahri",
                       "team": "CHAOS",
                       "scores": {{ "kills": 6, "deaths": 11, "assists": 2, "creepScore": 140 }} }}
                ],
                "events": {{ "Events": {events} }},
                "gameData": {{ "gameMode": "{game_mode}", "mapName": "Map11", "gameTime": 1900.5 }}
            }}"#
        )
    }

    #[test]
    fn match_start_then_win_with_stats() {
        let state = Mutex::new(LolState::default());

        let evs = digest(&state, &snapshot("CLASSIC", None));
        assert!(matches!(evs[0], TelemetryEvent::MatchStarted { .. }));

        // Mid-game polls emit nothing new.
        assert!(digest(&state, &snapshot("CLASSIC", None)).is_empty());

        let evs = digest(&state, &snapshot("CLASSIC", Some("Win")));
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .expect("expected MatchEnded");
        assert_eq!(ended.result, Outcome::Win);
        assert!(ended.streak_eligible);
        assert_eq!(ended.own_score, 11);
        assert_eq!(ended.opp_score, 6);

        let perf = ended.performance.as_ref().expect("expected performance");
        assert_eq!(perf.kills, Some(11));
        assert_eq!(perf.deaths, Some(3));
        assert_eq!(perf.assists, Some(8));
        assert_eq!(perf.cs, Some(187));
        assert_eq!(
            ended.build.as_ref().and_then(|b| b.character.as_deref()),
            Some("Jinx")
        );

        // Trailing post-game polls don't re-emit.
        assert!(digest(&state, &snapshot("CLASSIC", Some("Win"))).is_empty());
    }

    #[test]
    fn loss_result_maps_to_loss() {
        let state = Mutex::new(LolState::default());
        digest(&state, &snapshot("CLASSIC", None));
        let evs = digest(&state, &snapshot("CLASSIC", Some("Lose")));
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .expect("expected MatchEnded");
        assert_eq!(ended.result, Outcome::Loss);
    }

    #[test]
    fn practice_tool_not_streak_eligible() {
        let state = Mutex::new(LolState::default());
        digest(&state, &snapshot("PRACTICETOOL", None));
        let evs = digest(&state, &snapshot("PRACTICETOOL", Some("Win")));
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .expect("expected MatchEnded");
        assert!(!ended.streak_eligible);
    }

    #[test]
    fn garbage_and_empty_payloads_are_quiet() {
        let state = Mutex::new(LolState::default());
        assert!(digest(&state, "not json").is_empty());
        assert!(digest(&state, "{}").is_empty());
    }
}
