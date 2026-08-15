use crate::events::Event;
use crate::game_sensing::{ActiveGame, GameEvent};
use crate::telemetry::{MatchResult, Outcome, TelemetryEvent};

const MIN_SESSION_LEDGER_MIN: u32 = 2;
/// Used by the UI handler to decide whether to show post-game prompt.
pub const MIN_SESSION_POSTGAME_MIN: u32 = 5;

/// One open game session. v1 kept these three fields loose on the manager,
/// which is why only one game could be tracked at a time.
struct GameSession {
    game: ActiveGame,
    /// Match outcomes accumulated this session (from a telemetry adapter).
    matches: Vec<MatchResult>,
}

#[derive(Default)]
pub struct GameStateManager {
    /// Every open session, keyed by pid — two games can run at once.
    sessions: std::collections::HashMap<u32, GameSession>,
    /// The pid the user is actually looking at; drives presence and the bar.
    primary: Option<u32>,
}

impl GameStateManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process a game event from the sensor and return UI events to emit
    /// plus an optional session summary for the `game_session_end` RPC.
    pub fn handle_event(&mut self, event: GameEvent) -> (Vec<Event>, Option<SessionSummary>) {
        let mut events = Vec::new();
        let mut session_end = None;

        match event {
            GameEvent::Started(game) => {
                log::info!(
                    "[game-state] game started: {} ({}, pid={}, since {})",
                    game.game_name,
                    game.game_id,
                    game.pid,
                    game.started_at
                );
                let pid = game.pid;
                events.push(Event::GameDetected {
                    game_id: game.game_id.clone(),
                    game_name: game.game_name.clone(),
                    short_name: game.short_name.clone(),
                    color: game.color.clone(),
                    pid,
                });
                self.sessions.insert(
                    pid,
                    GameSession {
                        game,
                        matches: Vec::new(),
                    },
                );
                // First game in becomes primary until the sensor says otherwise,
                // so a single-game session never waits for a PrimaryChanged.
                if self.primary.is_none() {
                    self.primary = Some(pid);
                }
            }
            GameEvent::Stopped { game, ended_at } => {
                let Some(mut session) = self.sessions.remove(&game.pid) else {
                    log::debug!(
                        "[game-state] stop for untracked pid {} ({}); ignoring",
                        game.pid,
                        game.game_name
                    );
                    return (events, session_end);
                };
                if self.primary == Some(game.pid) {
                    self.primary = self.sessions.keys().copied().next();
                }

                // Duration spans the real process lifetime, not the window we
                // happened to be watching. `ended_at` is when it was last seen
                // alive — for a session recovered after a restart that is well
                // before now, and using now would invent hours that never
                // happened.
                let duration_min = ((ended_at - session.game.started_at).max(0) / 60_000) as u32;
                let (wins, losses, draws) = tally(&session.matches);

                log::info!(
                    "[game-state] game stopped: {} (duration={}min, foreground={}min, {}W-{}L-{}D over {} matches)",
                    session.game.game_name,
                    duration_min,
                    session.game.foreground_ms / 60_000,
                    wins,
                    losses,
                    draws,
                    session.matches.len(),
                );

                if duration_min >= MIN_SESSION_LEDGER_MIN {
                    session_end = Some(SessionSummary {
                        game_name: session.game.game_name.clone(),
                        game_id: session.game.game_id.clone(),
                        duration_min,
                        wins,
                        losses,
                        draws,
                        matches: std::mem::take(&mut session.matches),
                    });
                }

                events.push(Event::GameEnded {
                    game_id: session.game.game_id.clone(),
                    game_name: session.game.game_name.clone(),
                    short_name: session.game.short_name.clone(),
                    duration_min,
                });
            }
            GameEvent::PrimaryChanged { pid } => {
                self.primary = pid;
            }
            // Handled by the client loop (confirm prompt); never reaches here.
            GameEvent::UnknownCandidate { .. } => {}
        }

        (events, session_end)
    }

    /// Process a telemetry event from an adapter (e.g. CS2 GSI). Accumulates
    /// match outcomes into the matching session and returns any live UI events.
    pub fn handle_telemetry(&mut self, event: TelemetryEvent) -> Vec<Event> {
        match event {
            TelemetryEvent::MatchEnded(m) => {
                // Attribute by game_id rather than "whatever is current" — with
                // two games open, the wrong session would otherwise absorb the
                // result.
                let Some(session) = self
                    .sessions
                    .values_mut()
                    .find(|s| s.game.game_id == m.game_id)
                else {
                    log::debug!(
                        "[game-state] telemetry for {} with no matching session; ignoring",
                        m.game_id
                    );
                    return Vec::new();
                };
                log::info!(
                    "[game-state] match ended: {} {}-{} on {}",
                    m.result.as_str(),
                    m.own_score,
                    m.opp_score,
                    m.map
                );
                let ev = Event::MatchEnded {
                    result: m.result.as_str().to_string(),
                    own_score: m.own_score,
                    opp_score: m.opp_score,
                    map: m.map.clone(),
                };
                session.matches.push(*m);
                vec![ev]
            }
            // Match start / score changes are tracked by the adapter; no UI
            // event yet (reserved for live HUD score and future auto-clip hooks).
            TelemetryEvent::MatchStarted { .. } | TelemetryEvent::ScoreChanged { .. } => Vec::new(),
            TelemetryEvent::SetupRequired { game_id, hint } => {
                log::info!("[game-state] telemetry setup required for {game_id}: {hint}");
                vec![Event::TelemetrySetupHint { game_id, hint }]
            }
        }
    }

    /// The game the user is looking at, for presence and the NOW PLAYING bar.
    pub fn current_game(&self) -> Option<&ActiveGame> {
        self.primary
            .and_then(|pid| self.sessions.get(&pid))
            .map(|s| &s.game)
    }

    /// True while any game is running, regardless of which has focus.
    pub fn any_active(&self) -> bool {
        !self.sessions.is_empty()
    }
}

