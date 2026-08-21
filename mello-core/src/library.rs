//! Installed-game discovery from launcher manifests.
//!
//! This is the **tail** mechanism of the resolution ladder
//! (plans/GAME-SENSING-V2.md §5.2). The curated exe table covers the ~50 games
//! people actually play; this covers the other hundred thousand, at zero
//! per-game cost, by reading what the launcher already knows.
//!
//! The key idea is that identity comes from the **install path**, not the
//! executable name. Steam records that appid 730 lives in
//! `…/steamapps/common/Counter-Strike Global Offensive`, so *any* process
//! running from under that directory is that game — all of its shipping
//! executables, launchers and helpers, with no per-game mapping and no
//! guessing. That is what makes `javaw.exe`-style ambiguity structurally
//! impossible here.
//!
//! Steam ships first because it is 77% of the desktop catalogue; the other
//! launchers are a long tail of a few percent each and land later.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Which launcher an installed game came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibrarySource {
    Steam,
    Epic,
    Gog,
}

impl LibrarySource {
    /// Prefix for the stable game id. `steam` must never change: sessions and
    /// stats are already recorded under `steam-<appid>` keys.
    pub fn prefix(self) -> &'static str {
        match self {
            LibrarySource::Steam => "steam",
            LibrarySource::Epic => "epic",
            LibrarySource::Gog => "gog",
        }
    }
}

/// One installed game, as the launcher describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryEntry {
    pub source: LibrarySource,
    /// The launcher's own id for the game — a Steam appid, an Epic catalog
    /// item, a GOG product id. Precise, stable, and identical for every user
    /// who owns the game, even before we know IGDB's number for it.
    pub external_id: String,
    /// The launcher's own display name, e.g. "Counter-Strike 2". Free and
    /// authoritative, which is why this module needs no network to name a
    /// game it finds.
    pub name: String,
    /// Absolute install directory; every executable beneath it is this game.
    pub install_dir: PathBuf,
}

impl LibraryEntry {
    /// Stable id for the ledger and stats.
    ///
    /// Deliberately not an IGDB slug: the launcher's id *is* correct identity,
    /// it needs no lookup, and it upgrades to a richer id later without the
    /// recorded sessions changing meaning. Namespaced by launcher because the
    /// same number means different games on different stores.
    pub fn game_id(&self) -> String {
        format!("{}-{}", self.source.prefix(), self.external_id)
    }

    /// Badge label. Mirrors the build-time derivation in
    /// `scripts/build_catalogue.py` so curated and discovered games look alike.
    pub fn short_name(&self) -> String {
        derive_short_name(&self.name)
    }
}

const STOPWORDS: &[&str] = &["of", "the", "and", "a", "an", "de", "la"];

/// Badge label from a display name. Public because every surface that shows a
/// game needs one, including for games the catalogue has never heard of.
pub fn derive_short_name(name: &str) -> String {
    if name.chars().count() <= 7 {
        return name.to_string();
    }
    let cleaned = name.replace(['-', ':'], " ");
    let significant: Vec<&str> = cleaned
        .split_whitespace()
        .filter(|w| !STOPWORDS.contains(&w.to_ascii_lowercase().as_str()))
        .collect();
    match significant.len() {
        0 => name.chars().take(7).collect(),
        1 => significant[0].chars().take(8).collect(),
        _ => {
            let mut out = String::new();
            for w in &significant {
                if w.chars().all(|c| c.is_ascii_digit()) {
                    out.push_str(w);
                } else if let Some(c) = w.chars().next() {
                    out.extend(c.to_uppercase());
                }
            }
            out.chars().take(8).collect()
        }
    }
}

/// Installed games, indexed for prefix lookup by executable path.
#[derive(Debug, Default)]
pub struct LibraryIndex {
    /// Lowercased install dir -> entry. Held as a vec because lookup is a
    /// longest-prefix match, not an exact one.
    entries: Vec<(String, LibraryEntry)>,
}

impl LibraryIndex {
    pub fn from_entries(mut entries: Vec<LibraryEntry>) -> Self {
        // Longest first: a game installed inside another game's directory must
        // win over its parent.
        entries.sort_by(|a, b| {
            b.install_dir
                .as_os_str()
                .len()
                .cmp(&a.install_dir.as_os_str().len())
        });
        let entries = entries
            .into_iter()
            .map(|e| (normalize(&e.install_dir.to_string_lossy()), e))
            .collect();
        LibraryIndex { entries }
    }

