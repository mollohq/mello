//! The testers on the About tab: their Steam avatars, fetched when the tab is
//! opened and kept on disk.
//!
//! A tester's public Steam profile answers `<profile url>?xml=1` with XML that
//! has an `avatarFull` element, the URL of the 184 px image on Steam's CDN.
//! The image is saved as `<dir>/<slug>.jpg`. Nothing is sent: no account, no
//! cookie, only the public profile address the About tab already links to.
//!
//! Cache rules:
//! - The file's modified time is the time of its last check. A file younger
//!   than [`REVALIDATE_AFTER`] is used as it is and Steam is not asked. An
//!   older file is still used, and one fetch replaces it, so Steam is asked
//!   once a week per tester.
//! - A private profile, a missing avatar or an error is remembered in memory
//!   for [`NEGATIVE_TTL`]. The tab then shows the monogram.
//! - Nothing is fetched before the About tab is opened.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// Steam Community. `MELLO_STEAM_COMMUNITY_BASE` replaces it (dev and e2e).
pub const DEFAULT_BASE: &str = "https://steamcommunity.com";
/// A cached avatar is fetched again after this long.
pub const REVALIDATE_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);
/// A private profile or an error is not asked again for this long.
pub const NEGATIVE_TTL: Duration = Duration::from_secs(24 * 3600);
/// Largest avatar kept. The 184 px JPEG is a few KB.
const MAX_BYTES: usize = 1024 * 1024;

/// One tester: how the name is spelled, the file slug and the profile path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tester {
    pub name: &'static str,
    pub slug: &'static str,
    /// The path of the Steam profile, with both slashes.
    pub path: &'static str,
}

impl Tester {
    /// The profile page the avatar opens in the browser.
    pub fn profile_url(&self) -> String {
        format!("{DEFAULT_BASE}{}", self.path)
    }
}

/// The seven, in the order the About tab shows them.
pub const TESTERS: [Tester; 7] = [
    Tester {
        name: "Ostkatt",
        slug: "ostkatt",
        path: "/id/Larzon/",
    },
    Tester {
        name: "Propotato",
        slug: "propotato",
        path: "/id/martin1337maden/",
    },
    Tester {
        name: "xtrÖm",
        slug: "xtrom",
        path: "/id/xtroem/",
    },
    Tester {
        name: "Erle",
        slug: "erle",
        path: "/id/erlexd/",
    },
    Tester {
        name: "Halvtokig",
        slug: "halvtokig",
        path: "/profiles/76561197974140093/",
    },
    Tester {
        name: "LipzzarN",
        slug: "lipzzarn",
        path: "/profiles/76561198156554449/",
    },
    Tester {
        name: "Keyden",
        slug: "keyden",
        path: "/profiles/76561198173235561/",
    },
];

/// What the cache knows about one tester, without any I/O beyond a file stat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// On disk and checked in the last week, or a check failed in the last day.
    Fresh(PathBuf),
    /// On disk, older than a week. Use it and fetch again.
    Stale(PathBuf),
    /// Not on disk, and a private profile or an error is remembered.
    Negative,
    /// Not on disk and not asked yet.
    Unknown,
}

/// The result of one fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// A new avatar was written.
    Hit(PathBuf),
    /// The profile is private, has no avatar, or the body is not an image.
    Miss,
    /// Network or server error.
    Error,
}

/// Disk cache, negative cache and downloader of the tester avatars.
pub struct TesterAvatars {
    base: Option<String>,
    dir: PathBuf,
    negative: Mutex<HashMap<String, Instant>>,
    inflight: Mutex<HashSet<String>>,
    http: reqwest::Client,
}

static GLOBAL: OnceLock<Arc<TesterAvatars>> = OnceLock::new();

/// Set the process-wide store. The client calls this once at start. A second
/// call is ignored.
pub fn init(dir: PathBuf) -> Arc<TesterAvatars> {
    GLOBAL
        .get_or_init(|| Arc::new(TesterAvatars::new(&base_from_env(), dir)))
        .clone()
}

/// The process-wide store. Before [`init`] it has no base and asks for nothing.
pub fn global() -> Arc<TesterAvatars> {
    GLOBAL
        .get_or_init(|| Arc::new(TesterAvatars::disabled()))
        .clone()
}

/// `MELLO_STEAM_COMMUNITY_BASE` when set and not empty, else [`DEFAULT_BASE`].
pub fn base_from_env() -> String {
    std::env::var("MELLO_STEAM_COMMUNITY_BASE")
        .ok()
        .map(|b| b.trim().trim_end_matches('/').to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE.to_string())
}

/// The text of `<avatarFull>`, with or without CDATA.
fn avatar_full(xml: &str) -> Option<String> {
    let start = xml.find("<avatarFull>")? + "<avatarFull>".len();
    let end = start + xml[start..].find("</avatarFull>")?;
    let inner = xml[start..end].trim();
    let inner = inner
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(inner);
    let url = inner.trim();
    (!url.is_empty()).then(|| url.to_string())
}

