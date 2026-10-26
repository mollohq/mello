//! Installed-library report (plans/next-gen-feed.md section 5, spec 16
//! section 3.5, spec 17 section 3.3).
//!
//! After a library scan, the client may tell the server which games are
//! installed, so the crew feed can show recent games. This module holds the
//! rules and no I/O:
//!
//! - what goes in the report (no hidden games, capped at the server limit);
//! - when to send (consent on, session active, content changed);
//! - when to clear (consent turned off).
//!
//! `Client` calls [`LibraryReporter::plan`] and performs the RPC it returns.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use serde::Serialize;

use crate::library::LibraryEntry;

/// The server rejects a report with more entries than this.
pub const MAX_REPORT_GAMES: usize = 500;

/// One game in a `library_report` payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ReportGame {
    pub game_id: String,
    pub name: String,
    pub launcher: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playtime_min: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_played_unix: Option<u64>,
}

/// One installed game for the settings list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledGame {
    pub game_id: String,
    pub name: String,
    /// Hidden in the Steam library. Never leaves the device.
    pub steam_hidden: bool,
}

/// Installed games for the settings list, sorted by name.
pub fn installed_games(entries: &[LibraryEntry]) -> Vec<InstalledGame> {
    let mut out: Vec<InstalledGame> = entries
        .iter()
        .map(|e| InstalledGame {
            game_id: e.game_id(),
            name: e.name.clone(),
            steam_hidden: e.hidden,
        })
        .collect();
    out.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.game_id.cmp(&b.game_id))
    });
    out
}

/// Build the report from a scan. Games hidden in Steam and games in
/// `hidden_games` never appear. Over the cap, the most recently played games
/// stay. The result is sorted by game id, so equal content gives an equal hash.
pub fn build_report(entries: &[LibraryEntry], hidden_games: &HashSet<String>) -> Vec<ReportGame> {
    let mut games: Vec<ReportGame> = entries
        .iter()
        .filter(|e| !e.hidden)
        .filter(|e| !hidden_games.contains(&e.game_id()))
        .map(|e| ReportGame {
            game_id: e.game_id(),
            name: e.name.clone(),
            launcher: e.source.prefix().to_string(),
            playtime_min: e.playtime_min,
            last_played_unix: e.last_played_unix.filter(|&t| t > 0),
        })
        .collect();
    // The index can hold one id twice, for example a game found by two scans.
    games.sort_by(|a, b| a.game_id.cmp(&b.game_id));
    games.dedup_by(|a, b| a.game_id == b.game_id);
    if games.len() > MAX_REPORT_GAMES {
        games.sort_by(|a, b| {
            b.last_played_unix
                .cmp(&a.last_played_unix)
                .then_with(|| a.game_id.cmp(&b.game_id))
        });
        games.truncate(MAX_REPORT_GAMES);
        games.sort_by(|a, b| a.game_id.cmp(&b.game_id));
    }
    games
}

fn content_hash(games: &[ReportGame]) -> u64 {
    let mut h = DefaultHasher::new();
    games.hash(&mut h);
    h.finish()
}

/// What the server holds for this login, as far as this client knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    /// Nothing known. The next report is sent.
    Unknown,
    /// A report with this content hash was sent.
    Sent(u64),
    /// An empty report cleared the document.
    Cleared,
}

/// Decides when a report goes to the server.
#[derive(Debug)]
pub struct LibraryReporter {
    entries: Option<Vec<LibraryEntry>>,
    held: Held,
    /// Consent turned off and the clearing report is not sent yet.
    clear_pending: bool,
}

impl Default for LibraryReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl LibraryReporter {
    pub fn new() -> Self {
        Self {
            entries: None,
            held: Held::Unknown,
            clear_pending: false,
        }
    }

    /// Store the newest scan.
    pub fn set_scan(&mut self, entries: Vec<LibraryEntry>) {
        self.entries = Some(entries);
    }

