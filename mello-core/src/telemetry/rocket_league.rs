//! Rocket League official Stats API adapter (`MatchStatsExporter_TA`).
//!
//! Active source: once enabled via `TAGame/Config/DefaultStatsAPI.ini`, the
//! game listens on a local TCP socket (default `127.0.0.1:49123`) and streams
//! concatenated JSON envelopes (`{"Event": …, "Data": …}`) while a match is
//! in progress. (Psyonix's docs say "web socket", but the wire format is a
//! raw TCP JSON stream.)
//!
//! `start()` spawns a connect-and-read thread; `reset()` stops it. Outcomes
//! come from the `MatchEnded` event (`WinnerTeamNum`) versus the local team,
//! which we infer from the spectator target of non-replay `UpdateState` ticks
//! (in first-person play the client views the local car). Matches without a
//! `MatchGuid` (offline modes) are recorded but not streak-eligible.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use super::{
    GameTelemetryAdapter, MatchResult, Outcome, Performance, SourceQuality, TelemetryError,
    TelemetryEvent,
};

const GAME_ID: &str = "rocket-league";
const STATS_ADDR: &str = "127.0.0.1:49123";
const RECONNECT_INTERVAL_MS: u64 = 3_000;
/// Stop-check granularity while waiting between connection attempts.
const SLEEP_SLICE_MS: u64 = 100;

#[derive(Default)]
struct RlState {
    match_active: bool,
    /// True once the current match came with a MatchGuid (online/LAN).
    online: bool,
    /// Local team index (0/1) inferred from the non-replay spectator target.
    own_team: Option<u8>,
    /// Local player name (the spectator target), for the performance slot.
    own_name: String,
    arena: String,
    team_scores: [u32; 2],
    /// Last stats snapshot of the local player.
    own_stats: Option<PlayerTick>,
}

pub struct RocketLeagueAdapter {
    state: Arc<Mutex<RlState>>,
    running: Arc<AtomicBool>,
}

impl RocketLeagueAdapter {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(RlState::default())),
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Default for RocketLeagueAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GameTelemetryAdapter for RocketLeagueAdapter {
    fn game_id(&self) -> &str {
        GAME_ID
    }

    fn ensure_installed(&self, _token: &str, _port: u16) -> Result<(), TelemetryError> {
        install_ini()
    }

    fn start(&self, tx: Sender<TelemetryEvent>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return; // already running
        }
        *self.state.lock().expect("rl telemetry state poisoned") = RlState::default();

        let running = self.running.clone();
        let state = self.state.clone();
        let spawn = std::thread::Builder::new()
            .name("rl-stats-stream".into())
            .spawn(move || stream_loop(&running, &state, &tx));
        match spawn {
            Ok(_) => log::info!("[telemetry] rocket league stats stream started"),
            Err(e) => {
                log::warn!("[telemetry] rl stats thread failed to spawn: {e}");
                self.running.store(false, Ordering::SeqCst);
            }
        }
    }

    fn reset(&self) {
        self.running.store(false, Ordering::SeqCst);
        *self.state.lock().expect("rl telemetry state poisoned") = RlState::default();
    }
}

fn stream_loop(running: &AtomicBool, state: &Mutex<RlState>, tx: &Sender<TelemetryEvent>) {
    while running.load(Ordering::SeqCst) {
        match std::net::TcpStream::connect(STATS_ADDR) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
                let reader = std::io::BufReader::new(stream);
                let mut de = serde_json::Deserializer::from_reader(reader).into_iter::<Envelope>();
                for envelope in de.by_ref() {
                    if !running.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(envelope) = envelope else {
                        break; // framing lost or socket dropped; reconnect
                    };
                    for ev in digest(state, &envelope) {
                        if tx.send(ev).is_err() {
                            log::info!("[telemetry] rl stats receiver dropped, stopping");
                            running.store(false, Ordering::SeqCst);
                            return;
                        }
                    }
                }
                // Socket closed (game exited or match feature idle). A match
                // still marked active at this point was torn down without a
                // MatchEnded/MatchDestroyed → finalize as Incomplete.
                if let Some(ev) = finalize_incomplete(state) {
                    if tx.send(ev).is_err() {
                        running.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            }
            Err(_) => {
                // Not listening (feature disabled or game still booting);
                // retry quietly until reset().
            }
        }

        let mut slept = 0;
        while slept < RECONNECT_INTERVAL_MS {
            if !running.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(SLEEP_SLICE_MS));
            slept += SLEEP_SLICE_MS;
        }
    }
}