/// Outcome summary for a finished gaming session, fed to `game_session_end`.
pub struct SessionSummary {
    pub game_name: String,
    pub game_id: String,
    pub duration_min: u32,
    /// Decisive (streak-eligible) wins/losses this session.
    pub wins: u32,
    pub losses: u32,
    /// Drawn matches — recorded but don't move the streak.
    pub draws: u32,
    pub matches: Vec<MatchResult>,
}

/// Count streak-eligible wins/losses/draws. Wins/losses move the record;
/// draws are recorded but don't move the streak; incompletes (crash/disconnect)
/// and non-streak-mode results (played only) are ignored entirely.
fn tally(matches: &[MatchResult]) -> (u32, u32, u32) {
    let mut wins = 0;
    let mut losses = 0;
    let mut draws = 0;
    for m in matches {
        if !m.streak_eligible {
            continue;
        }
        match m.result {
            Outcome::Win => wins += 1,
            Outcome::Loss => losses += 1,
            Outcome::Draw => draws += 1,
            Outcome::Incomplete => {}
        }
    }
    (wins, losses, draws)
}

/// Session timing now comes from the sensor's real process timestamps, so the
/// only remaining caller is the test fixture below.
#[cfg(test)]
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game_sensing::ActiveGame;

    const START: i64 = 1_700_000_000_000;

    fn game_at(pid: u32, game_id: &str, started_at: i64) -> ActiveGame {
        ActiveGame {
            game_id: game_id.into(),
            game_name: format!("Game {game_id}"),
            short_name: "G".into(),
            color: "#DE9B35".into(),
            exe: format!("{game_id}.exe"),
            pid,
            igdb_id: None,
            started_at,
            started_at_ms: started_at,
            foreground_ms: 0,
        }
    }

    fn test_game() -> ActiveGame {
        ActiveGame {
            game_id: "counter-strike-2".into(),
            game_name: "Counter-Strike 2".into(),
            short_name: "CS2".into(),
            color: "#DE9B35".into(),
            exe: "cs2.exe".into(),
            pid: 1234,
            igdb_id: Some(242408),
            started_at: START,
            started_at_ms: START,
            foreground_ms: 0,
        }
    }

    /// Stop `game` `minutes` after it started.
    fn stop_after(game: &ActiveGame, minutes: i64) -> GameEvent {
        GameEvent::Stopped {
            game: Box::new(game.clone()),
            ended_at: game.started_at + minutes * 60_000,
        }
    }

    fn match_result(result: Outcome) -> Box<MatchResult> {
        match_result_for("counter-strike-2", result)
    }

    fn match_result_for(game_id: &str, result: Outcome) -> Box<MatchResult> {
        Box::new(MatchResult {
            game_id: game_id.into(),
            mode: "competitive".into(),
            map: "de_mirage".into(),
            result,
            streak_eligible: true,
            own_score: 13,
            opp_score: 7,
            performance: None,
            build: None,
            run: None,
            source: crate::telemetry::SourceQuality::Live,
            ts: now_ms(),
        })
    }

    #[test]
    fn start_emits_detected() {
        let mut mgr = GameStateManager::new();
        let (events, session_end) = mgr.handle_event(GameEvent::Started(test_game()));
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], Event::GameDetected { game_id, .. } if game_id == "counter-strike-2")
        );
        assert!(session_end.is_none());
        assert!(mgr.current_game().is_some());
    }

    #[test]
    fn stop_short_session_no_ledger() {
        let mut mgr = GameStateManager::new();
        let game = test_game();
        mgr.handle_event(GameEvent::Started(game.clone()));
        let (events, session_end) = mgr.handle_event(stop_after(&game, 1));
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], Event::GameEnded { duration_min, .. } if *duration_min < MIN_SESSION_LEDGER_MIN)
        );
        assert!(session_end.is_none());
        assert!(mgr.current_game().is_none());
    }

    #[test]
    fn duration_spans_the_real_process_lifetime() {
        // The whole point of the libmello start-time change: a game running
        // for four hours before Mello noticed must report four hours, not the
        // few minutes we happened to be watching.
        let mut mgr = GameStateManager::new();
        let game = game_at(1, "counter-strike-2", START);
        mgr.handle_event(GameEvent::Started(game.clone()));
        let (events, summary) = mgr.handle_event(stop_after(&game, 240));
        assert!(
            matches!(&events[0], Event::GameEnded { duration_min, .. } if *duration_min == 240)
        );
        assert_eq!(summary.expect("ledger-worthy").duration_min, 240);
    }

    #[test]
    fn duration_uses_ended_at_not_wall_clock() {
        // A session recovered after a client restart ends when the process was
        // last seen. Using "now" would invent the entire downtime as playtime.
        let mut mgr = GameStateManager::new();
        let game = game_at(1, "counter-strike-2", START);
        mgr.handle_event(GameEvent::Started(game.clone()));
        let (_e, summary) = mgr.handle_event(GameEvent::Stopped {
            game: Box::new(game),
            ended_at: START + 20 * 60_000,
        });
        assert_eq!(summary.expect("ledger-worthy").duration_min, 20);
    }

    #[test]
    fn negative_span_clamps_to_zero() {
        // A clock adjustment mid-session must not produce a huge u32 duration.
        let mut mgr = GameStateManager::new();
        let game = game_at(1, "counter-strike-2", START);
        mgr.handle_event(GameEvent::Started(game.clone()));
        let (events, summary) = mgr.handle_event(GameEvent::Stopped {
            game: Box::new(game),
            ended_at: START - 60 * 60_000,
        });
        assert!(matches!(&events[0], Event::GameEnded { duration_min, .. } if *duration_min == 0));
        assert!(summary.is_none());
    }

    #[test]
    fn two_games_hold_independent_sessions() {
        // v1 kept a single Option, so starting a second game silently replaced
        // the first and its session was never reported.
        let mut mgr = GameStateManager::new();
        let cs = game_at(1, "counter-strike-2", START);
        let dota = game_at(2, "dota-2", START);
        mgr.handle_event(GameEvent::Started(cs.clone()));
        mgr.handle_event(GameEvent::Started(dota.clone()));

        let (_e, cs_summary) = mgr.handle_event(stop_after(&cs, 30));
        assert_eq!(cs_summary.expect("cs session").game_id, "counter-strike-2");
        // Dota is still running.
        assert!(mgr.any_active());

        let (_e, dota_summary) = mgr.handle_event(stop_after(&dota, 45));
        let dota_summary = dota_summary.expect("dota session");
        assert_eq!(dota_summary.game_id, "dota-2");
        assert_eq!(dota_summary.duration_min, 45);
        assert!(!mgr.any_active());
    }

    #[test]
    fn telemetry_is_attributed_by_game_id() {
        // With two games open, a result must land on its own session rather
        // than on whichever happened to be "current".
        let mut mgr = GameStateManager::new();
        let cs = game_at(1, "counter-strike-2", START);
        let dota = game_at(2, "dota-2", START);
        mgr.handle_event(GameEvent::Started(cs.clone()));
        mgr.handle_event(GameEvent::Started(dota.clone()));

        mgr.handle_telemetry(TelemetryEvent::MatchEnded(match_result_for(
            "dota-2",
            Outcome::Win,
        )));

        let (_e, cs_summary) = mgr.handle_event(stop_after(&cs, 30));
        assert_eq!(cs_summary.expect("cs session").matches.len(), 0);
        let (_e, dota_summary) = mgr.handle_event(stop_after(&dota, 30));
        let dota_summary = dota_summary.expect("dota session");
        assert_eq!(dota_summary.matches.len(), 1);
        assert_eq!(dota_summary.wins, 1);
    }

    #[test]
    fn primary_follows_the_sensor() {
        let mut mgr = GameStateManager::new();
        let cs = game_at(1, "counter-strike-2", START);
        let dota = game_at(2, "dota-2", START);
        mgr.handle_event(GameEvent::Started(cs.clone()));
        // First game in is primary until told otherwise.
        assert_eq!(mgr.current_game().unwrap().game_id, "counter-strike-2");

        mgr.handle_event(GameEvent::Started(dota));
        mgr.handle_event(GameEvent::PrimaryChanged { pid: Some(2) });
        assert_eq!(mgr.current_game().unwrap().game_id, "dota-2");

        // Losing the primary game promotes a survivor rather than going blank
        // while another game is still running.
        mgr.handle_event(GameEvent::Stopped {
            game: Box::new(game_at(2, "dota-2", START)),
            ended_at: START + 60_000,
        });
        assert_eq!(mgr.current_game().unwrap().game_id, "counter-strike-2");
    }

    #[test]
    fn telemetry_accumulates_into_summary() {
        let mut mgr = GameStateManager::new();
        let game = test_game();
        mgr.handle_event(GameEvent::Started(game.clone()));

        // Four matches: 2 wins, 1 loss, 1 draw (draw recorded but not in W/L).
        let ui = mgr.handle_telemetry(TelemetryEvent::MatchEnded(match_result(Outcome::Win)));
        assert!(matches!(&ui[0], Event::MatchEnded { result, .. } if result == "win"));
        mgr.handle_telemetry(TelemetryEvent::MatchEnded(match_result(Outcome::Loss)));
        mgr.handle_telemetry(TelemetryEvent::MatchEnded(match_result(Outcome::Win)));
        mgr.handle_telemetry(TelemetryEvent::MatchEnded(match_result(Outcome::Draw)));

        let (_events, session_end) = mgr.handle_event(stop_after(&game, 30));
        let summary = session_end.expect("expected a session summary");
        assert_eq!(summary.wins, 2);
        assert_eq!(summary.losses, 1);
        assert_eq!(summary.draws, 1);
        assert_eq!(summary.matches.len(), 4);
        assert_eq!(summary.game_id, "counter-strike-2");
    }

    #[test]
    fn played_only_results_dont_move_record() {
        let mut mgr = GameStateManager::new();
        let game = test_game();
        mgr.handle_event(GameEvent::Started(game.clone()));

        let mut casual_win = match_result(Outcome::Win);
        casual_win.streak_eligible = false;
        mgr.handle_telemetry(TelemetryEvent::MatchEnded(casual_win));
        mgr.handle_telemetry(TelemetryEvent::MatchEnded(match_result(Outcome::Win)));

        let (_e, session_end) = mgr.handle_event(stop_after(&game, 30));
        let summary = session_end.expect("expected a session summary");
        // Only the streak-eligible win counts; the casual one is played-only.
        assert_eq!(summary.wins, 1);
        assert_eq!(summary.losses, 0);
        assert_eq!(summary.matches.len(), 2);
    }

    #[test]
    fn telemetry_ignored_without_active_game() {
        let mut mgr = GameStateManager::new();
        let ui = mgr.handle_telemetry(TelemetryEvent::MatchEnded(match_result(Outcome::Win)));
        assert!(ui.is_empty());
    }

    #[test]
    fn stop_for_untracked_pid_is_ignored() {
        let mut mgr = GameStateManager::new();
        let (events, summary) = mgr.handle_event(stop_after(&test_game(), 30));
        assert!(events.is_empty());
        assert!(summary.is_none());
    }

    #[test]
    fn matches_reset_between_sessions() {
        let mut mgr = GameStateManager::new();
        let game = test_game();
        mgr.handle_event(GameEvent::Started(game.clone()));
        mgr.handle_telemetry(TelemetryEvent::MatchEnded(match_result(Outcome::Win)));
        mgr.handle_event(stop_after(&game, 30));

        // New session starts clean.
        mgr.handle_event(GameEvent::Started(game.clone()));
        let (_e, session_end) = mgr.handle_event(stop_after(&game, 30));
        let summary = session_end.unwrap();
        assert_eq!(summary.wins, 0);
        assert_eq!(summary.matches.len(), 0);
    }

    #[test]
    fn postgame_threshold() {
        const { assert!(MIN_SESSION_POSTGAME_MIN > MIN_SESSION_LEDGER_MIN) };
        assert_eq!(MIN_SESSION_POSTGAME_MIN, 5);
        assert_eq!(MIN_SESSION_LEDGER_MIN, 2);
    }
}
