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
    /// Lifetime minutes played, from Steam's local `localconfig.vdf`. Account
    /// wide, so it includes play on other machines. Steam only; `None` when
    /// the file is missing or has no value for this app.
    pub playtime_min: Option<u32>,
    /// Last played, unix seconds. `localconfig.vdf` first, then the
    /// `appmanifest` value. Steam only.
    pub last_played_unix: Option<u64>,
    /// The user hid this game in the Steam library. Steam only.
    pub hidden: bool,
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
    if let Some(root) = steam_root() {
        apply_local_steam_data(&root, &mut out);
    }
    out
}

// ------------------------------------------------------------ Steam local art

/// Files Steam keeps for one app in `appcache/librarycache/<appid>/`, best
/// first. A header is the landscape capsule (460 by 215). The portrait cover
/// is the last resort. Newer clients put the header in a hash-named
/// subfolder, so each name is also tried one level down.
const STEAM_ART_NAMES: &[&str] = &["header.jpg", "library_header.jpg", "library_600x900.jpg"];

/// Local Steam header art for a game id such as `steam-730`. `None` for any
/// other launcher, for an id with a non-numeric tail, or when the Steam cache
/// has no art for the app. Reads the disk only: nothing goes to the network
/// and nothing leaves the device.
pub fn local_steam_art(game_id: &str) -> Option<PathBuf> {
    let appid = game_id.strip_prefix("steam-")?;
    steam_header_art_in(&steam_root()?, appid)
}

/// The header image of an installed Steam app under a Steam root.
///
/// The appid must be all digits, so an id cannot walk out of the cache folder.
/// Order: `<appid>_header.jpg` (old clients), then for each art name
/// `<appid>/<name>`, then `<appid>/<hash>/<name>` with the hash folders in
/// name order, so the answer does not depend on directory order.
pub fn steam_header_art_in(root: &Path, appid: &str) -> Option<PathBuf> {
    if appid.is_empty() || !appid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let cache = root.join("appcache").join("librarycache");
    let old = cache.join(format!("{appid}_header.jpg"));
    if old.is_file() {
        return Some(old);
    }
    let app_dir = cache.join(appid);
    let mut subdirs: Vec<PathBuf> = std::fs::read_dir(&app_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    subdirs.sort();
    for name in STEAM_ART_NAMES {
        let direct = app_dir.join(name);
        if direct.is_file() {
            return Some(direct);
        }
        if let Some(found) = subdirs.iter().map(|d| d.join(name)).find(|p| p.is_file()) {
            return Some(found);
        }
    }
    None
}

// ------------------------------------------------------- Steam local readers

/// A parsed Valve KeyValues document. Only what the readers below need.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Vdf {
    Str(String),
    Map(Vec<(String, Vdf)>),
}

impl Vdf {
    /// Child lookup. Valve files are inconsistent about key case
    /// (`Playtime`, `playtime`), so the match ignores case.
    fn get(&self, key: &str) -> Option<&Vdf> {
        match self {
            Vdf::Map(items) => items
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            Vdf::Str(_) => None,
        }
    }

    fn path(&self, keys: &[&str]) -> Option<&Vdf> {
        keys.iter().try_fold(self, |node, k| node.get(k))
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Vdf::Str(s) => Some(s),
            Vdf::Map(_) => None,
        }
    }

    fn entries(&self) -> &[(String, Vdf)] {
        match self {
            Vdf::Map(items) => items,
            Vdf::Str(_) => &[],
        }
    }
}

/// Keys the parser drops while reading, so their values never sit in memory.
/// `LaunchOptions` can hold paths, tokens or account names.
const VDF_SKIPPED_KEYS: &[&str] = &["launchoptions"];

#[derive(PartialEq)]
enum VdfTok {
    Str(String),
    Open,
    Close,
}

fn vdf_tokens(text: &str) -> Option<Vec<VdfTok>> {
    let mut toks = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => toks.push(VdfTok::Open),
            '}' => toks.push(VdfTok::Close),
            '"' => {
                let mut v = String::new();
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            'n' => v.push('\n'),
                            't' => v.push('\t'),
                            other => v.push(other),
                        },
                        other => v.push(other),
                    }
                }
                toks.push(VdfTok::Str(v));
            }
            '/' if chars.peek() == Some(&'/') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    Some(toks)
}