/// Digest one Stats API envelope into telemetry events. Pure state machine so
/// tests can drive it without a socket.
fn digest(state: &Mutex<RlState>, envelope: &Envelope) -> Vec<TelemetryEvent> {
    let mut st = state.lock().expect("rl telemetry state poisoned");
    match envelope.event.as_str() {
        "MatchCreated" => {
            // Teams replicated; a fresh match is about to run. Prime the
            // state — the first UpdateState tick emits MatchStarted.
            *st = RlState::default();
            st.online = !envelope.match_guid().is_empty();
            Vec::new()
        }
        "UpdateState" => {
            let Ok(data) = serde_json::from_value::<UpdateState>(envelope.data.clone()) else {
                return Vec::new();
            };
            let mut events = Vec::new();

            // First tick starts tracking; also covers connecting mid-match
            // (client restart) when no MatchCreated was seen.
            let first_tick = !st.match_active;
            if first_tick {
                st.match_active = true;
                if !envelope.match_guid().is_empty() {
                    st.online = true;
                }
            }
            if !data.game.arena.is_empty() {
                st.arena = data.game.arena.clone();
            }
            if first_tick {
                events.push(TelemetryEvent::MatchStarted {
                    mode: "match".to_string(),
                    map: st.arena.clone(),
                });
            }

            // The spectator target of a non-replay tick is the local car in
            // first-person play; goal replays may retarget the scorer.
            if !data.game.b_replay {
                if let Some(target) = &data.game.target {
                    if !target.name.is_empty() {
                        st.own_team = Some(target.team_num);
                        st.own_name = target.name.clone();
                    }
                }
            }
            if let Some(me) = data.players.iter().find(|p| p.name == st.own_name) {
                st.own_stats = Some(me.clone());
            }

            // Team goal totals, oriented once we know our side.
            let mut scores = [0u32; 2];
            for team in &data.game.teams {
                if let Some(slot) = scores.get_mut(team.team_num as usize) {
                    *slot = team.score.max(0) as u32;
                }
            }
            if scores != st.team_scores {
                st.team_scores = scores;
                let (own, opp) = oriented_scores(&st);
                events.push(TelemetryEvent::ScoreChanged { own, opp });
            }
            events
        }
        "MatchEnded" => {
            if !st.match_active {
                return Vec::new();
            }
            st.match_active = false;
            let Ok(data) = serde_json::from_value::<MatchEndedData>(envelope.data.clone()) else {
                return Vec::new();
            };
            let result = match st.own_team {
                Some(own) => {
                    if u8::try_from(data.winner_team_num).ok() == Some(own) {
                        Outcome::Win
                    } else {
                        Outcome::Loss
                    }
                }
                None => Outcome::Incomplete, // never saw our side → can't attribute
            };
            let (own, opp) = oriented_scores(&st);
            let performance = st.own_stats.as_ref().map(|p| Performance {
                goals: Some(p.goals.max(0) as u32),
                assists: Some(p.assists.max(0) as u32),
                saves: Some(p.saves.max(0) as u32),
                shots: Some(p.shots.max(0) as u32),
                score: Some(p.score.max(0) as u32),
                ..Performance::default()
            });
            vec![TelemetryEvent::MatchEnded(Box::new(MatchResult {
                game_id: GAME_ID.to_string(),
                mode: "match".to_string(),
                map: st.arena.clone(),
                result,
                // Playlist isn't broadcast; online/LAN (MatchGuid set) counts,
                // offline modes (exhibition, training) don't.
                streak_eligible: st.online,
                own_score: own,
                opp_score: opp,
                performance,
                build: None,
                run: None,
                source: SourceQuality::Live,
                ts: now_ms(),
            }))]
        }
        "MatchDestroyed" => {
            // Left the game. If the match never reached MatchEnded, it was
            // abandoned → Incomplete.
            let ev = finalize_incomplete_locked(&mut st);
            ev.into_iter().collect()
        }
        _ => Vec::new(),
    }
}

/// Finalize an in-flight match as Incomplete (socket drop / abandon).
fn finalize_incomplete(state: &Mutex<RlState>) -> Option<TelemetryEvent> {
    let mut st = state.lock().expect("rl telemetry state poisoned");
    finalize_incomplete_locked(&mut st)
}

fn finalize_incomplete_locked(st: &mut RlState) -> Option<TelemetryEvent> {
    if !st.match_active {
        return None;
    }
    st.match_active = false;
    let arena = std::mem::take(&mut st.arena);
    let online = st.online;
    *st = RlState::default();
    Some(TelemetryEvent::MatchEnded(Box::new(MatchResult {
        game_id: GAME_ID.to_string(),
        mode: "match".to_string(),
        map: arena,
        result: Outcome::Incomplete,
        streak_eligible: online,
        own_score: 0,
        opp_score: 0,
        performance: None,
        build: None,
        run: None,
        source: SourceQuality::Live,
        ts: now_ms(),
    })))
}