    /// Scan every supported launcher. Never fails: a missing or unreadable
    /// launcher just contributes nothing, because sensing must keep working
    /// for someone who has no Steam install at all.
    pub fn scan() -> Self {
        let mut entries = scan_steam();
        entries.extend(scan_epic());
        entries.extend(scan_gog());
        log::info!(
            "[library] {} installed game(s) found (steam+epic+gog)",
            entries.len()
        );
        Self::from_entries(entries)
    }

    /// Which installed game owns this executable path, if any.
    pub fn resolve(&self, exe_path: &str) -> Option<&LibraryEntry> {
        if exe_path.is_empty() {
            return None;
        }
        let path = normalize(exe_path);
        self.entries
            .iter()
            .find(|(dir, _)| !dir.is_empty() && path.starts_with(dir.as_str()))
            .map(|(_, e)| e)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Every installed game found, for diagnostics and future library surfaces.
    pub fn iter(&self) -> impl Iterator<Item = &LibraryEntry> {
        self.entries.iter().map(|(_, e)| e)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Lowercase and unify separators so a Windows path compares predictably
/// regardless of how the manifest or the process table spelled it.
fn normalize(path: &str) -> String {
    let mut s = path.to_ascii_lowercase().replace('/', "\\");
    // A directory prefix must not match a sibling whose name merely starts the
    // same way ("...\Portal" vs "...\Portal 2").
    if !s.ends_with('\\') {
        s.push('\\');
    }
    s
}

// --------------------------------------------------------------------- Steam

/// Steam install root.
fn steam_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let path: String = hkcu
            .open_subkey("Software\\Valve\\Steam")
            .ok()
            .and_then(|k| k.get_value("SteamPath").ok())?;
        Some(PathBuf::from(path))
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("Steam"),
        )
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
    }
}

/// Every Steam library folder: the install root plus extras from
/// `libraryfolders.vdf` (games are routinely on a second drive).
fn steam_libraries() -> Vec<PathBuf> {
    let Some(root) = steam_root() else {
        return Vec::new();
    };
    let mut libraries = vec![root.clone()];
    let vdf = root.join("steamapps").join("libraryfolders.vdf");
    if let Ok(contents) = std::fs::read_to_string(&vdf) {
        libraries.extend(parse_library_paths(&contents));
    }
    dedup_paths(libraries)
}

/// Drop duplicate library folders, comparing normalized.
///
/// `libraryfolders.vdf` lists the install root alongside the extra libraries,
/// and spells it differently from the registry — `c:/program files
/// (x86)/steam` against `C:\Program Files (x86)\Steam`. Deduping the raw
/// `PathBuf`s misses that and every game is found twice, which on a real
/// install turned 8 games into 16.
fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter(|p| seen.insert(normalize(&p.to_string_lossy())))
        .collect()
}

/// Steam apps that are not games: shared runtimes and redistributables that
/// install like games, appear in `steamapps`, and would otherwise show up as
/// "playing Steamworks Common Redistributables".
const NON_GAME_APPIDS: &[u32] = &[
    228980,  // Steamworks Common Redistributables
    1070560, // Steam Linux Runtime 1.0 (scout)
    1391110, // Steam Linux Runtime 2.0 (soldier)
    1628350, // Steam Linux Runtime 3.0 (sniper)
    1826330, // Proton EasyAntiCheat Runtime
    1493710, // Proton Experimental
    2180100, // Proton Hotfix
];

/// `"path"  "D:\\SteamLibrary"` lines out of a `libraryfolders.vdf`.
fn parse_library_paths(vdf: &str) -> Vec<PathBuf> {
    vdf.lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.to_ascii_lowercase().starts_with("\"path\"") {
                return None;
            }
            let mut parts = line.split('"').filter(|s| !s.trim().is_empty());
            parts.next()?; // the "path" key
            parts
                .next()
                .map(|raw| PathBuf::from(raw.replace("\\\\", "\\")))
        })
        .collect()
}

fn scan_steam() -> Vec<LibraryEntry> {
    let mut out = Vec::new();
    for library in steam_libraries() {
        let steamapps = library.join("steamapps");
        let Ok(dir) = std::fs::read_dir(&steamapps) else {
            continue;
        };
        for manifest in dir.flatten() {
            let path = manifest.path();
            let is_manifest = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("appmanifest_") && n.ends_with(".acf"));
            if !is_manifest {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(entry) = parse_app_manifest(&contents, &steamapps) {
                out.push(entry);
            }
        }
    }
    out
}

