use std::collections::HashMap;

use serde::Deserialize;

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
    by_exe: HashMap<String, GameEntry>,
    by_id: HashMap<String, GameEntry>,
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
        for entry in entries {
            for exe in &entry.exe {
                by_exe.insert(exe.to_lowercase(), entry.clone());
            }
            by_id.insert(entry.id.clone(), entry.clone());
        }
        GameDatabase { by_exe, by_id }
    }

    pub fn lookup_by_exe(&self, exe: &str) -> Option<&GameEntry> {
        self.by_exe.get(&exe.to_lowercase())
    }

    /// Look up a game by its stable DB id (e.g. "counter-strike-2"). Used to
    /// resolve display name/short-name/color for stats surfaces (spec 19).
    pub fn lookup_by_id(&self, id: &str) -> Option<&GameEntry> {
        self.by_id.get(id)
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
