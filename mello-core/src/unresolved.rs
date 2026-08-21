//! A local tally of executables that looked like games but that nothing could
//! name.
//!
//! These are exactly the rows worth adding to `scripts/exe_mappings.json`, and
//! this is how we find out which ones matter instead of guessing. The curated
//! list was written from knowledge; this replaces that with evidence.
//!
//! It deliberately does **not** phone home. An earlier design had users submit
//! exe→game mappings to a shared table with promotion thresholds and a review
//! queue; that buys little (an unresolved game already gets a session, a
//! consistent id, a name and an icon — only the display name varies between
//! users) and costs a moderation burden plus a way for one person to mislabel
//! a game for every crew. Curating from observed misses reaches the same place
//! with none of that.
//!
//! The file sits beside the other client state and is picked up by the
//! diagnostic capture bundle (spec 15 §8), so a beta user reproducing an issue
//! ships it without a bespoke upload path.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Keep the file small and bounded — it is a curation hint, not an audit log.
const MAX_ENTRIES: usize = 200;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedEntry {
    /// Executable basename, e.g. "night stones.exe".
    pub exe: String,
    /// The install folder's name only — enough to recognise the game and to
    /// write a `path_contains` guard, without carrying a full path that would
    /// include the user's home directory.
    pub folder: String,
    /// Best display name we managed (window title or prettified stem).
    pub name: String,
    /// How many separate runs have hit this.
    pub seen: u32,
    pub last_seen_ms: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    /// Keyed by lowercased exe so repeated sightings merge.
    entries: BTreeMap<String, UnresolvedEntry>,
}

fn store_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("mello").join("unresolved_games.json"))
}

/// The install folder's own name, which is what a curator actually needs.
///
/// Returning only the leaf keeps usernames and drive layouts out of the file:
/// `C:\Users\bob\Games\Night Stones\ns.exe` yields `Night Stones`.
pub fn folder_of(exe_path: &str) -> String {
    // Split on both separators rather than using `std::path`, which treats a
    // backslash as one only on Windows. Two reasons: a macOS build must still
    // parse a path that was recorded on Windows, and the separators genuinely
    // mix in the wild — Steam reports its own root with forward slashes
    // (`c:/program files (x86)/steam`), which is why `library::normalize`
    // handles both as well.
    exe_path
        .rsplit(['\\', '/'])
        .nth(1)
        .unwrap_or_default()
        .to_string()
}

/// Note that `exe` could not be resolved. Best-effort and cheap: called once
/// per executable per run, never in a hot loop.
pub fn record(exe: &str, exe_path: &str, name: &str, now_ms: i64) {
    let Some(path) = store_path() else {
        return;
    };
    let mut store: Store = std::fs::read_to_string(&path)
        .ok()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();

    let key = exe.to_lowercase();
    let entry = store.entries.entry(key).or_insert_with(|| UnresolvedEntry {
        exe: exe.to_string(),
        folder: folder_of(exe_path),
        name: name.to_string(),
        seen: 0,
        last_seen_ms: now_ms,
    });
    entry.seen = entry.seen.saturating_add(1);
    entry.last_seen_ms = now_ms;

    // Bounded: drop the least-seen entry rather than growing without limit.
    if store.entries.len() > MAX_ENTRIES {
        if let Some(worst) = store
            .entries
            .iter()
            .min_by_key(|(_, e)| (e.seen, e.last_seen_ms))
            .map(|(k, _)| k.clone())
        {
            store.entries.remove(&worst);
        }
    }

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&store) {
        let _ = std::fs::write(&path, json);
    }
}

/// Everything recorded so far, most-seen first. Used by the resolution report
/// and worth surfacing in any future "help us name your games" flow.
pub fn all() -> Vec<UnresolvedEntry> {
    let Some(path) = store_path() else {
        return Vec::new();
    };
    let store: Store = std::fs::read_to_string(&path)
        .ok()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let mut out: Vec<UnresolvedEntry> = store.entries.into_values().collect();
    out.sort_by(|a, b| b.seen.cmp(&a.seen));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_is_the_leaf_directory_only() {
        // A full path would carry the user's home directory into a file we ask
        // people to send us.
        assert_eq!(
            folder_of(r"C:\Users\bob\Games\Night Stones\ns.exe"),
            "Night Stones"
        );
        assert_eq!(folder_of("/home/bob/games/hades/hades"), "hades");
    }

    #[test]
    fn folder_parsing_does_not_depend_on_the_host_platform() {
        // This passed on Windows and failed on macOS, because `std::path`
        // treats a backslash as a separator only on Windows. Both spellings
        // must parse the same way everywhere.
        assert_eq!(
            folder_of(r"D:\Steam\common\Portal 2\portal2.exe"),
            "Portal 2"
        );
        assert_eq!(
            folder_of("D:/Steam/common/Portal 2/portal2.exe"),
            "Portal 2"
        );
        // Steam reports its own root with forward slashes on Windows, so a
        // path that mixes both is not hypothetical.
        assert_eq!(
            folder_of(r"c:/program files/steam\common\Rust\rust.exe"),
            "Rust"
        );
    }

    #[test]
    fn folder_of_degenerate_paths_is_empty() {
        assert_eq!(folder_of(""), "");
        assert_eq!(folder_of("ns.exe"), "");
    }

    #[test]
    fn entries_sort_by_how_often_they_were_seen() {
        // `all()` drives curation priority, so ordering is the point.
        let mut store = Store::default();
        for (exe, seen) in [("rare.exe", 1u32), ("common.exe", 40), ("mid.exe", 7)] {
            store.entries.insert(
                exe.to_string(),
                UnresolvedEntry {
                    exe: exe.to_string(),
                    seen,
                    ..Default::default()
                },
            );
        }
        let mut out: Vec<UnresolvedEntry> = store.entries.into_values().collect();
        out.sort_by(|a, b| b.seen.cmp(&a.seen));
        assert_eq!(out[0].exe, "common.exe");
        assert_eq!(out[2].exe, "rare.exe");
    }
}