    /// The newest scan, if one arrived.
    pub fn scan(&self) -> Option<&[LibraryEntry]> {
        self.entries.as_deref()
    }

    /// Consent turned off. The next plan clears the server document, once.
    pub fn consent_turned_off(&mut self) {
        self.clear_pending = true;
    }

    /// Logout or a login as another account. Forget what the server holds, so
    /// the first plan after the next login sends in full even when the games
    /// are the same. The scan stays: it describes this device, not the account.
    pub fn reset_for_account(&mut self) {
        self.held = Held::Unknown;
    }

    /// A send failed. The next plan tries again.
    pub fn send_failed(&mut self, was_clear: bool) {
        self.held = Held::Unknown;
        if was_clear {
            self.clear_pending = true;
        }
    }

    /// The report to send now, or `None` to send nothing.
    ///
    /// `session_active` is false before login and after logout. Then nothing
    /// is sent and the client forgets what the server holds, so the first scan
    /// after the next login sends in full.
    pub fn plan(
        &mut self,
        share: bool,
        session_active: bool,
        hidden_games: &HashSet<String>,
    ) -> Option<Vec<ReportGame>> {
        if !session_active {
            self.held = Held::Unknown;
            return None;
        }
        if !share {
            let sent_before = matches!(self.held, Held::Sent(_));
            if self.clear_pending || sent_before {
                self.clear_pending = false;
                self.held = Held::Cleared;
                return Some(Vec::new());
            }
            return None;
        }
        // Consent is on: a clear that never went out no longer applies.
        self.clear_pending = false;
        let entries = self.entries.as_ref()?;
        let report = build_report(entries, hidden_games);
        let hash = content_hash(&report);
        if self.held == Held::Sent(hash) {
            return None;
        }
        self.held = Held::Sent(hash);
        Some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::LibrarySource;
    use std::path::PathBuf;

    fn entry(source: LibrarySource, id: &str, name: &str) -> LibraryEntry {
        LibraryEntry {
            source,
            external_id: id.into(),
            name: name.into(),
            install_dir: PathBuf::from(format!("C:\\Games\\{name}")),
            playtime_min: None,
            last_played_unix: None,
            hidden: false,
        }
    }

    fn steam(id: &str, name: &str) -> LibraryEntry {
        entry(LibrarySource::Steam, id, name)
    }

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    fn ids(report: &[ReportGame]) -> Vec<&str> {
        report.iter().map(|g| g.game_id.as_str()).collect()
    }

    #[test]
    fn report_carries_the_fields_and_sorts_by_id() {
        let mut cs = steam("730", "Counter-Strike 2");
        cs.playtime_min = Some(249_008);
        cs.last_played_unix = Some(1_760_000_000);
        let epic = entry(LibrarySource::Epic, "fortnite", "Fortnite");
        let report = build_report(&[steam("9", "Zed"), epic, cs], &none());
        assert_eq!(ids(&report), ["epic-fortnite", "steam-730", "steam-9"]);
        assert_eq!(report[1].playtime_min, Some(249_008));
        assert_eq!(report[1].last_played_unix, Some(1_760_000_000));
        assert_eq!(report[1].launcher, "steam");
        assert_eq!(report[0].playtime_min, None);
    }

    #[test]
    fn zero_last_played_means_none() {
        let mut e = steam("1", "A");
        e.last_played_unix = Some(0);
        assert_eq!(build_report(&[e], &none())[0].last_played_unix, None);
    }

    #[test]
    fn steam_hidden_entries_never_leave() {
        let mut secret = steam("2", "Secret");
        secret.hidden = true;
        let report = build_report(&[steam("1", "Shown"), secret], &none());
        assert_eq!(ids(&report), ["steam-1"]);
    }

    #[test]
    fn user_hidden_games_never_leave() {
        let hidden: HashSet<String> = ["steam-2".to_string()].into();
        let report = build_report(&[steam("1", "A"), steam("2", "B")], &hidden);
        assert_eq!(ids(&report), ["steam-1"]);
    }

    #[test]
    fn report_is_capped_and_keeps_the_most_recent() {
        let mut entries: Vec<LibraryEntry> = (0..MAX_REPORT_GAMES + 5)
            .map(|i| {
                let mut e = steam(&i.to_string(), "G");
                e.last_played_unix = Some(1000 + i as u64);
                e
            })
            .collect();
        // The oldest five fall away.
        entries.reverse();
        let report = build_report(&entries, &none());
        assert_eq!(report.len(), MAX_REPORT_GAMES);
        assert!(!report.iter().any(|g| g.game_id == "steam-0"));
        assert!(report.iter().any(|g| g.game_id == "steam-504"));
    }

    #[test]
    fn duplicate_ids_collapse() {
        let report = build_report(&[steam("1", "A"), steam("1", "A")], &none());
        assert_eq!(report.len(), 1);
    }

    #[test]
    fn payload_omits_absent_figures() {
        let report = build_report(&[steam("1", "A")], &none());
        let json = serde_json::to_value(&report[0]).unwrap();
        assert!(json.get("playtime_min").is_none());
        assert!(json.get("last_played_unix").is_none());
        assert!(json.get("hidden").is_none());
    }

    #[test]
    fn installed_list_is_sorted_and_flags_steam_hidden() {
        let mut b = steam("2", "beta");
        b.hidden = true;
        let list = installed_games(&[b, steam("1", "Alpha")]);
        assert_eq!(list[0].name, "Alpha");
        assert!(!list[0].steam_hidden);
        assert!(list[1].steam_hidden);
    }

    // ---- reporter ----

    #[test]
    fn nothing_is_sent_before_the_first_scan() {
        let mut r = LibraryReporter::new();
        assert!(r.plan(true, true, &none()).is_none());
    }

    #[test]
    fn consent_off_sends_nothing() {
        let mut r = LibraryReporter::new();
        r.set_scan(vec![steam("1", "A")]);
        assert!(r.plan(false, true, &none()).is_none());
        assert!(r.plan(false, true, &none()).is_none());
    }

    #[test]
    fn first_scan_after_login_sends_once() {
        let mut r = LibraryReporter::new();
        r.set_scan(vec![steam("1", "A")]);
        let first = r.plan(true, true, &none()).expect("first report");
        assert_eq!(ids(&first), ["steam-1"]);
        assert!(
            r.plan(true, true, &none()).is_none(),
            "unchanged, no resend"
        );
    }

    #[test]
    fn no_session_sends_nothing_and_the_next_login_sends_in_full() {
        let mut r = LibraryReporter::new();
        r.set_scan(vec![steam("1", "A")]);
        assert!(r.plan(true, false, &none()).is_none());
        assert!(r.plan(true, true, &none()).is_some());
        // Logout, then login again: the server may hold another user's state.
        assert!(r.plan(true, false, &none()).is_none());
        assert!(r.plan(true, true, &none()).is_some());
    }

    #[test]
    fn login_a_logout_login_b_sends_for_b_even_with_the_same_entries() {
        let mut r = LibraryReporter::new();
        r.set_scan(vec![steam("1", "A"), steam("2", "B")]);
        assert!(r.plan(true, true, &none()).is_some(), "login A sends");
        assert!(r.plan(true, true, &none()).is_none(), "unchanged");

        // Logout and the next login happen with no plan call in between: the
        // sender must not rely on a plan with `session_active == false`.
        r.reset_for_account();
        let b = r.plan(true, true, &none()).expect("login B sends");
        assert_eq!(ids(&b), ["steam-1", "steam-2"]);
        assert!(r.plan(true, true, &none()).is_none(), "then quiet again");
    }

    #[test]
    fn a_rescan_with_the_same_content_does_not_resend() {
        let mut r = LibraryReporter::new();
        r.set_scan(vec![steam("1", "A"), steam("2", "B")]);
        assert!(r.plan(true, true, &none()).is_some());
        // Same games, other scan order.
        r.set_scan(vec![steam("2", "B"), steam("1", "A")]);
        assert!(r.plan(true, true, &none()).is_none());
    }

    #[test]
    fn a_changed_set_or_playtime_resends() {
        let mut r = LibraryReporter::new();
        r.set_scan(vec![steam("1", "A")]);
        assert!(r.plan(true, true, &none()).is_some());

        r.set_scan(vec![steam("1", "A"), steam("2", "B")]);
        assert!(r.plan(true, true, &none()).is_some(), "new game");

        let mut played = steam("1", "A");
        played.playtime_min = Some(60);
        r.set_scan(vec![played, steam("2", "B")]);
        assert!(r.plan(true, true, &none()).is_some(), "new playtime");
    }

    #[test]
    fn turning_consent_off_sends_one_empty_report() {
        let mut r = LibraryReporter::new();
        r.set_scan(vec![steam("1", "A")]);
        assert!(r.plan(true, true, &none()).is_some());

        r.consent_turned_off();
        let clear = r.plan(false, true, &none()).expect("empty report");
        assert!(clear.is_empty());
        assert!(r.plan(false, true, &none()).is_none(), "only once");
    }

    #[test]
    fn consent_off_before_any_send_still_clears_once() {
        // The server can hold a document from an earlier run.
        let mut r = LibraryReporter::new();
        r.consent_turned_off();
        assert_eq!(r.plan(false, true, &none()), Some(Vec::new()));
        assert!(r.plan(false, true, &none()).is_none());
    }

    #[test]
    fn a_clear_waits_for_the_session() {
        let mut r = LibraryReporter::new();
        r.consent_turned_off();
        assert!(r.plan(false, false, &none()).is_none());
        assert_eq!(r.plan(false, true, &none()), Some(Vec::new()));
    }

    #[test]
    fn consent_back_on_sends_in_full_again() {
        let mut r = LibraryReporter::new();
        r.set_scan(vec![steam("1", "A")]);
        assert!(r.plan(true, true, &none()).is_some());
        r.consent_turned_off();
        assert!(r.plan(false, true, &none()).is_some());
        assert!(
            r.plan(true, true, &none()).is_some(),
            "cleared doc is refilled"
        );
    }

    #[test]
    fn a_failed_send_is_retried() {
        let mut r = LibraryReporter::new();
        r.set_scan(vec![steam("1", "A")]);
        assert!(r.plan(true, true, &none()).is_some());
        r.send_failed(false);
        assert!(r.plan(true, true, &none()).is_some());

        r.consent_turned_off();
        assert!(r.plan(false, true, &none()).is_some());
        r.send_failed(true);
        assert_eq!(r.plan(false, true, &none()), Some(Vec::new()));
    }

    #[test]
    fn hiding_a_game_removes_it_from_the_next_report() {
        let mut r = LibraryReporter::new();
        r.set_scan(vec![steam("1", "A"), steam("2", "B")]);
        let first = r.plan(true, true, &none()).unwrap();
        assert_eq!(ids(&first), ["steam-1", "steam-2"]);

        let hidden: HashSet<String> = ["steam-2".to_string()].into();
        let next = r.plan(true, true, &hidden).expect("the set changed");
        assert_eq!(ids(&next), ["steam-1"]);

        // Unhiding brings it back.
        let back = r.plan(true, true, &none()).expect("the set changed");
        assert_eq!(ids(&back), ["steam-1", "steam-2"]);
    }

    #[test]
    fn hiding_the_last_game_sends_an_empty_report_not_nothing() {
        let mut r = LibraryReporter::new();
        r.set_scan(vec![steam("1", "A")]);
        assert!(r.plan(true, true, &none()).is_some());
        let hidden: HashSet<String> = ["steam-1".to_string()].into();
        assert_eq!(r.plan(true, true, &hidden), Some(Vec::new()));
    }
}
