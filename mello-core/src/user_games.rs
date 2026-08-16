//! Games the *user* told us about, and the ones they told us to ignore.
//!
//! This used to be `game_db`, a bundled catalogue of 25 hand-mapped
//! executables plus a user overlay. The catalogue half is gone: identity now
//! comes from [`crate::catalogue`] (curated exe table + IGDB metadata), the
//! installed-library scan, and provisional tracking. What is left is the part
//! only the user can supply — a game they confirmed by hand, and executables
//! they said are not games at all.
//!
//! Shared with the sensor thread behind an `RwLock` so a confirmation applies
//! from the next scan without a restart.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;

/// A game the user confirmed by hand, outside anything the catalogue or a
/// launcher could resolve. Persisted in client settings.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct CustomGame {
    pub id: String,
    pub name: String,
    pub short_name: String,
    pub exe: String,
}

/// Stable id for a user-confirmed game, e.g. "custom-night-stones" from
/// "Night Stones.exe". Namespaced away from catalogue, launcher (`steam-`)
/// and provisional (`local-`) ids.
pub fn custom_game_id(exe: &str) -> String {
    let stem = exe
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(exe)
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE");
    let mut slug = String::with_capacity(stem.len());
    let mut last_dash = true; // suppress leading dashes
    for c in stem.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            slug.push(c);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "custom-game".to_string()
    } else {
        format!("custom-{slug}")
    }
}

/// The user's own games and dismissals.
#[derive(Clone, Default)]
pub struct UserGames {
    /// Lowercased exe -> confirmed game.
    by_exe: HashMap<String, CustomGame>,
    /// Executables the user has said are not games. Lowercased.
    ///
    /// A dismissal has to suppress *tracking*, not just the confirm prompt:
    /// provisional tracking would otherwise keep filing sessions for something
    /// the user already rejected, with no prompt left to reject it again.
    dismissed_exes: HashSet<String>,
}

impl UserGames {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the confirmed-game set.
    pub fn set_games(&mut self, games: &[CustomGame]) {
        self.by_exe = games
            .iter()
            .map(|g| (g.exe.to_lowercase(), g.clone()))
            .collect();
    }

    /// Replace the "not a game" list.
    pub fn set_dismissed_exes(&mut self, exes: &[String]) {
        self.dismissed_exes = exes.iter().map(|e| e.to_lowercase()).collect();
    }

    pub fn is_dismissed(&self, exe: &str) -> bool {
        self.dismissed_exes.contains(&exe.to_lowercase())
    }

    pub fn lookup_by_exe(&self, exe: &str) -> Option<&CustomGame> {
        self.by_exe.get(&exe.to_lowercase())
    }

    pub fn len(&self) -> usize {
        self.by_exe.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_exe.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(exe: &str, name: &str) -> CustomGame {
        CustomGame {
            id: custom_game_id(exe),
            name: name.to_string(),
            short_name: "NS".into(),
            exe: exe.to_string(),
        }
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let mut store = UserGames::new();
        store.set_games(&[game("Night Stones.exe", "Night Stones")]);
        assert!(store.lookup_by_exe("night stones.exe").is_some());
        assert!(store.lookup_by_exe("NIGHT STONES.EXE").is_some());
        assert!(store.lookup_by_exe("other.exe").is_none());
    }

    #[test]
    fn set_games_replaces_rather_than_merges() {
        // Settings are the source of truth; a removed game must disappear.
        let mut store = UserGames::new();
        store.set_games(&[game("a.exe", "A")]);
        store.set_games(&[game("b.exe", "B")]);
        assert!(store.lookup_by_exe("a.exe").is_none());
        assert!(store.lookup_by_exe("b.exe").is_some());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn dismissals_are_case_insensitive() {
        let mut store = UserGames::new();
        store.set_dismissed_exes(&["Night Stones.exe".to_string()]);
        assert!(store.is_dismissed("night stones.exe"));
        assert!(!store.is_dismissed("something-else.exe"));
    }

    #[test]
    fn custom_game_id_slugs() {
        assert_eq!(custom_game_id("Night Stones.exe"), "custom-night-stones");
        assert_eq!(custom_game_id("octogeddon.exe"), "custom-octogeddon");
        assert_eq!(
            custom_game_id("C:\\Games\\Some_Game v2.exe"),
            "custom-some-game-v2"
        );
        assert_eq!(custom_game_id("....exe"), "custom-game");
    }

    #[test]
    fn ids_do_not_collide_with_other_namespaces() {
        // custom- / steam- / epic- / local- must stay distinct, or a
        // user-confirmed game could shadow a discovered one in the ledger.
        let id = custom_game_id("night stones.exe");
        assert!(id.starts_with("custom-"));
        assert!(!id.starts_with("steam-"));
        assert!(!id.starts_with("local-"));
    }
}