fn looks_like_image(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xD8, 0xFF]) || bytes.starts_with(&[0x89, b'P', b'N', b'G'])
}

impl TesterAvatars {
    /// A store that asks `base` and keeps files in `dir`.
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

    /// A store with no server and no files. Tests use it so a test never
    /// reaches the network or the cache of the machine it runs on.
    pub fn disabled() -> Self {
        let mut s = Self::new("", PathBuf::new());
        s.base = None;
        s
    }

    /// True when the store has a server to ask.
    pub fn enabled(&self) -> bool {
        self.base.is_some()
    }

    fn image_path(&self, slug: &str) -> Option<PathBuf> {
        let known = TESTERS.iter().any(|t| t.slug == slug);
        (self.enabled() && known).then(|| self.dir.join(format!("{slug}.jpg")))
    }

    fn is_negative(&self, slug: &str) -> bool {
        let mut neg = self.negative.lock().unwrap_or_else(|e| e.into_inner());
        match neg.get(slug) {
            Some(at) if at.elapsed() < NEGATIVE_TTL => true,
            Some(_) => {
                neg.remove(slug);
                false
            }
            None => false,
        }
    }

    fn remember_negative(&self, slug: &str) {
        self.negative
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(slug.to_string(), Instant::now());
    }

    /// What the cache holds for the slug. A file stat at most.
    pub fn lookup(&self, slug: &str) -> Lookup {
        let Some(path) = self.image_path(slug) else {
            return Lookup::Negative;
        };
        if let Ok(meta) = std::fs::metadata(&path) {
            let age = meta
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .unwrap_or_default();
            return if age < REVALIDATE_AFTER || self.is_negative(slug) {
                Lookup::Fresh(path)
            } else {
                Lookup::Stale(path)
            };
        }
        if self.is_negative(slug) {
            Lookup::Negative
        } else {
            Lookup::Unknown
        }
    }

    /// Mark the slug as being fetched. False when it already is. Pair with
    /// [`TesterAvatars::finish`].
    pub fn begin(&self, slug: &str) -> bool {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(slug.to_string())
    }

    /// Clear the mark of [`TesterAvatars::begin`].
    pub fn finish(&self, slug: &str) {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(slug);
    }

    /// Resolve the avatar URL from the profile XML, download it and save it.
    pub async fn fetch(&self, slug: &str) -> Fetched {
        let (Some(base), Some(path)) = (self.base.as_ref(), self.image_path(slug)) else {
            return Fetched::Miss;
        };
        let Some(tester) = TESTERS.iter().find(|t| t.slug == slug) else {
            return Fetched::Miss;
        };
        let xml_url = format!("{base}{}?xml=1", tester.path);
        let xml = match self.get(&xml_url).await {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => {
                log::debug!("[testers] profile request failed: {e}");
                self.remember_negative(slug);
                return Fetched::Error;
            }
        };
        let Some(avatar_url) = avatar_full(&xml) else {
            log::debug!("[testers] a profile has no avatar");
            self.remember_negative(slug);
            return Fetched::Miss;
        };
        // The real service is https only. A plain http server is a test or a
        // dev base, and then the base itself is plain http.
        if !(avatar_url.starts_with("https://")
            || (avatar_url.starts_with("http://") && base.starts_with("http://")))
        {
            self.remember_negative(slug);
            return Fetched::Miss;
        }
        let body = match self.get(&avatar_url).await {
            Ok(b) => b,
            Err(e) => {
                log::debug!("[testers] avatar request failed: {e}");
                self.remember_negative(slug);
                return Fetched::Error;
            }
        };
        if !looks_like_image(&body) {
            self.remember_negative(slug);
            return Fetched::Miss;
        }
        if write_atomic(&path, &body).is_err() {
            return Fetched::Error;
        }
        log::debug!("[testers] avatar stored, {} bytes", body.len());
        Fetched::Hit(path)
    }

    async fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        let resp = self.http.get(url).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("status {}", resp.status().as_u16()));
        }
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        if bytes.len() > MAX_BYTES {
            return Err("too large".into());
        }
        Ok(bytes.to_vec())
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

/// A small Steam Community for tests: serves profile XML and images and
/// counts requests. Used by the unit tests here and by the client harness.
#[doc(hidden)]
pub mod fake {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// The running fake. Dropping it stops the server.
    pub struct FakeSteam {
        pub base: String,
        seen: Arc<Mutex<Vec<String>>>,
        files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        server: Arc<tiny_http::Server>,
    }

    impl FakeSteam {
        /// Start on a free local port.
        pub fn start() -> Self {
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind fake steam"));
            let port = server
                .server_addr()
                .to_ip()
                .map(|a| a.port())
                .expect("ip addr");
            let files: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
            let seen: Arc<Mutex<Vec<String>>> = Arc::default();
            let (s, f, se) = (server.clone(), files.clone(), seen.clone());
            std::thread::spawn(move || {
                for req in s.incoming_requests() {
                    let url = req.url().to_string();
                    let body = f.lock().unwrap().get(&url).cloned();
                    se.lock().unwrap().push(url);
                    let resp = match body {
                        Some(b) => tiny_http::Response::from_data(b).boxed(),
                        None => tiny_http::Response::from_string("not found")
                            .with_status_code(404)
                            .boxed(),
                    };
                    let _ = req.respond(resp);
                }
            });
            Self {
                base: format!("http://127.0.0.1:{port}"),
                seen,
                files,
                server,
            }
        }

