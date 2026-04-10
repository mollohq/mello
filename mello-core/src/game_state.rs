use crate::events::Event;
use crate::game_sensing::{ActiveGame, GameEvent};

const MIN_SESSION_LEDGER_MIN: u32 = 2;
/// Used by the UI handler to decide whether to show post-game prompt.
pub const MIN_SESSION_POSTGAME_MIN: u32 = 5;

#[derive(Default)]
pub struct GameStateManager {
    current_game: Option<ActiveGame>,
    session_start: Option<i64>,
}

impl GameStateManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process a game event from the sensor and return UI events to emit
    /// plus an optional (crew_id-independent) game_session_end request.
    pub fn handle_event(&mut self, event: GameEvent) -> (Vec<Event>, Option<GameSessionEndInfo>) {
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

                log::info!(
                    "[game-state] game stopped: {} (duration={}min)",
                    game.game_name,
                    duration_min
                );

                self.current_game = None;
                self.session_start = None;

                if duration_min >= MIN_SESSION_LEDGER_MIN {
                    session_end = Some(GameSessionEndInfo {
                        game_name: game.game_name.clone(),
                        duration_min,
                    });
                }

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

    pub fn current_game(&self) -> Option<&ActiveGame> {
        self.current_game.as_ref()
    }
}

/// Info needed to call the game_session_end RPC (crew_id is supplied by the caller).
pub struct GameSessionEndInfo {
    pub game_name: String,
    pub duration_min: u32,
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
    fn postgame_threshold() {
        const { assert!(MIN_SESSION_POSTGAME_MIN > MIN_SESSION_LEDGER_MIN) };
        assert_eq!(MIN_SESSION_POSTGAME_MIN, 5);
        assert_eq!(MIN_SESSION_LEDGER_MIN, 2);
    }
}
