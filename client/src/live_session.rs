//! The games that the viewer and the active crew play now.
//!
//! The data is the `game` field of presence (spec 17 section 5): game id, game
//! name and `started_at`. Nothing else leaves a device. The crew state load
//! and the presence push carry it for crewmates. The viewer's own game comes
//! from the game sensor events, because presence does not echo it back.
//! The live session card is built from this store. It is not a ledger event.

use std::collections::HashMap;

use mello_core::presence::{from_rfc3339_ms, GamePresence, PresenceStatus};

/// One game that one member plays now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveGame {
    pub game_id: String,
    pub game_name: String,
    /// When the game process started (Unix ms).
    pub started_ms: i64,
}

impl LiveGame {
    /// The game of a presence, or `None` without a name or a start time.
    pub fn from_presence(g: &GamePresence) -> Option<Self> {
        if g.game_name.is_empty() {
            return None;
        }
        Some(LiveGame {
            game_id: g.game_id.clone(),
            game_name: g.game_name.clone(),
            started_ms: from_rfc3339_ms(&g.started_at)?,
        })
    }
}

/// The members who play one game now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveGroup {
    pub game_id: String,
    pub game_name: String,
    /// The earliest start of the group (Unix ms).
    pub started_ms: i64,
    /// User ids, the viewer first, then in the order of the first start.
    pub user_ids: Vec<String>,
}

/// Whole minutes between a start and now. Never negative.
pub fn since_min(started_ms: i64, now_ms: i64) -> i64 {
    (now_ms - started_ms).max(0) / 60_000
}

/// The text of the card: "since 12 min", "since 1 h 5 min".
pub fn since_text(min: i64) -> String {
    let min = min.max(1);
    if min >= 60 && min % 60 == 0 {
        format!("since {} h", min / 60)
    } else if min >= 60 {
        format!("since {} h {} min", min / 60, min % 60)
    } else {
        format!("since {min} min")
    }
}

#[derive(Default)]
pub struct LiveGames {
    /// The games that the viewer's sensor has open, by game id.
    own: HashMap<String, LiveGame>,
    /// Crewmates of the active crew, by user id.
    others: HashMap<String, LiveGame>,
}

impl LiveGames {
    /// The viewer's sensor opened a game.
    pub fn start_own(&mut self, game: LiveGame) {
        self.own.insert(game.game_id.clone(), game);
    }

    /// The viewer's sensor closed a game.
    pub fn end_own(&mut self, game_id: &str) {
        self.own.remove(game_id);
    }

    /// Set or clear the game of one crewmate from a presence push.
    pub fn set_presence(
        &mut self,
        user_id: &str,
        status: &PresenceStatus,
        game: Option<&GamePresence>,
    ) {
        match Self::game_of(status, game) {
            Some(g) => {
                self.others.insert(user_id.to_string(), g);
            }
            None => {
                self.others.remove(user_id);
            }
        }
    }

    /// Replace the crewmates with the members of a crew state load.
    pub fn replace_others<'a>(
        &mut self,
        members: impl IntoIterator<Item = (&'a str, &'a PresenceStatus, Option<&'a GamePresence>)>,
    ) {
        self.others = members
            .into_iter()
            .filter_map(|(id, st, g)| Some((id.to_string(), Self::game_of(st, g)?)))
            .collect();
    }

    /// Forget the crewmates, for example when the active crew changes.
    pub fn clear_others(&mut self) {
        self.others.clear();
    }

    fn game_of(status: &PresenceStatus, game: Option<&GamePresence>) -> Option<LiveGame> {
        if *status == PresenceStatus::Offline {
            return None;
        }
        LiveGame::from_presence(game?)
    }