        /// Serve a public profile at `profile_path` whose avatar is `image`.
        pub fn put_profile(&self, profile_path: &str, image: Vec<u8>) {
            let image_path = format!("/avatars{}full.jpg", profile_path);
            let xml = format!(
                "<?xml version=\"1.0\"?><profile><steamID><![CDATA[x]]></steamID>\
                 <avatarFull><![CDATA[{}{}]]></avatarFull></profile>",
                self.base, image_path
            );
            let mut files = self.files.lock().unwrap();
            files.insert(format!("{profile_path}?xml=1"), xml.into_bytes());
            files.insert(image_path, image);
        }

        /// Serve a private profile: XML with no avatar.
        pub fn put_private_profile(&self, profile_path: &str) {
            self.files.lock().unwrap().insert(
                format!("{profile_path}?xml=1"),
                b"<?xml version=\"1.0\"?><response><error><![CDATA[private]]></error></response>"
                    .to_vec(),
            );
        }

        /// How many requests there were in all.
        pub fn requests(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    impl Drop for FakeSteam {
        fn drop(&mut self) {
            self.server.unblock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeSteam;
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("m3llo-testers-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn jpeg_bytes() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
        v.extend(std::iter::repeat_n(7u8, 32));
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
    fn the_seven_names_are_spelled_as_given() {
        let names: Vec<&str> = TESTERS.iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            [
                "Ostkatt",
                "Propotato",
                "xtrÖm",
                "Erle",
                "Halvtokig",
                "LipzzarN",
                "Keyden"
            ]
        );
        assert_eq!(
            TESTERS[0].profile_url(),
            "https://steamcommunity.com/id/Larzon/"
        );
        assert_eq!(
            TESTERS[6].profile_url(),
            "https://steamcommunity.com/profiles/76561198173235561/"
        );
        let slugs: HashSet<_> = TESTERS.iter().map(|t| t.slug).collect();
        assert_eq!(slugs.len(), 7);
    }

    #[test]
    fn avatar_full_reads_cdata_and_plain_text() {
        assert_eq!(
            avatar_full("<a><avatarFull><![CDATA[https://x/y.jpg]]></avatarFull></a>").as_deref(),
            Some("https://x/y.jpg")
        );
        assert_eq!(
            avatar_full("<avatarFull> https://x/z.jpg </avatarFull>").as_deref(),
            Some("https://x/z.jpg")
        );
        assert_eq!(avatar_full("<error>private</error>"), None);
        assert_eq!(avatar_full("<avatarFull></avatarFull>"), None);
    }

    #[test]
    fn a_fetch_saves_the_avatar_and_the_next_lookup_is_fresh() {
        let steam = FakeSteam::start();
        steam.put_profile(TESTERS[0].path, jpeg_bytes());
        let store = TesterAvatars::new(&steam.base, dir("hit"));
        assert_eq!(store.lookup("ostkatt"), Lookup::Unknown);
        let got = run(store.fetch("ostkatt"));
        assert!(matches!(got, Fetched::Hit(_)), "{got:?}");
        assert!(matches!(store.lookup("ostkatt"), Lookup::Fresh(_)));
        assert_eq!(steam.requests(), 2, "the XML and the image");
    }

    #[test]
    fn a_file_older_than_a_week_is_stale() {
        let steam = FakeSteam::start();
        steam.put_profile(TESTERS[1].path, jpeg_bytes());
        let store = TesterAvatars::new(&steam.base, dir("stale"));
        let Fetched::Hit(path) = run(store.fetch("propotato")) else {
            panic!("a hit");
        };
        let old = SystemTime::now() - REVALIDATE_AFTER - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(matches!(store.lookup("propotato"), Lookup::Stale(_)));
    }

    #[test]
    fn a_private_profile_is_a_miss_and_is_not_asked_again() {
        let steam = FakeSteam::start();
        steam.put_private_profile(TESTERS[2].path);
        let store = TesterAvatars::new(&steam.base, dir("private"));
        assert_eq!(run(store.fetch("xtrom")), Fetched::Miss);
        assert_eq!(store.lookup("xtrom"), Lookup::Negative);
    }

    #[test]
    fn an_unknown_slug_and_a_disabled_store_ask_for_nothing() {
        let steam = FakeSteam::start();
        let store = TesterAvatars::new(&steam.base, dir("unknown"));
        assert_eq!(run(store.fetch("../etc")), Fetched::Miss);
        assert_eq!(TesterAvatars::disabled().lookup("erle"), Lookup::Negative);
        assert_eq!(steam.requests(), 0);
    }
}