fn vdf_block(
    toks: &mut std::iter::Peekable<std::vec::IntoIter<VdfTok>>,
    top: bool,
) -> Option<Vec<(String, Vdf)>> {
    let mut items = Vec::new();
    loop {
        match toks.next() {
            None if top => return Some(items),
            Some(VdfTok::Close) if !top => return Some(items),
            Some(VdfTok::Str(key)) => {
                let skip = VDF_SKIPPED_KEYS.iter().any(|k| key.eq_ignore_ascii_case(k));
                match toks.next()? {
                    VdfTok::Str(v) => {
                        if !skip {
                            items.push((key, Vdf::Str(v)));
                        }
                    }
                    VdfTok::Open => {
                        let sub = vdf_block(toks, false)?;
                        if !skip {
                            items.push((key, Vdf::Map(sub)));
                        }
                    }
                    VdfTok::Close => return None,
                }
            }
            _ => return None,
        }
    }
}

/// Parse Valve KeyValues text into a tree. Returns `None` on malformed input.
fn parse_vdf(text: &str) -> Option<Vdf> {
    let mut it = vdf_tokens(text)?.into_iter().peekable();
    vdf_block(&mut it, true).map(Vdf::Map)
}

/// Steam account ids in `userdata/` are the low 32 bits of the SteamID64.
const STEAM_ID64_BASE: u64 = 76_561_197_960_265_728;

/// `userdata/<id>` folder names for users marked `MostRecent "1"` in
/// `config/loginusers.vdf`.
fn most_recent_account_ids(loginusers: &str) -> Vec<String> {
    let Some(doc) = parse_vdf(loginusers) else {
        return Vec::new();
    };
    let Some(users) = doc.get("users") else {
        return Vec::new();
    };
    users
        .entries()
        .iter()
        .filter(|(_, u)| u.get("MostRecent").and_then(Vdf::as_str) == Some("1"))
        .filter_map(|(id64, _)| id64.parse::<u64>().ok())
        .filter(|id| *id >= STEAM_ID64_BASE)
        .map(|id| (id - STEAM_ID64_BASE).to_string())
        .collect()
}

/// The `userdata/<id>` folder of the active Steam user.
///
/// Order: the `MostRecent "1"` user in `loginusers.vdf`; else the only
/// `userdata` folder; else the folder whose `localconfig.vdf` changed last.
fn active_user_dir(root: &Path) -> Option<PathBuf> {
    let userdata = root.join("userdata");
    if let Ok(text) = std::fs::read_to_string(root.join("config").join("loginusers.vdf")) {
        for id in most_recent_account_ids(&text) {
            let dir = userdata.join(&id);
            if dir.is_dir() {
                return Some(dir);
            }
        }
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&userdata)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()) && n != "0")
        })
        .collect();
    match dirs.len() {
        0 => None,
        1 => dirs.pop(),
        _ => {
            let newest = dirs
                .into_iter()
                .filter_map(|d| {
                    let m = std::fs::metadata(d.join("config").join("localconfig.vdf"))
                        .and_then(|m| m.modified())
                        .ok()?;
                    Some((m, d))
                })
                .max_by_key(|(m, _)| *m)
                .map(|(_, d)| d);
            if newest.is_some() {
                log::info!("[library] steam: no MostRecent user, using newest localconfig");
            }
            newest
        }
    }
}

/// What `localconfig.vdf` records for one app.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LocalAppStats {
    playtime_min: Option<u32>,
    last_played_unix: Option<u64>,
}

/// `Playtime` and `LastPlayed` per appid from `localconfig.vdf`.
///
/// `Playtime` is lifetime minutes, account wide. `Playtime2wks` is not read:
/// Steam leaves it stale while the client is idle. `LaunchOptions` is dropped
/// by the parser.
fn parse_local_stats(localconfig: &str) -> HashMap<u32, LocalAppStats> {
    let mut out = HashMap::new();
    let Some(doc) = parse_vdf(localconfig) else {
        return out;
    };
    let apps = doc.path(&["UserLocalConfigStore", "Software", "Valve", "Steam", "apps"]);
    for (id, app) in apps.map(Vdf::entries).unwrap_or_default() {
        let Ok(appid) = id.parse::<u32>() else {
            continue;
        };
        let num = |k: &str| app.get(k).and_then(Vdf::as_str)?.trim().parse::<u64>().ok();
        out.insert(
            appid,
            LocalAppStats {
                playtime_min: num("Playtime").and_then(|v| u32::try_from(v).ok()),
                last_played_unix: num("LastPlayed").filter(|&t| t > 0),
            },
        );
    }
    out
}

