use std::collections::HashMap;

use serde::Deserialize;

/// A user-confirmed game outside the bundled DB (spec 17 extension: the
/// unknown-game confirm flow). Persisted in client settings and overlaid on
/// the bundled DB at startup / on confirm.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct CustomGame {
    pub id: String,
    pub name: String,
    pub short_name: String,
    pub exe: String,
}

impl CustomGame {
    fn to_entry(&self) -> GameEntry {
        GameEntry {
            id: self.id.clone(),
            igdb_id: None,
            name: self.name.clone(),
            short_name: self.short_name.clone(),
            exe: vec![self.exe.clone()],
            icon_url: None,
            cover_url: None,
            color: None,
            category: None,
        }
    }
}

/// Stable id for a user-confirmed game, e.g. "custom-night-stones" from
/// "Night Stones.exe". Namespaced away from bundled ids.
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

#[derive(Debug, Clone, Deserialize)]
pub struct GameEntry {
    pub id: String,
    #[serde(default)]
    pub igdb_id: Option<u64>,
    pub name: String,
    pub short_name: String,
    pub exe: Vec<String>,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub cover_url: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GamesEnvelope {
    #[allow(dead_code)]
    version: u32,
    games: Vec<GameEntry>,
}

#[derive(Clone)]
pub struct GameDatabase {
    /// Executables the user has explicitly said are not games. Lowercased.
    /// Held here because this is already the structure shared with the sensor
    /// thread, and a dismissal has to suppress *tracking*, not just the
    /// prompt — otherwise "not a game" still files sessions for it.
    dismissed_exes: std::collections::HashSet<String>,
    by_exe: HashMap<String, GameEntry>,
    by_id: HashMap<String, GameEntry>,
    /// Lowercased display name → entry, for legacy events that carry only a
    /// game name (e.g. stream sessions before game_id was recorded).
    by_name: HashMap<String, GameEntry>,
}

impl GameDatabase {
    pub fn load_bundled() -> Self {
        let json = include_str!("../../client/assets/games.json");
        let envelope: GamesEnvelope =
            serde_json::from_str(json).expect("invalid bundled games.json");
        Self::from_entries(&envelope.games)
    }

    fn from_entries(entries: &[GameEntry]) -> Self {
        let mut by_exe = HashMap::new();
        let mut by_id = HashMap::new();
        let mut by_name = HashMap::new();
        for entry in entries {
            for exe in &entry.exe {
                by_exe.insert(exe.to_lowercase(), entry.clone());
            }
            by_id.insert(entry.id.clone(), entry.clone());
            by_name.insert(entry.name.to_lowercase(), entry.clone());
        }
        GameDatabase {
            dismissed_exes: std::collections::HashSet::new(),
            by_exe,
            by_id,
            by_name,
        }
    }

    /// Overlay user-confirmed games onto the DB. Bundled entries win on
    /// conflict (a custom entry must never shadow e.g. cs2.exe).
    pub fn add_user_entries(&mut self, games: &[CustomGame]) {
        for game in games {
            let entry = game.to_entry();
            self.by_exe
                .entry(game.exe.to_lowercase())
                .or_insert_with(|| entry.clone());
            self.by_name
                .entry(entry.name.to_lowercase())
                .or_insert_with(|| entry.clone());
            self.by_id.entry(entry.id.clone()).or_insert(entry);
        }
    }

    /// Replace the user's "not a game" list.
    pub fn set_dismissed_exes(&mut self, exes: &[String]) {
        self.dismissed_exes = exes.iter().map(|e| e.to_lowercase()).collect();
    }

    /// Has the user said this executable is not a game?
    pub fn is_dismissed(&self, exe: &str) -> bool {
        self.dismissed_exes.contains(&exe.to_lowercase())
    }

    pub fn lookup_by_exe(&self, exe: &str) -> Option<&GameEntry> {
        self.by_exe.get(&exe.to_lowercase())
    }

    /// Look up a game by its stable DB id (e.g. "counter-strike-2"). Used to
    /// resolve display name/short-name/color for stats surfaces (spec 19).
    pub fn lookup_by_id(&self, id: &str) -> Option<&GameEntry> {
        self.by_id.get(id)
    }