/// Orient team scores to the local side; unknown side falls back to
/// (leading, trailing) so the numbers stay meaningful for display.
fn oriented_scores(st: &RlState) -> (u32, u32) {
    let [blue, orange] = st.team_scores;
    match st.own_team {
        Some(0) => (blue, orange),
        Some(1) => (orange, blue),
        _ => (blue.max(orange), blue.min(orange)),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ---------------------------------------------------------------------------
// Stats API payloads (subset; everything optional/defaulted).
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default, Debug)]
struct Envelope {
    #[serde(default, rename = "Event")]
    event: String,
    #[serde(default, rename = "Data")]
    data: serde_json::Value,
}

impl Envelope {
    /// `MatchGuid` from the payload; empty for offline matches.
    fn match_guid(&self) -> &str {
        self.data
            .get("MatchGuid")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }
}

#[derive(Deserialize, Default)]
struct UpdateState {
    #[serde(default, rename = "Players")]
    players: Vec<PlayerTick>,
    #[serde(default, rename = "Game")]
    game: GameTick,
}

#[derive(Deserialize, Default, Clone)]
struct PlayerTick {
    #[serde(default, rename = "Name")]
    name: String,
    #[serde(default, rename = "Score")]
    score: i32,
    #[serde(default, rename = "Goals")]
    goals: i32,
    #[serde(default, rename = "Shots")]
    shots: i32,
    #[serde(default, rename = "Assists")]
    assists: i32,
    #[serde(default, rename = "Saves")]
    saves: i32,
}

#[derive(Deserialize, Default)]
struct GameTick {
    #[serde(default, rename = "Teams")]
    teams: Vec<TeamTick>,
    #[serde(default, rename = "Arena")]
    arena: String,
    #[serde(default, rename = "bReplay")]
    b_replay: bool,
    #[serde(default, rename = "Target")]
    target: Option<TargetTick>,
}

#[derive(Deserialize, Default)]
struct TeamTick {
    #[serde(default, rename = "TeamNum")]
    team_num: u8,
    #[serde(default, rename = "Score")]
    score: i32,
}

#[derive(Deserialize, Default)]
struct TargetTick {
    #[serde(default, rename = "Name")]
    name: String,
    #[serde(default, rename = "TeamNum")]
    team_num: u8,
}

#[derive(Deserialize, Default)]
struct MatchEndedData {
    #[serde(default, rename = "WinnerTeamNum")]
    winner_team_num: i64,
}

// ---------------------------------------------------------------------------
// Config installation (Windows/Steam-first, mirroring the GSI adapters).
// ---------------------------------------------------------------------------

/// The Stats API ini. `PacketSendRate` must be > 0 to enable the socket; we
/// keep it low — match events fire on their own tick regardless of the rate.
#[cfg(windows)]
const STATS_INI: &str = "[TAGame.MatchStatsExporter_TA]\nPort=49123\nPacketSendRate=10\n";

#[cfg(windows)]
fn install_ini() -> Result<(), TelemetryError> {
    let cfg_dir = super::steam::find_app_subdir(
        "rocketleague",
        &["TAGame", "Config"],
        "Rocket League install not found in any Steam library",
    )?;
    let ini_path = cfg_dir.join("DefaultStatsAPI.ini");

    // Idempotent, and never clobber a user's own Stats API config: only write
    // when the file is missing or was written by us with different contents.
    let current = std::fs::read_to_string(&ini_path).unwrap_or_default();
    if current.contains("MatchStatsExporter_TA") {
        log::debug!("[telemetry] RL stats ini already configured");
        return Ok(());
    }
    std::fs::write(&ini_path, STATS_INI)?;
    log::info!(
        "[telemetry] installed RL stats ini at {}",
        ini_path.display()
    );
    Ok(())
}