/// Appids with `hidden "1"` in `sharedconfig.vdf`.
fn parse_hidden_apps(sharedconfig: &str) -> std::collections::HashSet<u32> {
    let Some(doc) = parse_vdf(sharedconfig) else {
        return Default::default();
    };
    let apps = doc.path(&[
        "UserRoamingConfigStore",
        "Software",
        "Valve",
        "Steam",
        "apps",
    ]);
    apps.map(Vdf::entries)
        .unwrap_or_default()
        .iter()
        .filter(|(_, app)| app.get("hidden").and_then(Vdf::as_str).map(str::trim) == Some("1"))
        .filter_map(|(id, _)| id.parse().ok())
        .collect()
}

/// Fill playtime, last played and hidden on the installed Steam entries from
/// the active user's local files. Local reads only. Logs counts, never values.
fn apply_local_steam_data(root: &Path, entries: &mut [LibraryEntry]) {
    let user = active_user_dir(root);
    let stats = user
        .as_ref()
        .and_then(|u| std::fs::read_to_string(u.join("config").join("localconfig.vdf")).ok())
        .map(|t| parse_local_stats(&t))
        .unwrap_or_default();
    let hidden = user
        .as_ref()
        .and_then(|u| {
            std::fs::read_to_string(u.join("7").join("remote").join("sharedconfig.vdf")).ok()
        })
        .map(|t| parse_hidden_apps(&t))
        .unwrap_or_default();
    apply_local_data(entries, &stats, &hidden);
    log::info!(
        "[library] steam local data: {} installed, {} with playtime, {} hidden",
        entries.len(),
        entries.iter().filter(|e| e.playtime_min.is_some()).count(),
        entries.iter().filter(|e| e.hidden).count()
    );
}

