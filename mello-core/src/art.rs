//! Game art from the art Worker (`art.m3llo.app`): a download, a disk cache
//! and a negative cache. It is one rung of the tile ladder in the client,
//! after the local Steam art.
//!
//! The request is `GET <base>/<game id>/<slot>`. It carries no user data: no
//! account, no crew, no cookie. Logs hold counts and outcomes, never ids.
//!
//! Cache rules:
//! - The image lives on disk as `<id>.<slot>.jpg` with its `ETag` beside it as
//!   `<id>.<slot>.etag`. The file's modified time is the time of the last
//!   check.
//! - A file younger than [`REVALIDATE_AFTER`] is used as it is. An older file
//!   is still used, and a request with `If-None-Match` checks it again.
//! - A 404 or an error is remembered in memory for [`NEGATIVE_TTL`], so a
//!   missing game costs one request per day and not one per redraw.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

/// The art Worker. `MELLO_ART_BASE` replaces it (dev and e2e).
pub const DEFAULT_BASE: &str = "https://art.m3llo.app";
/// A cached image is checked again with its ETag after this long.
pub const REVALIDATE_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);
/// A 404 or an error is not asked again for this long.
pub const NEGATIVE_TTL: Duration = Duration::from_secs(24 * 3600);
/// Largest body kept. The Worker serves 720p and 1080p JPEGs, well under this.
const MAX_BYTES: usize = 8 * 1024 * 1024;

/// What the art is for. The Worker picks the IGDB size from the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArtSlot {
    /// Tiles: landscape art, drawn at 460 by 215.
    Header,
    /// The live and session posters: landscape art, drawn at 1920 by 620.
    Hero,
    /// The opened recap: the cover.
    Capsule,
    /// The NOW PLAYING pill: the small cover.
    Icon,
}

impl ArtSlot {
    /// The last path segment of the request.
    pub fn as_str(self) -> &'static str {
        match self {
            ArtSlot::Header => "header",
            ArtSlot::Hero => "hero",
            ArtSlot::Capsule => "capsule",
            ArtSlot::Icon => "icon",
        }
    }
}

/// What the cache knows about one game and slot, without any I/O beyond a
/// file stat. Safe to call on the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtLookup {
    /// On disk and checked in the last week.
    Fresh(PathBuf),
    /// On disk, older than a week. Use it and check it again.
    Stale(PathBuf),
    /// Not on disk, and a 404 or an error is remembered.
    Negative,
    /// Not on disk and not asked yet.
    Unknown,
}

/// The result of one download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// A new image was written.
    Hit(PathBuf),
    /// The server said 304: the cached file is current.
    NotModified(PathBuf),
    /// 404, or the body is not an image.
    Miss,
    /// Network or server error.
    Error,
}

/// Disk cache, negative cache and downloader of the art Worker.
pub struct ArtStore {
    base: Option<String>,
    dir: PathBuf,
    negative: Mutex<HashMap<(String, ArtSlot), Instant>>,
    inflight: Mutex<HashSet<String>>,
    http: reqwest::Client,
}

static GLOBAL: OnceLock<Arc<ArtStore>> = OnceLock::new();

/// Set the process-wide store. The client calls this once at start with the
/// app data dir. A second call is ignored.
pub fn init(dir: PathBuf) -> Arc<ArtStore> {
    GLOBAL
        .get_or_init(|| Arc::new(ArtStore::new(&base_from_env(), dir)))
        .clone()
}

/// The process-wide store. Before [`init`] it is a store with no base, which
/// finds nothing and asks for nothing.
pub fn global() -> Arc<ArtStore> {
    GLOBAL
        .get_or_init(|| Arc::new(ArtStore::disabled()))
        .clone()
}

/// `MELLO_ART_BASE` when set and not empty, else [`DEFAULT_BASE`].
pub fn base_from_env() -> String {
    std::env::var("MELLO_ART_BASE")
        .ok()
        .map(|b| b.trim().trim_end_matches('/').to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE.to_string())
}

