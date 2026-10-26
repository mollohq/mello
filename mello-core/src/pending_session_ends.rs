//! Persisted queue of `game_session_end` reports that could not be sent.
//!
//! A session ends while no crew is active, the client is offline or the RPC
//! fails. Without this queue the report is dropped and the crew feed never
//! shows the night. The queue sits next to `session_store` in the same
//! directory and uses the same best-effort JSON file pattern.
//!
//! Rules:
//! - Nothing is queued while `share_game_activity` is off.
//! - The queue holds at most [`MAX_ENTRIES`]. The oldest entries drop first.
//! - An entry whose session ended more than [`MAX_AGE_MS`] ago is dropped.
//! - Flush is oldest first. An entry leaves the queue only after the server
//!   accepts it. The request keeps its `session_id`, so the server's
//!   idempotency check absorbs a repeat after a lost answer.

use std::future::Future;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::crew_events::GameSessionEndRequest;

/// Queue size cap. Oldest entries drop first.
pub const MAX_ENTRIES: usize = 200;
/// Entries whose session ended longer ago than this are dropped (30 days).
pub const MAX_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// A flush that fails this many times drops the entry, so one permanent
/// server refusal cannot block the queue.
pub const MAX_ATTEMPTS: u32 = 5;

/// One report waiting to be sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSessionEnd {
    /// The request as built at the end of the session. An empty `crew_id`
    /// means no crew was known; flush fills in the active crew.
    pub request: GameSessionEndRequest,
    /// Failed flush attempts so far.
    #[serde(default)]
    pub attempts: u32,
}

/// The queue and its backing file.
#[derive(Debug)]
pub struct PendingQueue {
    path: Option<PathBuf>,
    entries: Vec<PendingSessionEnd>,
}

impl PendingQueue {
    /// Open the queue in the default state directory, next to `session_store`.
    pub fn open_default() -> Self {
        let path = crate::session_store::store_dir().map(|d| d.join("pending_session_ends.json"));
        Self::open(path, now_ms())
    }

    /// Open the queue at `path` and drop entries older than [`MAX_AGE_MS`].
    /// An unreadable file gives an empty queue.
    pub fn open(path: Option<PathBuf>, now_ms: i64) -> Self {
        let entries = path.as_deref().map(read_entries).unwrap_or_default();
        let mut q = Self { path, entries };
        if q.prune(now_ms) {
            q.save();
        }
        q
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[PendingSessionEnd] {
        &self.entries
    }

    /// Queue a report that could not be sent. Returns false and stores
    /// nothing when sharing is off.
    pub fn push_unsent(
        &mut self,
        share_enabled: bool,
        request: GameSessionEndRequest,
        now_ms: i64,
    ) -> bool {
        if !share_enabled {
            return false;
        }
        self.entries.push(PendingSessionEnd {
            request,
            attempts: 0,
        });
        // Stable: reports with an equal end time keep their arrival order.
        self.entries.sort_by_key(|e| e.request.end_ms);
        self.prune(now_ms);
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            log::warn!("[pending-session-ends] queue over {MAX_ENTRIES}, dropping {excess} oldest");
            self.entries.drain(..excess);
        }
        self.save();
        true
    }

    /// Remove everything, for example when the user turns sharing off.
    pub fn clear(&mut self) {
        if !self.entries.is_empty() {
            log::info!(
                "[pending-session-ends] dropping {} queued report(s)",
                self.entries.len()
            );
            self.entries.clear();
            self.save();
        }
    }

    /// Send queued reports oldest first. `send` returns `Ok` when the server
    /// accepted the report. Stops at the first failure and keeps that entry.
    /// Returns how many reports were sent.
    pub async fn flush<F, Fut>(&mut self, active_crew_id: &str, mut send: F) -> usize
    where
        F: FnMut(GameSessionEndRequest) -> Fut,
        Fut: Future<Output = Result<(), String>>,
    {
        let mut sent = 0;
        while let Some(front) = self.entries.first() {
            let mut req = front.request.clone();
            if req.crew_id.is_empty() {
                req.crew_id = active_crew_id.to_string();
            }
            match send(req).await {
                Ok(()) => {
                    self.entries.remove(0);
                    self.save();
                    sent += 1;
                }
                Err(e) => {
                    let entry = &mut self.entries[0];
                    entry.attempts += 1;
                    log::warn!(
                        "[pending-session-ends] flush failed (attempt {}): {}",
                        entry.attempts,
                        e
                    );
                    if entry.attempts >= MAX_ATTEMPTS {
                        log::warn!("[pending-session-ends] dropping a report after {MAX_ATTEMPTS} failed attempts");
                        self.entries.remove(0);
                    }
                    self.save();
                    break;
                }
            }
        }
        sent
    }