/// Pull appid / name / installdir out of an `appmanifest_*.acf`.
///
/// Steam ships partially-written manifests for queued and in-progress
/// downloads, so anything missing a field is skipped rather than guessed at —
/// a half-installed game is not something the user is playing.
fn parse_app_manifest(contents: &str, steamapps: &Path) -> Option<LibraryEntry> {
    let mut fields: HashMap<String, String> = HashMap::new();
    for line in contents.lines() {
        let mut parts = line.trim().split('"').filter(|s| !s.trim().is_empty());
        let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        if matches!(key.as_str(), "appid" | "name" | "installdir") {
            fields.entry(key).or_insert_with(|| value.to_string());
        }
    }

    let appid: u32 = fields.get("appid")?.parse().ok()?;
    if NON_GAME_APPIDS.contains(&appid) {
        return None;
    }
    let name = fields.get("name")?.trim().to_string();
    let installdir = fields.get("installdir")?.trim();
    if name.is_empty() || installdir.is_empty() {
        return None;
    }
    Some(LibraryEntry {
        source: LibrarySource::Steam,
        external_id: appid.to_string(),
        name,
        install_dir: steamapps.join("common").join(installdir),
    })
}

// ---------------------------------------------------------------------- Epic

/// Epic writes one JSON manifest per installed game into a shared ProgramData
/// directory — no registry, no per-user path, and readable without the
/// launcher running.
#[cfg(windows)]
fn epic_manifest_dir() -> Option<PathBuf> {
    let programdata = std::env::var_os("PROGRAMDATA")?;
    Some(
        PathBuf::from(programdata)
            .join("Epic")
            .join("EpicGamesLauncher")
            .join("Data")
            .join("Manifests"),
    )
}