/// Ids are `steam-440`, `epic-<id>`, `gog-<id>`, `igdb-<id>`. Anything else
/// would be a path in the cache dir or in the URL.
fn id_is_safe(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 72
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn looks_like_image(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xD8, 0xFF]) || bytes.starts_with(&[0x89, b'P', b'N', b'G'])
}

impl ArtStore {
    /// A store that downloads from `base` into `dir`.
    pub fn new(base: &str, dir: PathBuf) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("mello/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self {
            base: Some(base.trim_end_matches('/').to_string()),
            dir,
            negative: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashSet::new()),
            http,
        }
    }

    /// A store that has no server and no files. Tests use it so a test never
    /// reaches the network or the art cache of the machine it runs on.
    pub fn disabled() -> Self {
        let mut s = Self::new("", PathBuf::new());
        s.base = None;
        s
    }

    /// True when the store has a server to ask.
    pub fn enabled(&self) -> bool {
        self.base.is_some()
    }

    /// The request URL, or `None` for an unsafe id or a disabled store.
    pub fn url(&self, id: &str, slot: ArtSlot) -> Option<String> {
        let base = self.base.as_ref()?;
        id_is_safe(id).then(|| format!("{base}/{id}/{}", slot.as_str()))
    }

    fn image_path(&self, id: &str, slot: ArtSlot) -> Option<PathBuf> {
        (self.enabled() && id_is_safe(id))
            .then(|| self.dir.join(format!("{id}.{}.jpg", slot.as_str())))
    }

    fn etag_path(path: &Path) -> PathBuf {
        path.with_extension("etag")
    }

    fn is_negative(&self, id: &str, slot: ArtSlot) -> bool {
        let mut neg = self.negative.lock().unwrap_or_else(|e| e.into_inner());
        match neg.get(&(id.to_string(), slot)) {
            Some(at) if at.elapsed() < NEGATIVE_TTL => true,
            Some(_) => {
                neg.remove(&(id.to_string(), slot));
                false
            }
            None => false,
        }
    }

    fn remember_negative(&self, id: &str, slot: ArtSlot) {
        self.negative
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((id.to_string(), slot), Instant::now());
    }

    /// What the cache holds for the id. A file stat at most.
    pub fn lookup(&self, id: &str, slot: ArtSlot) -> ArtLookup {
        let Some(path) = self.image_path(id, slot) else {
            return ArtLookup::Negative;
        };
        if let Ok(meta) = std::fs::metadata(&path) {
            let age = meta
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .unwrap_or_default();
            // A check that failed in the last day is not repeated: the old
            // file counts as fresh until the negative entry ends.
            return if age < REVALIDATE_AFTER || self.is_negative(id, slot) {
                ArtLookup::Fresh(path)
            } else {
                ArtLookup::Stale(path)
            };
        }
        if self.is_negative(id, slot) {
            ArtLookup::Negative
        } else {
            ArtLookup::Unknown
        }
    }

    /// True when the id has a request in flight, or when it is already marked
    /// as asked. Marks it. Pair with [`ArtStore::finish`].
    pub fn begin(&self, key: &str) -> bool {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string())
    }

    /// Clear the mark of [`ArtStore::begin`].
    pub fn finish(&self, key: &str) {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }

    /// Download one image, or check the cached one with its ETag.
    pub async fn fetch(&self, id: &str, slot: ArtSlot) -> Fetched {
        let (Some(url), Some(path)) = (self.url(id, slot), self.image_path(id, slot)) else {
            return Fetched::Miss;
        };
        let etag_path = Self::etag_path(&path);
        let cached = path.exists();
        let mut req = self.http.get(&url);
        if cached {
            if let Ok(etag) = std::fs::read_to_string(&etag_path) {
                if !etag.trim().is_empty() {
                    req = req.header(reqwest::header::IF_NONE_MATCH, etag.trim());
                }
            }
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                log::debug!("[art] {} request failed: {e}", slot.as_str());
                self.remember_negative(id, slot);
                return Fetched::Error;
            }
        };
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_MODIFIED && cached {
            touch(&path);
            log::debug!("[art] {} not modified", slot.as_str());
            return Fetched::NotModified(path);
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            log::debug!("[art] {} not found", slot.as_str());
            self.remember_negative(id, slot);
            return Fetched::Miss;
        }
        if !status.is_success() {
            log::debug!("[art] {} status {}", slot.as_str(), status.as_u16());
            self.remember_negative(id, slot);
            return Fetched::Error;
        }
        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = match resp.bytes().await {
            Ok(b) if b.len() <= MAX_BYTES => b,
            _ => {
                self.remember_negative(id, slot);
                return Fetched::Error;
            }
        };
        if !looks_like_image(&body) {
            self.remember_negative(id, slot);
            return Fetched::Miss;
        }
        if write_atomic(&path, &body).is_err() {
            return Fetched::Error;
        }
        match etag {
            Some(e) => {
                let _ = std::fs::write(&etag_path, e);
            }
            None => {
                let _ = std::fs::remove_file(&etag_path);
            }
        }
        log::debug!("[art] {} stored, {} bytes", slot.as_str(), body.len());
        Fetched::Hit(path)
    }

    /// Try the ids in order. A cached image ends the walk; a fresh one is not
    /// asked again, a stale one is checked. Returns the id that has art now.
    pub async fn walk(&self, ids: &[String], slot: ArtSlot) -> Option<String> {
        for id in ids {
            match self.lookup(id, slot) {
                ArtLookup::Fresh(_) => return Some(id.clone()),
                ArtLookup::Negative => continue,
                ArtLookup::Stale(_) | ArtLookup::Unknown => {}
            }
            match self.fetch(id, slot).await {
                Fetched::Hit(_) | Fetched::NotModified(_) => return Some(id.clone()),
                Fetched::Miss | Fetched::Error => {
                    // An old file is better than no art when the check fails.
                    if matches!(self.lookup(id, slot), ArtLookup::Stale(_)) {
                        return Some(id.clone());
                    }
                }
            }
        }
        None
    }
}