    /// The groups, one for each game id: the viewer's group first, then the
    /// oldest. `viewer_id` names the viewer in `user_ids`.
    pub fn groups(&self, viewer_id: &str) -> Vec<LiveGroup> {
        let mut all: Vec<(String, LiveGame)> = self
            .others
            .iter()
            .filter(|(id, _)| id.as_str() != viewer_id)
            .map(|(id, g)| (id.clone(), g.clone()))
            .collect();
        all.extend(
            self.own
                .values()
                .map(|g| (viewer_id.to_string(), g.clone())),
        );
        all.sort_by(|a, b| {
            let mine = |x: &(String, LiveGame)| x.0 != viewer_id;
            (mine(a), a.1.started_ms, &a.0).cmp(&(mine(b), b.1.started_ms, &b.0))
        });
        let mut out: Vec<LiveGroup> = Vec::new();
        for (id, g) in all {
            // A game with no id groups by its name.
            let key = |x: &LiveGroup| {
                if g.game_id.is_empty() {
                    x.game_id.is_empty() && x.game_name == g.game_name
                } else {
                    x.game_id == g.game_id
                }
            };
            match out.iter_mut().find(|x| key(x)) {
                Some(x) => {
                    x.started_ms = x.started_ms.min(g.started_ms);
                    x.user_ids.push(id);
                }
                None => out.push(LiveGroup {
                    game_id: g.game_id,
                    game_name: g.game_name,
                    started_ms: g.started_ms,
                    user_ids: vec![id],
                }),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mello_core::presence::to_rfc3339;

    /// A presence as the crew state and the push carry it.
    struct P {
        status: PresenceStatus,
        game: Option<GamePresence>,
    }

    const NOW: i64 = 1_760_000_000_000;

    fn playing(id: &str, name: &str, ago_min: i64) -> P {
        P {
            status: PresenceStatus::Online,
            game: Some(GamePresence {
                game_name: name.into(),
                game_id: id.into(),
                started_at: to_rfc3339(NOW - ago_min * 60_000),
            }),
        }
    }

    fn set(g: &mut LiveGames, id: &str, p: &P) {
        g.set_presence(id, &p.status, p.game.as_ref());
    }

    #[test]
    fn presence_gives_the_game_id_name_and_start() {
        let mut g = LiveGames::default();
        set(&mut g, "u2", &playing("rocket-league", "Rocket League", 12));
        let groups = g.groups("me");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].game_id, "rocket-league");
        assert_eq!(since_min(groups[0].started_ms, NOW), 12);
    }

    #[test]
    fn two_crewmates_in_one_game_make_one_group_from_the_earliest_start() {
        let mut g = LiveGames::default();
        set(&mut g, "u2", &playing("cs", "Counter-Strike 2", 5));
        set(&mut g, "u3", &playing("cs", "Counter-Strike 2", 20));
        set(&mut g, "u4", &playing("dota", "Dota 2", 1));
        let groups = g.groups("me");
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].user_ids, vec!["u3", "u2"]);
        assert_eq!(since_min(groups[0].started_ms, NOW), 20);
    }

    #[test]
    fn the_viewer_comes_first_and_a_cleared_game_leaves() {
        let mut g = LiveGames::default();
        set(&mut g, "u2", &playing("cs", "Counter-Strike 2", 30));
        g.start_own(LiveGame {
            game_id: "cs".into(),
            game_name: "Counter-Strike 2".into(),
            started_ms: NOW - 2 * 60_000,
        });
        assert_eq!(g.groups("me")[0].user_ids, vec!["me", "u2"]);
        g.set_presence("u2", &PresenceStatus::Online, None);
        g.end_own("cs");
        assert!(g.groups("me").is_empty());
    }

    #[test]
    fn an_offline_member_and_a_game_without_a_start_do_not_count() {
        let mut g = LiveGames::default();
        let mut off = playing("cs", "Counter-Strike 2", 3);
        off.status = PresenceStatus::Offline;
        let mut nostart = playing("cs", "Counter-Strike 2", 3);
        nostart.game.as_mut().unwrap().started_at.clear();
        g.replace_others([
            ("u2", &off.status, off.game.as_ref()),
            ("u3", &nostart.status, nostart.game.as_ref()),
            ("u4", &PresenceStatus::Online, None),
        ]);
        assert!(g.groups("me").is_empty());
    }

    #[test]
    fn since_text_reads_minutes_then_hours() {
        assert_eq!(since_text(0), "since 1 min");
        assert_eq!(since_text(12), "since 12 min");
        assert_eq!(since_text(65), "since 1 h 5 min");
        assert_eq!(since_text(120), "since 2 h");
    }
}
