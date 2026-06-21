use crate::events::Event;
use crate::game_sensing::{ActiveGame, GameEvent};
use crate::telemetry::{MatchResult, Outcome, TelemetryEvent};

const MIN_SESSION_LEDGER_MIN: u32 = 2;
/// Used by the UI handler to decide whether to show post-game prompt.
pub const MIN_SESSION_POSTGAME_MIN: u32 = 5;

#[derive(Default)]
pub struct GameStateManager {
    current_game: Option<ActiveGame>,
    session_start: Option<i64>,
    /// Match outcomes accumulated this session (from a telemetry adapter).
    matches: Vec<MatchResult>,
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
                    "[game-state] game started: {} ({})",
                    game.game_name,
                    game.game_id
                );
                self.session_start = Some(now_ms());
                self.matches.clear();
                self.current_game = Some(game.clone());

                events.push(Event::GameDetected {
                    game_id: game.game_id,
                    game_name: game.game_name,
                    short_name: game.short_name,
                    color: game.color,
                    pid: game.pid,
                });
            }
            GameEvent::Stopped(game) => {
                let duration_min = self
                    .session_start
                    .map(|s| ((now_ms() - s) / 60_000) as u32)
                    .unwrap_or(0);

                let (wins, losses, draws) = tally(&self.matches);

                log::info!(
                    "[game-state] game stopped: {} (duration={}min, {}W-{}L-{}D over {} matches)",
                    game.game_name,
                    duration_min,
                    wins,
                    losses,
                    draws,
                    self.matches.len(),
                );

                if duration_min >= MIN_SESSION_LEDGER_MIN {
                    session_end = Some(SessionSummary {
                        game_name: game.game_name.clone(),
                        game_id: game.game_id.clone(),
                        duration_min,
                        wins,
                        losses,
                        draws,
                        matches: std::mem::take(&mut self.matches),
                    });
                }

                self.current_game = None;
                self.session_start = None;
                self.matches.clear();

                events.push(Event::GameEnded {
                    game_id: game.game_id,
                    game_name: game.game_name,
                    short_name: game.short_name,
                    duration_min,
                });
            }
        }

        (events, session_end)
    }

    /// Process a telemetry event from an adapter (e.g. CS2 GSI). Accumulates
    /// match outcomes into the current session and returns any live UI events.
    pub fn handle_telemetry(&mut self, event: TelemetryEvent) -> Vec<Event> {
        match event {
            TelemetryEvent::MatchEnded(m) => {
                if self.current_game.is_none() {
                    log::debug!("[game-state] telemetry match ended with no active game; ignoring");
                    return Vec::new();
                }
                log::info!(
                    "[game-state] match ended: {} {}-{} on {}",
                    m.result.as_str(),
                    m.rounds_won,
                    m.rounds_lost,
                    m.map
                );
                let ev = Event::MatchEnded {
                    result: m.result.as_str().to_string(),
                    rounds_won: m.rounds_won,
                    rounds_lost: m.rounds_lost,
                    map: m.map.clone(),
                };
                self.matches.push(m);
                vec![ev]
            }
            // Match start / round resolution are tracked by the adapter; no UI
            // event yet (reserved for live HUD score and future auto-clip hooks).
            TelemetryEvent::MatchStarted { .. } | TelemetryEvent::RoundEnded { .. } => Vec::new(),
        }
    }

    pub fn current_game(&self) -> Option<&ActiveGame> {
        self.current_game.as_ref()
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

/// Count wins/losses/draws. Wins/losses move the record; draws are recorded but
/// don't move the streak; incompletes (crash/disconnect) are ignored entirely.
fn tally(matches: &[MatchResult]) -> (u32, u32, u32) {
    let mut wins = 0;
    let mut losses = 0;
    let mut draws = 0;
    for m in matches {
        match m.result {
            Outcome::Win => wins += 1,
            Outcome::Loss => losses += 1,
            Outcome::Draw => draws += 1,
            Outcome::Incomplete => {}
        }
    }
    (wins, losses, draws)
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
    use crate::game_sensing::ActiveGame;

    fn test_game() -> ActiveGame {
        ActiveGame {
            game_id: "counter-strike-2".into(),
            game_name: "Counter-Strike 2".into(),
            short_name: "CS2".into(),
            color: "#DE9B35".into(),
            exe: "cs2.exe".into(),
            pid: 1234,
            started_at: now_ms(),
        }
    }

    fn match_result(result: Outcome) -> MatchResult {
        MatchResult {
            game_id: "counter-strike-2".into(),
            mode: "competitive".into(),
            map: "de_mirage".into(),
            result,
            rounds_won: 13,
            rounds_lost: 7,
            ts: now_ms(),
        }
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
        mgr.handle_event(GameEvent::Started(test_game()));
        // Immediately stop (< 2 min)
        let (events, session_end) = mgr.handle_event(GameEvent::Stopped(test_game()));
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], Event::GameEnded { duration_min, .. } if *duration_min < MIN_SESSION_LEDGER_MIN)
        );
        assert!(session_end.is_none());
        assert!(mgr.current_game().is_none());
    }

    #[test]
    fn telemetry_accumulates_into_summary() {
        let mut mgr = GameStateManager::new();
        mgr.handle_event(GameEvent::Started(test_game()));

        // Four matches: 2 wins, 1 loss, 1 draw (draw recorded but not in W/L).
        let ui = mgr.handle_telemetry(TelemetryEvent::MatchEnded(match_result(Outcome::Win)));
        assert!(matches!(&ui[0], Event::MatchEnded { result, .. } if result == "win"));
        mgr.handle_telemetry(TelemetryEvent::MatchEnded(match_result(Outcome::Loss)));
        mgr.handle_telemetry(TelemetryEvent::MatchEnded(match_result(Outcome::Win)));
        mgr.handle_telemetry(TelemetryEvent::MatchEnded(match_result(Outcome::Draw)));

        // Backdate the session so it clears the ledger threshold.
        mgr.session_start = Some(now_ms() - 30 * 60_000);

        let (_events, session_end) = mgr.handle_event(GameEvent::Stopped(test_game()));
        let summary = session_end.expect("expected a session summary");
        assert_eq!(summary.wins, 2);
        assert_eq!(summary.losses, 1);
        assert_eq!(summary.draws, 1);
        assert_eq!(summary.matches.len(), 4);
        assert_eq!(summary.game_id, "counter-strike-2");
    }

    #[test]
    fn telemetry_ignored_without_active_game() {
        let mut mgr = GameStateManager::new();
        let ui = mgr.handle_telemetry(TelemetryEvent::MatchEnded(match_result(Outcome::Win)));
        assert!(ui.is_empty());
    }

    #[test]
    fn matches_reset_between_sessions() {
        let mut mgr = GameStateManager::new();
        mgr.handle_event(GameEvent::Started(test_game()));
        mgr.handle_telemetry(TelemetryEvent::MatchEnded(match_result(Outcome::Win)));
        mgr.handle_event(GameEvent::Stopped(test_game()));

        // New session starts clean.
        mgr.handle_event(GameEvent::Started(test_game()));
        mgr.session_start = Some(now_ms() - 30 * 60_000);
        let (_e, session_end) = mgr.handle_event(GameEvent::Stopped(test_game()));
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
