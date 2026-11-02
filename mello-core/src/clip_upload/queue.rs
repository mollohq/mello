//! The persisted queue of game clips that are not up (plans/GAME-CLIPS.md
//! 6.3): each clip with its upload id and its done parts, so a client restart
//! goes on from the last part. It uses the JSON file pattern of
//! `pending_session_ends`, in the same folder.
//!
//! Rules:
//! - A clip belongs to the user who clipped it. Only that user's session
//!   uploads it.
//! - The queue holds at most [`MAX_ENTRIES`]. The oldest clips drop first.
//! - A clip older than [`MAX_AGE_MS`] drops. Its local file stays.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::crew_events::CompletedPart;

/// Queue size cap. The oldest clips drop first.
pub const MAX_ENTRIES: usize = 50;
/// A clip that is not up after this time drops (7 days).
pub const MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// One game clip on its way up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingClip {
    pub user_id: String,
    pub crew_id: String,
    pub clip_id: String,
    #[serde(default)]
    pub game: String,
    pub mp4_path: String,
    pub poster_path: String,
    pub duration_seconds: f64,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub has_voice: bool,
    pub created_ms: i64,
    /// `post_clip` accepted the clip.
    #[serde(default)]
    pub posted: bool,
    /// The poster is up and `clip_upload_complete` set its URL.
    #[serde(default)]
    pub poster_done: bool,
    /// The multipart upload of the MP4. Empty until `clip_upload_url`.
    #[serde(default)]
    pub upload_id: String,
    #[serde(default)]
    pub done_parts: Vec<CompletedPart>,
}

impl PendingClip {
    /// The poster goes before any video part (G1), also before an older
    /// clip's video.
    fn needs_poster(&self) -> bool {
        !self.posted || !self.poster_done
    }
}

/// The queue and its backing file.
#[derive(Debug)]
pub struct UploadQueue {
    path: Option<PathBuf>,
    entries: Vec<PendingClip>,
}

impl UploadQueue {
    /// Open the queue in the state folder. A second client on the PC
    /// (`MELLO_SESSION_KEY`) gets its own file.
    pub fn open_default(now_ms: i64) -> Self {
        let name = match std::env::var("MELLO_SESSION_KEY") {
            Ok(key) if !key.is_empty() => format!("game_clip_uploads.{key}.json"),
            _ => "game_clip_uploads.json".to_string(),
        };
        let path = crate::session_store::store_dir().map(|d| d.join(name));
        Self::open(path, now_ms)
    }

    /// Open the queue at `path` and drop clips older than [`MAX_AGE_MS`].
    /// An unreadable file gives an empty queue.
    pub fn open(path: Option<PathBuf>, now_ms: i64) -> Self {
        let entries = path.as_deref().map(read_entries).unwrap_or_default();
        let mut q = Self { path, entries };
        if q.prune(now_ms) {
            q.save();
        }
        q
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Add a clip. A clip with the same id is replaced.
    pub fn push(&mut self, clip: PendingClip) {
        self.entries.retain(|c| c.clip_id != clip.clip_id);
        self.entries.push(clip);
        self.entries.sort_by_key(|c| c.created_ms);
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            log::warn!("[clip-upload] queue over {MAX_ENTRIES}, dropping {excess} oldest");
            self.entries.drain(..excess);
        }
        self.save();
    }

    /// The clip to work on for `user_id`: the oldest that still needs its
    /// post or its poster, else the oldest.
    pub fn next_for(&self, user_id: &str) -> Option<PendingClip> {
        let mine = || self.entries.iter().filter(|c| c.user_id == user_id);
        mine()
            .find(|c| c.needs_poster())
            .or_else(|| mine().next())
            .cloned()
    }

    /// Store the new state of a clip that is in the queue.
    pub fn update(&mut self, clip: &PendingClip) {
        if let Some(entry) = self.entries.iter_mut().find(|c| c.clip_id == clip.clip_id) {
            *entry = clip.clone();
            self.save();
        }
    }