    /// Drop entries older than [`MAX_AGE_MS`]. Returns true when any dropped.
    fn prune(&mut self, now_ms: i64) -> bool {
        let before = self.entries.len();
        // end_ms 0 means unknown; such an entry is kept and ages out by the cap.
        self.entries
            .retain(|e| e.request.end_ms == 0 || now_ms - e.request.end_ms <= MAX_AGE_MS);
        let dropped = before - self.entries.len();
        if dropped > 0 {
            log::info!("[pending-session-ends] dropped {dropped} report(s) older than 30 days");
        }
        dropped > 0
    }

    /// Best-effort write. A failed write costs the restart survival of the
    /// queue, never the live session.
    fn save(&self) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        if self.entries.is_empty() {
            let _ = std::fs::remove_file(path);
            return;
        }
        let Ok(json) = serde_json::to_string(&self.entries) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(path, json) {
            log::warn!("[pending-session-ends] could not persist the queue: {e}");
        }
    }
}

fn read_entries(path: &Path) -> Vec<PendingSessionEnd> {
    let Ok(json) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str(&json) {
        Ok(entries) => entries,
        Err(e) => {
            log::warn!("[pending-session-ends] discarding unreadable queue: {e}");
            Vec::new()
        }
    }
}

/// Current Unix time in ms.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    const DAY: i64 = 24 * 60 * 60 * 1000;
    const NOW: i64 = 1_800_000_000_000;

    /// A unique temp file path; removed by `Drop`.
    struct TempPath(PathBuf);
    impl TempPath {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "mello-pending-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            Self(dir.join("pending_session_ends.json"))
        }
        fn path(&self) -> Option<PathBuf> {
            Some(self.0.clone())
        }
    }
    impl Drop for TempPath {
        fn drop(&mut self) {
            if let Some(dir) = self.0.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    fn req(id: &str, end_ms: i64) -> GameSessionEndRequest {
        GameSessionEndRequest {
            crew_id: String::new(),
            game_name: "Counter-Strike 2".into(),
            game_id: "cs2".into(),
            duration_min: 45,
            active_min: 40,
            igdb_id: 0,
            wins: 0,
            losses: 0,
            draws: 0,
            session_id: id.into(),
            start_ms: end_ms - 45 * 60_000,
            end_ms,
            tz_offset_min: 120,
        }
    }

    fn ids(q: &PendingQueue) -> Vec<String> {
        q.entries()
            .iter()
            .map(|e| e.request.session_id.clone())
            .collect()
    }

    async fn flush_all(q: &mut PendingQueue, crew: &str) -> Vec<GameSessionEndRequest> {
        let seen = std::cell::RefCell::new(Vec::new());
        q.flush(crew, |r| {
            seen.borrow_mut().push(r);
            async { Ok(()) }
        })
        .await;
        seen.into_inner()
    }

    #[test]
    fn queue_survives_a_restart() {
        let t = TempPath::new();
        {
            let mut q = PendingQueue::open(t.path(), NOW);
            let mut r = req("a", NOW - 1000);
            r.crew_id = "crew-1".into();
            assert!(q.push_unsent(true, r.clone(), NOW));
            drop(q);
        }
        let q = PendingQueue::open(t.path(), NOW);
        assert_eq!(q.len(), 1);
        let got = &q.entries()[0].request;
        assert_eq!(got.session_id, "a");
        assert_eq!(got.crew_id, "crew-1");
        assert_eq!(got.end_ms, NOW - 1000, "the original end time is kept");
    }

    #[test]
    fn flush_sends_oldest_first_and_empties_the_queue() {
        let t = TempPath::new();
        let mut q = PendingQueue::open(t.path(), NOW);
        // Pushed out of order on purpose.
        q.push_unsent(true, req("newer", NOW - 1000), NOW);
        q.push_unsent(true, req("oldest", NOW - 3000), NOW);
        q.push_unsent(true, req("middle", NOW - 2000), NOW);

        let sent = tokio_test::block_on(flush_all(&mut q, "crew-9"));
        let order: Vec<_> = sent.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(order, ["oldest", "middle", "newer"]);
        assert!(sent.iter().all(|r| r.crew_id == "crew-9"));
        assert!(q.is_empty());
        assert!(
            PendingQueue::open(t.path(), NOW).is_empty(),
            "removal is persisted"
        );
    }

    #[test]
    fn flush_keeps_a_known_crew_id() {
        let t = TempPath::new();
        let mut q = PendingQueue::open(t.path(), NOW);
        let mut r = req("a", NOW - 1000);
        r.crew_id = "crew-old".into();
        q.push_unsent(true, r, NOW);
        let sent = tokio_test::block_on(flush_all(&mut q, "crew-active"));
        assert_eq!(sent[0].crew_id, "crew-old");
    }

    #[test]
    fn failed_send_stops_the_flush_and_keeps_the_entry() {
        let t = TempPath::new();
        let mut q = PendingQueue::open(t.path(), NOW);
        q.push_unsent(true, req("a", NOW - 3000), NOW);
        q.push_unsent(true, req("b", NOW - 2000), NOW);
        let sent = tokio_test::block_on(q.flush("c", |_| async { Err("offline".to_string()) }));
        assert_eq!(sent, 0);
        assert_eq!(ids(&q), ["a", "b"]);
        assert_eq!(q.entries()[0].attempts, 1);
    }

    #[test]
    fn an_entry_that_keeps_failing_is_dropped() {
        let t = TempPath::new();
        let mut q = PendingQueue::open(t.path(), NOW);
        q.push_unsent(true, req("bad", NOW - 3000), NOW);
        q.push_unsent(true, req("good", NOW - 2000), NOW);
        for _ in 0..MAX_ATTEMPTS {
            tokio_test::block_on(q.flush("c", |_| async { Err("refused".to_string()) }));
        }
        assert_eq!(ids(&q), ["good"]);
    }

    #[test]
    fn cap_drops_the_oldest() {
        let t = TempPath::new();
        let mut q = PendingQueue::open(t.path(), NOW);
        for i in 0..(MAX_ENTRIES as i64 + 5) {
            q.push_unsent(true, req(&format!("s{i}"), NOW - 1_000_000 + i), NOW);
        }
        assert_eq!(q.len(), MAX_ENTRIES);
        assert_eq!(q.entries()[0].request.session_id, "s5", "s0 to s4 dropped");
        let last = format!("s{}", MAX_ENTRIES + 4);
        assert_eq!(q.entries().last().unwrap().request.session_id, last);
    }

    #[test]
    fn entries_older_than_30_days_are_dropped() {
        let t = TempPath::new();
        let mut q = PendingQueue::open(t.path(), NOW);
        q.push_unsent(true, req("fresh", NOW - 29 * DAY), NOW);
        q.push_unsent(true, req("stale", NOW - 31 * DAY), NOW);
        assert_eq!(ids(&q), ["fresh"], "stale is dropped on push");

        // Aging happens across a restart too.
        let later = NOW + 2 * DAY;
        let q = PendingQueue::open(t.path(), later);
        assert!(q.is_empty(), "fresh is now 31 days old");
        assert!(PendingQueue::open(t.path(), later).is_empty());
    }

    #[test]
    fn nothing_is_queued_when_sharing_is_off() {
        let t = TempPath::new();
        let mut q = PendingQueue::open(t.path(), NOW);
        assert!(!q.push_unsent(false, req("a", NOW - 1000), NOW));
        assert!(q.is_empty());
        assert!(!t.0.exists(), "no file is written");
    }

    #[test]
    fn clear_empties_the_queue_and_the_file() {
        let t = TempPath::new();
        let mut q = PendingQueue::open(t.path(), NOW);
        q.push_unsent(true, req("a", NOW - 1000), NOW);
        q.clear();
        assert!(q.is_empty());
        assert!(PendingQueue::open(t.path(), NOW).is_empty());
    }

    #[test]
    fn request_without_the_new_fields_still_parses() {
        let json = r#"{"crew_id":"c","game_name":"G"}"#;
        let r: GameSessionEndRequest = serde_json::from_str(json).unwrap();
        assert_eq!((r.start_ms, r.end_ms, r.tz_offset_min), (0, 0, 0));
    }
}