fn touch(path: &Path) {
    if let Ok(f) = std::fs::File::options().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// A small art server for tests: serves the given images and counts requests.
/// Used by the unit tests here and by the client harness tests.
#[doc(hidden)]
pub mod fake {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// One request the fake saw.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Seen {
        pub path: String,
        pub if_none_match: Option<String>,
        pub status: u16,
    }

    /// The running fake. Dropping it stops the server.
    pub struct FakeArt {
        pub base: String,
        pub seen: Arc<Mutex<Vec<Seen>>>,
        images: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        server: Arc<tiny_http::Server>,
    }

    impl FakeArt {
        /// Start on a free local port.
        pub fn start() -> Self {
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind fake art"));
            let port = server
                .server_addr()
                .to_ip()
                .map(|a| a.port())
                .expect("ip addr");
            let images: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
            let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
            let (s, im, se) = (server.clone(), images.clone(), seen.clone());
            std::thread::spawn(move || {
                for req in s.incoming_requests() {
                    let path = req.url().to_string();
                    let inm = req
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("If-None-Match"))
                        .map(|h| h.value.to_string());
                    let body = im.lock().unwrap().get(&path).cloned();
                    let sum: u64 = body
                        .as_ref()
                        .map_or(0, |b| b.iter().map(|x| u64::from(*x)).sum());
                    let etag = format!("\"etag-{sum}\"");
                    let status = match &body {
                        None => 404,
                        Some(_) if inm.as_deref() == Some(etag.as_str()) => 304,
                        Some(_) => 200,
                    };
                    se.lock().unwrap().push(Seen {
                        path,
                        if_none_match: inm,
                        status,
                    });
                    let etag_h = tiny_http::Header::from_bytes(&b"ETag"[..], etag.as_bytes())
                        .expect("header");
                    let resp = match (status, body) {
                        (200, Some(b)) => tiny_http::Response::from_data(b)
                            .with_header(etag_h)
                            .boxed(),
                        (304, _) => tiny_http::Response::empty(304).with_header(etag_h).boxed(),
                        _ => tiny_http::Response::from_string("not found")
                            .with_status_code(404)
                            .boxed(),
                    };
                    let _ = req.respond(resp);
                }
            });
            Self {
                base: format!("http://127.0.0.1:{port}"),
                seen,
                images,
                server,
            }
        }

        /// Serve `bytes` at `/<id>/<slot>`.
        pub fn put(&self, id: &str, slot: &str, bytes: Vec<u8>) {
            self.images
                .lock()
                .unwrap()
                .insert(format!("/{id}/{slot}"), bytes);
        }

        /// How many requests the path got.
        pub fn count(&self, id: &str, slot: &str) -> usize {
            let p = format!("/{id}/{slot}");
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.path == p)
                .count()
        }
    }

    impl Drop for FakeArt {
        fn drop(&mut self) {
            self.server.unblock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeArt;
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("m3llo-art-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn jpeg_bytes(n: u8) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
        v.extend(std::iter::repeat_n(n, 32));
        v
    }

    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn base_defaults_and_the_env_overrides_it() {
        // One test owns the variable, so parallel tests cannot race on it.
        std::env::remove_var("MELLO_ART_BASE");
        assert_eq!(base_from_env(), "https://art.m3llo.app");
        std::env::set_var("MELLO_ART_BASE", "http://127.0.0.1:8787/");
        assert_eq!(base_from_env(), "http://127.0.0.1:8787");
        std::env::remove_var("MELLO_ART_BASE");
    }

    #[test]
    fn url_has_the_id_and_the_slot_and_refuses_paths() {
        let s = ArtStore::new("http://x.test", dir("url"));
        assert_eq!(
            s.url("steam-440", ArtSlot::Hero).as_deref(),
            Some("http://x.test/steam-440/hero")
        );
        assert!(s.url("../etc", ArtSlot::Header).is_none());
        assert!(s.url("a/b", ArtSlot::Header).is_none());
        assert!(ArtStore::disabled()
            .url("steam-440", ArtSlot::Icon)
            .is_none());
    }

    #[test]
    fn a_hit_is_written_and_then_found_on_disk() {
        let fake = FakeArt::start();
        fake.put("steam-440", "header", jpeg_bytes(1));
        let s = ArtStore::new(&fake.base, dir("hit"));
        assert_eq!(s.lookup("steam-440", ArtSlot::Header), ArtLookup::Unknown);
        let got = run(s.fetch("steam-440", ArtSlot::Header));
        assert!(matches!(got, Fetched::Hit(_)), "{got:?}");
        assert!(matches!(
            s.lookup("steam-440", ArtSlot::Header),
            ArtLookup::Fresh(_)
        ));
        // Another slot of the same game is its own file.
        assert_eq!(s.lookup("steam-440", ArtSlot::Hero), ArtLookup::Unknown);
    }

    #[test]
    fn a_404_is_remembered_and_the_walk_skips_it() {
        let fake = FakeArt::start();
        fake.put("steam-2", "header", jpeg_bytes(2));
        let s = ArtStore::new(&fake.base, dir("neg"));
        let ids = vec!["igdb-1".to_string(), "steam-2".to_string()];
        assert_eq!(
            run(s.walk(&ids, ArtSlot::Header)).as_deref(),
            Some("steam-2")
        );
        assert_eq!(s.lookup("igdb-1", ArtSlot::Header), ArtLookup::Negative);
        assert_eq!(fake.count("igdb-1", "header"), 1);
        // A second walk does not ask for the missing id again.
        let _ = run(s.walk(&ids[..1], ArtSlot::Header));
        assert_eq!(fake.count("igdb-1", "header"), 1);
    }

    #[test]
    fn a_negative_entry_ends_after_a_day() {
        let s = ArtStore::new("http://x.test", dir("ttl"));
        s.negative
            .lock()
            .unwrap()
            .insert(("steam-9".into(), ArtSlot::Icon), Instant::now());
        assert_eq!(s.lookup("steam-9", ArtSlot::Icon), ArtLookup::Negative);
        let old = Instant::now()
            .checked_sub(NEGATIVE_TTL + Duration::from_secs(1))
            .expect("clock");
        s.negative
            .lock()
            .unwrap()
            .insert(("steam-9".into(), ArtSlot::Icon), old);
        assert_eq!(s.lookup("steam-9", ArtSlot::Icon), ArtLookup::Unknown);
    }

    #[test]
    fn an_old_file_is_stale_and_the_etag_makes_it_a_304() {
        let fake = FakeArt::start();
        fake.put("steam-440", "hero", jpeg_bytes(3));
        let s = ArtStore::new(&fake.base, dir("etag"));
        let Fetched::Hit(path) = run(s.fetch("steam-440", ArtSlot::Hero)) else {
            panic!("expected a hit");
        };
        // Fresh: lookup does not ask for a check.
        assert!(matches!(
            s.lookup("steam-440", ArtSlot::Hero),
            ArtLookup::Fresh(_)
        ));
        // Age the file by eight days.
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(8 * 24 * 3600))
            .unwrap();
        assert!(matches!(
            s.lookup("steam-440", ArtSlot::Hero),
            ArtLookup::Stale(_)
        ));
        let got = run(s.fetch("steam-440", ArtSlot::Hero));
        assert!(matches!(got, Fetched::NotModified(_)), "{got:?}");
        let seen = fake.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].if_none_match, None);
        assert!(seen[1].if_none_match.is_some());
        assert_eq!(seen[1].status, 304);
        // The check made it fresh again.
        assert!(matches!(
            s.lookup("steam-440", ArtSlot::Hero),
            ArtLookup::Fresh(_)
        ));
    }

    #[test]
    fn a_changed_image_replaces_the_cached_one() {
        let fake = FakeArt::start();
        fake.put("steam-440", "hero", jpeg_bytes(3));
        let s = ArtStore::new(&fake.base, dir("change"));
        let Fetched::Hit(path) = run(s.fetch("steam-440", ArtSlot::Hero)) else {
            panic!("hit");
        };
        fake.put("steam-440", "hero", jpeg_bytes(4));
        let got = run(s.fetch("steam-440", ArtSlot::Hero));
        assert!(matches!(got, Fetched::Hit(_)), "{got:?}");
        assert_eq!(std::fs::read(path).unwrap()[10], 4);
    }

    #[test]
    fn a_body_that_is_not_an_image_is_a_miss() {
        let fake = FakeArt::start();
        fake.put("steam-5", "header", b"<html>not art</html>".to_vec());
        let s = ArtStore::new(&fake.base, dir("bad"));
        assert_eq!(run(s.fetch("steam-5", ArtSlot::Header)), Fetched::Miss);
        assert_eq!(s.lookup("steam-5", ArtSlot::Header), ArtLookup::Negative);
    }

    #[test]
    fn a_dead_server_is_an_error_and_is_remembered() {
        let s = ArtStore::new("http://127.0.0.1:1", dir("dead"));
        assert_eq!(run(s.fetch("steam-1", ArtSlot::Header)), Fetched::Error);
        assert_eq!(s.lookup("steam-1", ArtSlot::Header), ArtLookup::Negative);
    }

    #[test]
    fn the_inflight_mark_lets_one_request_through() {
        let s = ArtStore::new("http://x.test", dir("mark"));
        assert!(s.begin("k"));
        assert!(!s.begin("k"));
        s.finish("k");
        assert!(s.begin("k"));
    }
}