    pub fn remove(&mut self, clip_id: &str) {
        let before = self.entries.len();
        self.entries.retain(|c| c.clip_id != clip_id);
        if self.entries.len() != before {
            self.save();
        }
    }

    /// Drop clips older than [`MAX_AGE_MS`]. Returns true when any dropped.
    fn prune(&mut self, now_ms: i64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|c| now_ms - c.created_ms <= MAX_AGE_MS);
        let dropped = before - self.entries.len();
        if dropped > 0 {
            log::info!("[clip-upload] dropped {dropped} clip(s) older than 7 days");
        }
        dropped > 0
    }

    /// Best-effort write. A failed write costs the restart survival of the
    /// queue, never the upload in flight.
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
            log::warn!("[clip-upload] could not persist the queue: {e}");
        }
    }
}

fn read_entries(path: &Path) -> Vec<PendingClip> {
    let Ok(data) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str(&data).unwrap_or_else(|e| {
        log::warn!("[clip-upload] unreadable queue file, starting empty: {e}");
        Vec::new()
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn clip(id: &str, user: &str, created_ms: i64) -> PendingClip {
        PendingClip {
            user_id: user.into(),
            crew_id: "crew".into(),
            clip_id: id.into(),
            game: "Hearthstone".into(),
            mp4_path: format!("{id}.mp4"),
            poster_path: format!("{id}.jpg"),
            duration_seconds: 30.0,
            width: 1280,
            height: 720,
            has_voice: true,
            created_ms,
            posted: false,
            poster_done: false,
            upload_id: String::new(),
            done_parts: Vec::new(),
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mello-clip-upload-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("queue.json")
    }

    #[test]
    fn the_queue_survives_a_restart_with_its_parts() {
        let path = temp_path("restart");
        let mut q = UploadQueue::open(Some(path.clone()), 1_000);
        let mut c = clip("a", "bob", 1_000);
        q.push(c.clone());
        c.posted = true;
        c.poster_done = true;
        c.upload_id = "up-1".into();
        c.done_parts.push(CompletedPart {
            part_number: 1,
            etag: "\"e1\"".into(),
        });
        q.update(&c);

        let q = UploadQueue::open(Some(path.clone()), 2_000);
        assert_eq!(q.next_for("bob"), Some(c));
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn a_poster_goes_before_an_older_clips_video() {
        let mut q = UploadQueue::open(None, 0);
        let mut old = clip("old", "bob", 1);
        old.posted = true;
        old.poster_done = true;
        q.push(old.clone());
        assert_eq!(q.next_for("bob").map(|c| c.clip_id), Some("old".into()));
        q.push(clip("new", "bob", 2));
        assert_eq!(q.next_for("bob").map(|c| c.clip_id), Some("new".into()));
    }

    #[test]
    fn only_the_clippers_session_uploads_a_clip() {
        let mut q = UploadQueue::open(None, 0);
        q.push(clip("a", "alice", 1));
        assert_eq!(q.next_for("bob"), None);
        assert!(q.next_for("alice").is_some());
    }

    #[test]
    fn old_clips_drop_and_the_cap_drops_the_oldest() {
        let path = temp_path("prune");
        let mut q = UploadQueue::open(Some(path.clone()), 0);
        q.push(clip("old", "bob", 0));
        let q = UploadQueue::open(Some(path.clone()), MAX_AGE_MS + 1);
        assert_eq!(q.len(), 0);

        let mut q = UploadQueue::open(None, 0);
        for i in 0..=MAX_ENTRIES as i64 {
            q.push(clip(&format!("c{i}"), "bob", i));
        }
        assert_eq!(q.len(), MAX_ENTRIES);
        assert_eq!(q.next_for("bob").map(|c| c.clip_id), Some("c1".into()));
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn remove_takes_the_clip_out() {
        let mut q = UploadQueue::open(None, 0);
        q.push(clip("a", "bob", 1));
        q.remove("a");
        assert_eq!(q.len(), 0);
    }
}