#[cfg(not(windows))]
fn install_ini() -> Result<(), TelemetryError> {
    Err(TelemetryError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(json: &str) -> Envelope {
        serde_json::from_str(json).expect("test envelope")
    }

    fn update_state(blue: u32, orange: u32, target_team: u8, replay: bool) -> Envelope {
        env(&format!(
            r#"{{
                "Event": "UpdateState",
                "Data": {{
                    "MatchGuid": "GUID123",
                    "Players": [
                        {{ "Name": "Mello", "TeamNum": {target_team}, "Score": 350,
                           "Goals": 2, "Shots": 5, "Assists": 1, "Saves": 3 }},
                        {{ "Name": "Rival", "TeamNum": 1, "Score": 210,
                           "Goals": 1, "Shots": 3, "Assists": 0, "Saves": 1 }}
                    ],
                    "Game": {{
                        "Teams": [
                            {{ "Name": "Blue", "TeamNum": 0, "Score": {blue} }},
                            {{ "Name": "Orange", "TeamNum": 1, "Score": {orange} }}
                        ],
                        "Arena": "Stadium_P",
                        "bReplay": {replay},
                        "bHasTarget": true,
                        "Target": {{ "Name": "Mello", "TeamNum": {target_team} }}
                    }}
                }}
            }}"#
        ))
    }

    #[test]
    fn full_match_win_flow() {
        let state = Mutex::new(RlState::default());

        let evs = digest(
            &state,
            &env(r#"{ "Event": "MatchCreated", "Data": { "MatchGuid": "GUID123" } }"#),
        );
        assert!(evs.is_empty());

        // First tick starts the match and reports the score.
        let evs = digest(&state, &update_state(0, 0, 0, false));
        assert!(matches!(evs[0], TelemetryEvent::MatchStarted { .. }));

        // Goal for blue (us).
        let evs = digest(&state, &update_state(1, 0, 0, false));
        assert!(evs
            .iter()
            .any(|e| matches!(e, TelemetryEvent::ScoreChanged { own: 1, opp: 0 })));

        let evs = digest(
            &state,
            &env(
                r#"{ "Event": "MatchEnded", "Data": { "MatchGuid": "GUID123", "WinnerTeamNum": 0 } }"#,
            ),
        );
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .expect("expected MatchEnded");
        assert_eq!(ended.result, Outcome::Win);
        assert!(ended.streak_eligible);
        assert_eq!(ended.own_score, 1);
        assert_eq!(ended.opp_score, 0);
        assert_eq!(ended.map, "Stadium_P");

        let perf = ended.performance.as_ref().expect("expected performance");
        assert_eq!(perf.goals, Some(2));
        assert_eq!(perf.saves, Some(3));
        assert_eq!(perf.shots, Some(5));
        assert_eq!(perf.score, Some(350));
    }

    #[test]
    fn loss_when_other_team_wins() {
        let state = Mutex::new(RlState::default());
        digest(&state, &update_state(0, 2, 0, false));
        let evs = digest(
            &state,
            &env(r#"{ "Event": "MatchEnded", "Data": { "WinnerTeamNum": 1 } }"#),
        );
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .expect("expected MatchEnded");
        assert_eq!(ended.result, Outcome::Loss);
        assert_eq!(ended.own_score, 0);
        assert_eq!(ended.opp_score, 2);
    }

    #[test]
    fn replay_ticks_dont_retarget_local_player() {
        let state = Mutex::new(RlState::default());
        digest(&state, &update_state(0, 0, 0, false));
        // Goal replay targets the scorer on the other team; must not flip us.
        digest(&state, &update_state(0, 1, 1, true));
        assert_eq!(
            state.lock().unwrap().own_team,
            Some(0),
            "replay tick must not change the inferred local team"
        );
    }

    #[test]
    fn abandon_finalizes_incomplete() {
        let state = Mutex::new(RlState::default());
        digest(&state, &update_state(2, 2, 0, false));
        let evs = digest(&state, &env(r#"{ "Event": "MatchDestroyed", "Data": {} }"#));
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .expect("expected Incomplete MatchEnded");
        assert_eq!(ended.result, Outcome::Incomplete);
        // Repeated destroys are quiet.
        assert!(digest(&state, &env(r#"{ "Event": "MatchDestroyed", "Data": {} }"#)).is_empty());
    }

    #[test]
    fn offline_match_not_streak_eligible() {
        let state = Mutex::new(RlState::default());
        // No MatchGuid → offline (exhibition/training).
        let offline = env(r#"{
                "Event": "UpdateState",
                "Data": {
                    "Players": [],
                    "Game": {
                        "Teams": [
                            { "TeamNum": 0, "Score": 0 },
                            { "TeamNum": 1, "Score": 0 }
                        ],
                        "Arena": "Stadium_P",
                        "bReplay": false,
                        "Target": { "Name": "Mello", "TeamNum": 0 }
                    }
                }
            }"#);
        digest(&state, &offline);
        let evs = digest(
            &state,
            &env(r#"{ "Event": "MatchEnded", "Data": { "WinnerTeamNum": 0 } }"#),
        );
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .expect("expected MatchEnded");
        assert_eq!(ended.result, Outcome::Win);
        assert!(!ended.streak_eligible);
    }

    #[test]
    fn unknown_events_are_quiet() {
        let state = Mutex::new(RlState::default());
        assert!(digest(&state, &env(r#"{ "Event": "BallHit", "Data": {} }"#)).is_empty());
        assert!(digest(&state, &env(r#"{ "Event": "Nonsense", "Data": null }"#)).is_empty());
    }
}