#[cfg(target_os = "macos")]
fn epic_manifest_dir() -> Option<PathBuf> {
    Some(PathBuf::from(
        "/Users/Shared/Epic/EpicGamesLauncher/Data/Manifests",
    ))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn epic_manifest_dir() -> Option<PathBuf> {
    None
}

fn scan_epic() -> Vec<LibraryEntry> {
    let Some(dir) = epic_manifest_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("item"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|body| parse_epic_manifest(&body))
        .collect()
}

/// Pull display name, install location and catalog id out of an Epic `.item`.
///
/// Parsed with `serde_json` rather than a hand-rolled reader because these are
/// real JSON documents with dozens of fields; only four matter.
fn parse_epic_manifest(body: &str) -> Option<LibraryEntry> {
    #[derive(serde::Deserialize)]
    struct EpicManifest {
        #[serde(default)]
        display_name: String,
        #[serde(default)]
        install_location: String,
        #[serde(default)]
        catalog_item_id: String,
        #[serde(default)]
        app_name: String,
    }
    // Epic uses PascalCase keys.
    let raw: serde_json::Value = serde_json::from_str(body).ok()?;
    let get = |k: &str| {
        raw.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let m = EpicManifest {
        display_name: get("DisplayName"),
        install_location: get("InstallLocation"),
        catalog_item_id: get("CatalogItemId"),
        app_name: get("AppName"),
    };

    if m.display_name.trim().is_empty() || m.install_location.trim().is_empty() {
        return None;
    }
    // CatalogItemId is the durable product id; AppName is the per-artifact one
    // and changes between editions, so prefer the former.
    let id = if m.catalog_item_id.is_empty() {
        m.app_name
    } else {
        m.catalog_item_id
    };
    if id.is_empty() {
        return None;
    }
    Some(LibraryEntry {
        source: LibrarySource::Epic,
        external_id: id,
        name: m.display_name.trim().to_string(),
        install_dir: PathBuf::from(m.install_location.trim()),
    })
}

// ----------------------------------------------------------------------- GOG

/// GOG Galaxy records each install under its own registry key. Games installed
/// without Galaxy still land here — the installer writes the key regardless,
/// which is why this catches offline-installer users too.
#[cfg(windows)]
fn scan_gog() -> Vec<LibraryEntry> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    // 32-bit view first (where GOG writes on 64-bit Windows), then native.
    let roots = [
        "SOFTWARE\\WOW6432Node\\GOG.com\\Games",
        "SOFTWARE\\GOG.com\\Games",
    ];
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for root in roots {
        let Ok(games) = hklm.open_subkey(root) else {
            continue;
        };
        for id in games.enum_keys().flatten() {
            let Ok(key) = games.open_subkey(&id) else {
                continue;
            };
            let name: String = key.get_value("gameName").unwrap_or_default();
            let path: String = key.get_value("path").unwrap_or_default();
            if name.trim().is_empty() || path.trim().is_empty() || !seen.insert(id.clone()) {
                continue;
            }
            out.push(LibraryEntry {
                source: LibrarySource::Gog,
                external_id: id,
                name: name.trim().to_string(),
                install_dir: PathBuf::from(path.trim()),
            });
        }
    }
    out
}

#[cfg(not(windows))]
fn scan_gog() -> Vec<LibraryEntry> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
"AppState"
{
	"appid"		"730"
	"Universe"		"1"
	"name"		"Counter-Strike 2"
	"StateFlags"		"4"
	"installdir"		"Counter-Strike Global Offensive"
	"LastUpdated"		"1745000000"
}
"#;

    fn steamapps() -> PathBuf {
        PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps")
    }

    #[test]
    fn parses_an_app_manifest() {
        let e = parse_app_manifest(MANIFEST, &steamapps()).expect("valid manifest");
        assert_eq!(e.external_id, "730");
        assert_eq!(e.name, "Counter-Strike 2");
        assert_eq!(
            e.install_dir,
            steamapps()
                .join("common")
                .join("Counter-Strike Global Offensive")
        );
        assert_eq!(e.game_id(), "steam-730");
    }

    #[test]
    fn skips_incomplete_manifests() {
        // Steam writes these for queued and in-progress downloads.
        assert!(parse_app_manifest(r#""AppState" { "appid" "730" }"#, &steamapps()).is_none());
        assert!(parse_app_manifest(
            "\"AppState\"\n{\n\"appid\" \"730\"\n\"name\" \"X\"\n}",
            &steamapps()
        )
        .is_none());
        assert!(parse_app_manifest("garbage", &steamapps()).is_none());
    }

    const EPIC_MANIFEST: &str = r#"{
        "FormatVersion": 0,
        "AppName": "Fortnite",
        "CatalogItemId": "4fe75bbc5a674f4f9b356b5c90567da5",
        "DisplayName": "Fortnite",
        "InstallLocation": "C:\\Program Files\\Epic Games\\Fortnite",
        "LaunchExecutable": "FortniteGame/Binaries/Win64/FortniteClient-Win64-Shipping.exe"
    }"#;

    #[test]
    fn parses_an_epic_manifest() {
        let e = parse_epic_manifest(EPIC_MANIFEST).expect("valid manifest");
        assert_eq!(e.source, LibrarySource::Epic);
        assert_eq!(e.name, "Fortnite");
        assert_eq!(e.game_id(), "epic-4fe75bbc5a674f4f9b356b5c90567da5");
        assert_eq!(
            e.install_dir,
            PathBuf::from(r"C:\Program Files\Epic Games\Fortnite")
        );
    }

    #[test]
    fn epic_manifest_falls_back_to_app_name() {
        // Some manifests omit CatalogItemId; AppName is less durable across
        // editions but is better than dropping the game entirely.
        let body = r#"{"AppName":"Rocket League","DisplayName":"Rocket League",
            "InstallLocation":"D:\\Epic\\rocketleague"}"#;
        let e = parse_epic_manifest(body).expect("valid manifest");
        assert_eq!(e.game_id(), "epic-Rocket League");
    }

    #[test]
    fn epic_manifests_without_a_location_are_skipped() {
        // Owned-but-not-installed entries have no install path, and a game we
        // cannot locate can never be matched to a running process.
        assert!(parse_epic_manifest(r#"{"DisplayName":"X","CatalogItemId":"a"}"#).is_none());
        assert!(
            parse_epic_manifest(r#"{"InstallLocation":"C:\\X","CatalogItemId":"a"}"#).is_none()
        );
        assert!(parse_epic_manifest("not json").is_none());
    }

    #[test]
    fn game_ids_are_namespaced_per_launcher() {
        // The same number means different games on different stores, so ids
        // must not collide. `steam-` in particular can never change: sessions
        // and stats are already recorded under it.
        let steam = entry(730, "Counter-Strike 2", r"C:\a");
        let epic = LibraryEntry {
            source: LibrarySource::Epic,
            external_id: "730".into(),
            name: "Something Else".into(),
            install_dir: PathBuf::from(r"C:\b"),
        };
        assert_eq!(steam.game_id(), "steam-730");
        assert_eq!(epic.game_id(), "epic-730");
        assert_ne!(steam.game_id(), epic.game_id());
    }

    #[test]
    fn library_folders_dedupe_across_spellings() {
        // libraryfolders.vdf repeats the install root with a different
        // spelling than the registry. Without normalizing, every game in the
        // root library is found twice — which is what a real install did.
        let deduped = dedup_paths(vec![
            PathBuf::from(r"C:\Program Files (x86)\Steam"),
            PathBuf::from("c:/program files (x86)/steam"),
            PathBuf::from(r"D:\SteamLibrary"),
        ]);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0], PathBuf::from(r"C:\Program Files (x86)\Steam"));
    }

    #[test]
    fn redistributables_are_not_games() {
        // "Steamworks Common Redistributables" installs like a game and sits
        // in steamapps; reporting it as one is nonsense.
        let manifest = r#"
"AppState"
{
	"appid"		"228980"
	"name"		"Steamworks Common Redistributables"
	"installdir"		"Steamworks Shared"
}
"#;
        assert!(parse_app_manifest(manifest, &steamapps()).is_none());
    }

    #[test]
    fn parses_library_folders() {
        let vdf = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"
	}
}
"#;
        let paths = parse_library_paths(vdf);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[1], PathBuf::from(r"D:\SteamLibrary"));
    }

    fn entry(appid: u32, name: &str, dir: &str) -> LibraryEntry {
        LibraryEntry {
            source: LibrarySource::Steam,
            external_id: appid.to_string(),
            name: name.to_string(),
            install_dir: PathBuf::from(dir),
        }
    }

    #[test]
    fn resolves_any_executable_under_an_install_dir() {
        // The point of path-prefix identity: every shipping binary of a game
        // resolves without being named anywhere.
        let idx = LibraryIndex::from_entries(vec![entry(
            1245620,
            "ELDEN RING",
            r"D:\SteamLibrary\steamapps\common\ELDEN RING",
        )]);
        for exe in [
            r"D:\SteamLibrary\steamapps\common\ELDEN RING\Game\eldenring.exe",
            r"D:\SteamLibrary\steamapps\common\ELDEN RING\Game\start_protected_game.exe",
            r"d:\steamlibrary\steamapps\common\elden ring\game\eldenring.exe",
        ] {
            let e = idx.resolve(exe).unwrap_or_else(|| panic!("{exe}"));
            assert_eq!(e.external_id, "1245620");
        }
    }

    #[test]
    fn forward_slashes_and_case_do_not_matter() {
        let idx = LibraryIndex::from_entries(vec![entry(730, "CS2", r"C:\Steam\common\CS")]);
        assert!(idx.resolve("C:/Steam/common/CS/game/cs2.exe").is_some());
        assert!(idx.resolve(r"c:\STEAM\COMMON\cs\game\CS2.EXE").is_some());
    }

    #[test]
    fn a_sibling_with_a_shared_prefix_does_not_match() {
        // "Portal" must not swallow "Portal 2" — the reason install dirs are
        // compared with a trailing separator.
        let idx = LibraryIndex::from_entries(vec![
            entry(400, "Portal", r"C:\Steam\common\Portal"),
            entry(620, "Portal 2", r"C:\Steam\common\Portal 2"),
        ]);
        assert_eq!(
            idx.resolve(r"C:\Steam\common\Portal 2\bin\portal2.exe")
                .unwrap()
                .game_id(),
            "steam-620"
        );
        assert_eq!(
            idx.resolve(r"C:\Steam\common\Portal\bin\portal.exe")
                .unwrap()
                .game_id(),
            "steam-400"
        );
    }

    #[test]
    fn nested_install_prefers_the_deeper_match() {
        let idx = LibraryIndex::from_entries(vec![
            entry(1, "Outer", r"C:\Games\Outer"),
            entry(2, "Inner", r"C:\Games\Outer\Mods\Inner"),
        ]);
        assert_eq!(
            idx.resolve(r"C:\Games\Outer\Mods\Inner\inner.exe")
                .unwrap()
                .game_id(),
            "steam-2"
        );
        assert_eq!(
            idx.resolve(r"C:\Games\Outer\outer.exe").unwrap().game_id(),
            "steam-1"
        );
    }

    #[test]
    fn unrelated_paths_and_empty_input_resolve_to_nothing() {
        let idx = LibraryIndex::from_entries(vec![entry(730, "CS2", r"C:\Steam\common\CS")]);
        assert!(idx.resolve(r"C:\Windows\notepad.exe").is_none());
        assert!(idx.resolve("").is_none());
        assert!(LibraryIndex::default()
            .resolve(r"C:\anything.exe")
            .is_none());
    }

    #[test]
    fn short_names_match_the_build_time_derivation() {
        assert_eq!(derive_short_name("Counter-Strike 2"), "CS2");
        assert_eq!(derive_short_name("ELDEN RING"), "ER");
        assert_eq!(derive_short_name("Portal"), "Portal");
        // One significant word uses the word, not a truncated full name.
        assert_eq!(derive_short_name("The Saboteur"), "Saboteur");
    }
}