    /// Case-insensitive lookup by display name, for events that carry only a
    /// game name (stream sessions, legacy game sessions without an id).
    pub fn lookup_by_name(&self, name: &str) -> Option<&GameEntry> {
        self.by_name.get(&name.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> GameDatabase {
        let json = r##"{
            "version": 1,
            "updated_at": "2026-04-03T00:00:00Z",
            "games": [
                {
                    "id": "test-game",
                    "name": "Test Game",
                    "short_name": "TG",
                    "exe": ["TestGame.exe", "testgame_launcher.exe"],
                    "color": "#FF0000",
                    "category": "fps"
                },
                {
                    "id": "another-game",
                    "name": "Another Game",
                    "short_name": "AG",
                    "exe": ["another.exe"],
                    "color": "#00FF00"
                }
            ]
        }"##;
        let envelope: GamesEnvelope = serde_json::from_str(json).unwrap();
        GameDatabase::from_entries(&envelope.games)
    }

    #[test]
    fn lookup_case_insensitive() {
        let db = test_db();
        assert!(db.lookup_by_exe("testgame.exe").is_some());
        assert!(db.lookup_by_exe("TESTGAME.EXE").is_some());
        assert!(db.lookup_by_exe("TestGame.exe").is_some());
        assert_eq!(db.lookup_by_exe("testgame.exe").unwrap().id, "test-game");
    }

    #[test]
    fn lookup_multi_exe() {
        let db = test_db();
        let a = db.lookup_by_exe("TestGame.exe");
        let b = db.lookup_by_exe("testgame_launcher.exe");
        assert!(a.is_some());
        assert!(b.is_some());
        assert_eq!(a.unwrap().id, b.unwrap().id);
    }

    #[test]
    fn lookup_unknown_returns_none() {
        let db = test_db();
        assert!(db.lookup_by_exe("unknown.exe").is_none());
        assert!(db.lookup_by_exe("").is_none());
    }

    #[test]
    fn load_bundled_succeeds() {
        let db = GameDatabase::load_bundled();
        assert!(db.lookup_by_exe("cs2.exe").is_some());
        let cs2 = db.lookup_by_exe("cs2.exe").unwrap();
        assert_eq!(cs2.id, "counter-strike-2");
        assert_eq!(cs2.short_name, "CS2");
    }

    #[test]
    fn lookup_by_id_resolves_display() {
        let db = GameDatabase::load_bundled();
        let cs2 = db.lookup_by_id("counter-strike-2").unwrap();
        assert_eq!(cs2.short_name, "CS2");
        assert_eq!(cs2.name, "Counter-Strike 2");
        assert!(db.lookup_by_id("no-such-game").is_none());
    }

    #[test]
    fn bundled_valorant_lookup() {
        let db = GameDatabase::load_bundled();
        let val = db.lookup_by_exe("VALORANT-Win64-Shipping.exe").unwrap();
        assert_eq!(val.id, "valorant");
        // Case-insensitive
        let val2 = db.lookup_by_exe("valorant-win64-shipping.exe").unwrap();
        assert_eq!(val2.id, "valorant");
    }

    #[test]
    fn lookup_by_name_case_insensitive_incl_custom() {
        let mut db = GameDatabase::load_bundled();
        assert_eq!(
            db.lookup_by_name("counter-strike 2").unwrap().id,
            "counter-strike-2"
        );
        assert_eq!(
            db.lookup_by_name("Counter-Strike 2").unwrap().id,
            "counter-strike-2"
        );
        assert!(db.lookup_by_name("No Such Game").is_none());

        db.add_user_entries(&[CustomGame {
            id: "custom-night-stones".into(),
            name: "Night Stones".into(),
            short_name: "NS".into(),
            exe: "Night Stones.exe".into(),
        }]);
        assert_eq!(
            db.lookup_by_name("night stones").unwrap().id,
            "custom-night-stones"
        );
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
    fn user_entries_overlay_without_shadowing_bundled() {
        let mut db = GameDatabase::load_bundled();
        db.add_user_entries(&[
            CustomGame {
                id: "custom-night-stones".into(),
                name: "Night Stones".into(),
                short_name: "Night Stones".into(),
                exe: "Night Stones.exe".into(),
            },
            // A malicious/buggy custom entry must not shadow a bundled exe.
            CustomGame {
                id: "custom-fake-cs2".into(),
                name: "Fake".into(),
                short_name: "F".into(),
                exe: "cs2.exe".into(),
            },
        ]);
        assert_eq!(
            db.lookup_by_exe("night stones.exe").unwrap().id,
            "custom-night-stones"
        );
        assert_eq!(
            db.lookup_by_id("custom-night-stones").unwrap().name,
            "Night Stones"
        );
        assert_eq!(db.lookup_by_exe("cs2.exe").unwrap().id, "counter-strike-2");
    }

    #[test]
    fn optional_fields_deserialize() {
        let db = test_db();
        let entry = db.lookup_by_exe("another.exe").unwrap();
        assert!(entry.igdb_id.is_none());
        assert!(entry.icon_url.is_none());
        assert!(entry.cover_url.is_none());
        assert!(entry.category.is_none());
        assert_eq!(entry.color.as_deref(), Some("#00FF00"));
    }
}