/// Merge parsed local data into installed entries. The `localconfig` last
/// played wins. The `appmanifest` value stays when `localconfig` has none.
fn apply_local_data(
    entries: &mut [LibraryEntry],
    stats: &HashMap<u32, LocalAppStats>,
    hidden: &std::collections::HashSet<u32>,
) {
    for e in entries
        .iter_mut()
        .filter(|e| e.source == LibrarySource::Steam)
    {
        let Ok(appid) = e.external_id.parse::<u32>() else {
            continue;
        };
        if let Some(s) = stats.get(&appid) {
            e.playtime_min = s.playtime_min;
            if s.last_played_unix.is_some() {
                e.last_played_unix = s.last_played_unix;
            }
        }
        e.hidden = hidden.contains(&appid);
    }
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
        if matches!(key.as_str(), "appid" | "name" | "installdir" | "lastplayed") {
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
    // `LastOwner` sits in the same file and is deliberately never read.
    let last_played_unix = fields
        .get("lastplayed")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&t| t > 0);
    Some(LibraryEntry {
        source: LibrarySource::Steam,
        external_id: appid.to_string(),
        name,
        playtime_min: None,
        last_played_unix,
        hidden: false,
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
        playtime_min: None,
        last_played_unix: None,
        hidden: false,
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
                playtime_min: None,
                last_played_unix: None,
                hidden: false,
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
            playtime_min: None,
            last_played_unix: None,
            hidden: false,
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
            playtime_min: None,
            last_played_unix: None,
            hidden: false,
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

    const LOCALCONFIG: &str = r#"
"UserLocalConfigStore"
{
	"Software"
	{
		"Valve"
		{
			"Steam"
			{
				"apps"
				{
					"730"
					{
						"Playtime"		"249008"
						"Playtime2wks"		"77"
						"LastPlayed"		"1760000000"
						"LaunchOptions"		"-secret-token-value"
					}
					"620"
					{
						"Playtime"		"95"
						"LastPlayed"		"0"
					}
					"440"
					{
						"cloud"
						{
							"last_sync_state"		"synchronized"
						}
					}
				}
			}
		}
	}
}
"#;

    const LOGINUSERS: &str = r#"
"users"
{
	"76561198000000001"
	{
		"AccountName"		"first"
		"MostRecent"		"0"
	}
	"76561198000000002"
	{
		"AccountName"		"second"
		"MostRecent"		"1"
	}
}
"#;

    const SHAREDCONFIG: &str = r#"
"UserRoamingConfigStore"
{
	"Software"
	{
		"Valve"
		{
			"Steam"
			{
				"apps"
				{
					"620"
					{
						"hidden"		"1"
					}
					"730"
					{
						"LastPlayed"		"1"
					}
				}
			}
		}
	}
}
"#;

    #[test]
    fn local_stats_pass_minutes_through() {
        let stats = parse_local_stats(LOCALCONFIG);
        assert_eq!(stats[&730].playtime_min, Some(249_008));
        assert_eq!(stats[&730].last_played_unix, Some(1_760_000_000));
        assert_eq!(stats[&620].playtime_min, Some(95));
        // LastPlayed "0" means never.
        assert_eq!(stats[&620].last_played_unix, None);
        assert_eq!(stats[&440], LocalAppStats::default());
    }

    #[test]
    fn parser_drops_launch_options() {
        let doc = parse_vdf(LOCALCONFIG).expect("parses");
        let app = doc
            .path(&[
                "UserLocalConfigStore",
                "Software",
                "Valve",
                "Steam",
                "apps",
                "730",
            ])
            .expect("app 730");
        assert!(app.get("LaunchOptions").is_none());
        assert!(app.get("Playtime").is_some());
    }

    #[test]
    fn active_user_is_the_most_recent_one() {
        // 76561198000000002 - 76561197960265728 = 39734274
        assert_eq!(most_recent_account_ids(LOGINUSERS), vec!["39734274"]);
    }

    #[test]
    fn hidden_flag_is_true_only_when_set() {
        let hidden = parse_hidden_apps(SHAREDCONFIG);
        assert!(hidden.contains(&620));
        assert!(!hidden.contains(&730));
    }

    #[test]
    fn malformed_files_give_nothing() {
        assert!(parse_local_stats("\"a\" { \"b\"").is_empty());
        assert!(parse_hidden_apps("not vdf").is_empty());
        assert!(most_recent_account_ids("").is_empty());
    }

    #[test]
    fn manifest_last_played_is_the_fallback() {
        let manifest = r#"
"AppState"
{
	"appid"		"620"
	"name"		"Portal 2"
	"installdir"		"Portal 2"
	"LastOwner"		"76561198000000002"
	"LastPlayed"		"1750000000"
}
"#;
        let mut entries = vec![
            parse_app_manifest(manifest, &steamapps()).expect("manifest"),
            parse_app_manifest(MANIFEST, &steamapps()).expect("manifest"),
        ];
        assert_eq!(entries[0].last_played_unix, Some(1_750_000_000));
        assert_eq!(entries[1].last_played_unix, None);

        // localconfig has LastPlayed for 730 only. 620 keeps the manifest value.
        let stats = parse_local_stats(LOCALCONFIG);
        apply_local_data(&mut entries, &stats, &parse_hidden_apps(SHAREDCONFIG));
        assert_eq!(entries[0].last_played_unix, Some(1_750_000_000));
        assert_eq!(entries[0].playtime_min, Some(95));
        assert!(entries[0].hidden);
        assert_eq!(entries[1].last_played_unix, Some(1_760_000_000));
        assert_eq!(entries[1].playtime_min, Some(249_008));
        assert!(!entries[1].hidden);
    }

    #[test]
    fn missing_files_leave_none_and_no_error() {
        let mut entries = vec![parse_app_manifest(MANIFEST, &steamapps()).expect("manifest")];
        apply_local_data(&mut entries, &HashMap::new(), &Default::default());
        assert_eq!(entries[0].playtime_min, None);
        assert_eq!(entries[0].last_played_unix, None);
        assert!(!entries[0].hidden);

        let dir = std::env::temp_dir().join(format!("m3llo-steam-none-{}", std::process::id()));
        apply_local_steam_data(&dir, &mut entries);
        assert_eq!(entries[0].playtime_min, None);
    }

    #[test]
    fn active_user_dir_follows_the_rules() {
        let root = std::env::temp_dir().join(format!("m3llo-steam-users-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cfg = |id: &str| root.join("userdata").join(id).join("config");
        std::fs::create_dir_all(cfg("39734273")).expect("mkdir");
        // One folder, no loginusers.vdf: use it.
        assert_eq!(
            active_user_dir(&root),
            Some(root.join("userdata").join("39734273"))
        );
        // Two folders and MostRecent names the second.
        std::fs::create_dir_all(cfg("39734274")).expect("mkdir");
        std::fs::create_dir_all(root.join("config")).expect("mkdir");
        std::fs::write(root.join("config").join("loginusers.vdf"), LOGINUSERS).expect("write");
        assert_eq!(
            active_user_dir(&root),
            Some(root.join("userdata").join("39734274"))
        );
        // MostRecent names a missing folder: newest localconfig wins.
        std::fs::remove_dir_all(root.join("userdata").join("39734274")).expect("rm");
        std::fs::create_dir_all(cfg("39734275")).expect("mkdir");
        std::fs::write(cfg("39734273").join("localconfig.vdf"), "").expect("write");
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(cfg("39734275").join("localconfig.vdf"), "").expect("write");
        assert_eq!(
            active_user_dir(&root),
            Some(root.join("userdata").join("39734275"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Prints counts only. Run: `cargo test -p mello-core --lib
    /// steam_local_scan_counts -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the real Steam install on this machine"]
    fn steam_local_scan_counts() {
        let entries = scan_steam();
        println!(
            "installed={} with_playtime={} with_last_played={} hidden={}",
            entries.len(),
            entries.iter().filter(|e| e.playtime_min.is_some()).count(),
            entries
                .iter()
                .filter(|e| e.last_played_unix.is_some())
                .count(),
            entries.iter().filter(|e| e.hidden).count()
        );
    }

    // ---- local Steam art ----

    fn art_fixture(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("m3llo-steam-art-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("appcache").join("librarycache")).unwrap();
        dir
    }

    fn put(root: &Path, rel: &str) -> PathBuf {
        let path = root.join("appcache").join("librarycache").join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"jpg").unwrap();
        path
    }

    #[test]
    fn steam_art_finds_each_cache_layout() {
        let root = art_fixture("layouts");
        let old = put(&root, "10_header.jpg");
        let flat = put(&root, "20/header.jpg");
        let hashed = put(&root, "30/97fc0b0f/header.jpg");
        let lib_header = put(&root, "40/97fc0b0f/library_header.jpg");
        let portrait = put(&root, "50/library_600x900.jpg");
        assert_eq!(steam_header_art_in(&root, "10"), Some(old));
        assert_eq!(steam_header_art_in(&root, "20"), Some(flat));
        assert_eq!(steam_header_art_in(&root, "30"), Some(hashed));
        assert_eq!(steam_header_art_in(&root, "40"), Some(lib_header));
        assert_eq!(steam_header_art_in(&root, "50"), Some(portrait));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn steam_art_prefers_a_header_over_the_portrait_cover() {
        let root = art_fixture("prefer");
        put(&root, "7/library_600x900.jpg");
        let header = put(&root, "7/abc/header.jpg");
        assert_eq!(steam_header_art_in(&root, "7"), Some(header));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn steam_art_is_none_when_the_cache_has_nothing() {
        let root = art_fixture("none");
        put(&root, "8/logo.png");
        assert_eq!(steam_header_art_in(&root, "8"), None);
        assert_eq!(steam_header_art_in(&root, "9999"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn steam_art_rejects_ids_that_are_not_an_appid() {
        let root = art_fixture("reject");
        put(&root, "1/header.jpg");
        for bad in ["", "..", "../1", "1/../1", "1a", "-1"] {
            assert_eq!(steam_header_art_in(&root, bad), None, "{bad:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_steam_ids_have_local_art() {
        assert_eq!(local_steam_art("epic-fortnite"), None);
        assert_eq!(local_steam_art("counter-strike-2"), None);
        assert_eq!(local_steam_art("steam-"), None);
    }
}
