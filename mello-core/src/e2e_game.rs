//! Test-only fake game sensor input (`e2e-game` feature, never shipped).
//!
//! A journey cannot start a real game. `inject` queues the `GameEvent` that the
//! sensor would send for a catalogue game. `Client::drain_game_events` takes the
//! queue with the real sensor events, so the game state manager, the presence
//! publish, the UI events and `game_session_end` all run their normal code
//! (plans/E2E-QA.md section 5.1).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::catalogue::Head;
use crate::game_sensing::{ActiveGame, GameEvent};

static QUEUE: Mutex<Vec<GameEvent>> = Mutex::new(Vec::new());
/// The fake games that are open, by game id, so an end finds its start.
static OPEN: Mutex<Option<HashMap<String, ActiveGame>>> = Mutex::new(None);

/// What `inject` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    End,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Queue a sensor event for a game of the bundled catalogue. `ago_min` moves
/// the start into the past, so a short journey can end a session that is long
/// enough for the ledger (`MIN_SESSION_LEDGER_MIN`).
pub fn inject(action: Action, game_id: &str, ago_min: i64) -> Result<(), String> {
    let mut open = OPEN.lock().map_err(|_| "lock poisoned".to_string())?;
    let open = open.get_or_insert_with(HashMap::new);
    let event = match action {
        Action::Start => {
            let head = Head::bundled().ok_or("no bundled catalogue")?;
            let entry = head
                .by_game_id(game_id)
                .ok_or_else(|| format!("{game_id} is not in the catalogue"))?;
            let started = now_ms() - ago_min.max(0) * 60_000;
            let game = ActiveGame {
                game_id: entry.game_id.to_string(),
                game_name: entry.name.to_string(),
                short_name: entry.short_name.to_string(),
                color: "#2A2A30".into(),
                exe: format!("{game_id}.exe"),
                exe_path: String::new(),
                // A pid no real process of the sensor can have.
                pid: 4_000_000_000 - open.len() as u32,
                igdb_id: Some(entry.igdb_id),
                started_at: started,
                started_at_ms: started,
                foreground_ms: ago_min.max(0) * 60_000,
            };
            open.insert(game_id.to_string(), game.clone());
            GameEvent::Started(game)
        }
        Action::End => {
            let game = open
                .remove(game_id)
                .ok_or_else(|| format!("no fake game {game_id} is open"))?;
            GameEvent::Stopped {
                game: Box::new(game),
                ended_at: now_ms(),
            }
        }
    };
    log::info!("[e2e] fake game {action:?}: {game_id}");
    QUEUE
        .lock()
        .map_err(|_| "lock poisoned".to_string())?
        .push(event);
    Ok(())
}

/// The queued events, oldest first. Called by the client loop.
pub(crate) fn take() -> Vec<GameEvent> {
    QUEUE
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_and_an_end_queue_the_sensor_events_of_one_game() {
        inject(Action::Start, "rocket-league", 12).expect("start");
        assert!(inject(Action::Start, "no-such-game", 0).is_err());
        inject(Action::End, "rocket-league", 0).expect("end");
        assert!(inject(Action::End, "rocket-league", 0).is_err());
        let events = take();
        assert_eq!(events.len(), 2);
        let GameEvent::Started(g) = &events[0] else {
            panic!("first is a start");
        };
        assert_eq!(g.game_id, "rocket-league");
        assert!(now_ms() - g.started_at >= 12 * 60_000);
        assert!(
            matches!(&events[1], GameEvent::Stopped { game, .. } if game.game_id == "rocket-league")
        );
    }
}
