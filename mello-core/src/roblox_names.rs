//! Roblox experience names from Roblox's public games API (spec 18 §3.11):
//! a lookup by place id, a disk cache and a negative cache.
//!
//! The Roblox player log carries the place id of an experience, not its
//! name. Two public endpoints give the name. Neither takes an account, a
//! cookie or anything about the user; the place id identifies a public
//! experience.
//!
//! - `GET <apis>/universes/v1/places/<placeId>/universe` answers
//!   `{"universeId": N}`.
//! - `GET <games>/v1/games?universeIds=N` answers
//!   `{"data": [{"id": N, "name": "..."}]}`.
//!
//! Cache rules:
//! - A name lives in `roblox-names.json` in the client's store dir, keyed by
//!   place id, with the time it was fetched. A name younger than
//!   [`REFRESH_AFTER`] is used as it is. An older name is still used, and the
//!   next lookup asks again; a failed refresh keeps the old name.
//! - A miss or an error is remembered in memory for [`NEGATIVE_TTL`], so an
//!   unknown place costs one request per hour and not one per join.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

/// The place-to-universe endpoint's host.
pub const DEFAULT_APIS_BASE: &str = "https://apis.roblox.com";
/// The games endpoint's host.
pub const DEFAULT_GAMES_BASE: &str = "https://games.roblox.com";
/// A cached name is asked again after this long.
pub const REFRESH_AFTER: Duration = Duration::from_secs(30 * 24 * 3600);
/// A miss or an error is not asked again for this long.
pub const NEGATIVE_TTL: Duration = Duration::from_secs(3600);
/// The cache file, in the store dir beside `active_sessions.json`.
const FILE_NAME: &str = "roblox-names.json";
/// Roblox caps an experience name at 50 characters; anything longer is not
/// a name.
const MAX_NAME_CHARS: usize = 100;

/// One cached name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Entry {
    name: String,
    /// Unix ms of the fetch.
    fetched_ms: i64,
}

pub struct RobloxNames {
    apis_base: String,
    games_base: String,
    /// `None` keeps the cache in memory only.
    path: Option<PathBuf>,
    names: Mutex<HashMap<String, Entry>>,
    negative: Mutex<HashMap<String, Instant>>,
    http: reqwest::Client,
}

static GLOBAL: OnceLock<Arc<RobloxNames>> = OnceLock::new();

/// The store the client uses: the default hosts, or `MELLO_ROBLOX_API_BASE`
/// for both (dev and e2e), and the cache file in the store dir.
pub fn global() -> Arc<RobloxNames> {
    GLOBAL
        .get_or_init(|| {
            let (apis, games) = bases_from_env();
            let path = crate::session_store::store_dir().map(|d| d.join(FILE_NAME));
            Arc::new(RobloxNames::new(&apis, &games, path))
        })
        .clone()
}

/// The two hosts, or the one host that `MELLO_ROBLOX_API_BASE` names for
/// both.
pub fn bases_from_env() -> (String, String) {
    match std::env::var("MELLO_ROBLOX_API_BASE") {
        Ok(base) if !base.trim().is_empty() => {
            let base = base.trim().trim_end_matches('/').to_string();
            (base.clone(), base)
        }
        _ => (
            DEFAULT_APIS_BASE.to_string(),
            DEFAULT_GAMES_BASE.to_string(),
        ),
    }
}

/// A place id is digits only. Anything else never reaches the network.
fn id_is_safe(place_id: &str) -> bool {
    !place_id.is_empty() && place_id.len() <= 20 && place_id.bytes().all(|b| b.is_ascii_digit())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

impl RobloxNames {
    pub fn new(apis_base: &str, games_base: &str, path: Option<PathBuf>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        let names = path.as_deref().map(load).unwrap_or_default();
        Self {
            apis_base: apis_base.trim_end_matches('/').to_string(),
            games_base: games_base.trim_end_matches('/').to_string(),
            path,
            names: Mutex::new(names),
            negative: Mutex::new(HashMap::new()),
            http,
        }
    }

    /// The cached name without any I/O. `None` when not cached.
    pub fn cached(&self, place_id: &str) -> Option<String> {
        self.names
            .lock()
            .ok()?
            .get(place_id)
            .map(|e| e.name.clone())
    }

    fn cached_entry(&self, place_id: &str) -> Option<Entry> {
        self.names.lock().ok()?.get(place_id).cloned()
    }

    fn is_negative(&self, place_id: &str) -> bool {
        let Ok(mut neg) = self.negative.lock() else {
            return false;
        };
        match neg.get(place_id) {
            Some(at) if at.elapsed() < NEGATIVE_TTL => true,
            Some(_) => {
                neg.remove(place_id);
                false
            }
            None => false,
        }
    }

    fn remember_negative(&self, place_id: &str) {
        if let Ok(mut neg) = self.negative.lock() {
            neg.insert(place_id.to_string(), Instant::now());
        }
    }

    fn remember(&self, place_id: &str, name: String) {
        let Ok(mut names) = self.names.lock() else {
            return;
        };
        names.insert(
            place_id.to_string(),
            Entry {
                name,
                fetched_ms: now_ms(),
            },
        );
        if let Some(path) = &self.path {
            save(path, &names);
        }
    }

    /// The name of the experience at `place_id`: from the cache, else from
    /// the API. `None` for an unknown place, a bad answer or a network error,
    /// which is then not asked again for an hour.
    pub async fn name(&self, place_id: &str) -> Option<String> {
        if !id_is_safe(place_id) {
            log::debug!("[roblox-names] refusing a place id that is not digits");
            return None;
        }
        let cached = self.cached_entry(place_id);
        if let Some(entry) = &cached {
            let age = Duration::from_millis((now_ms() - entry.fetched_ms).max(0) as u64);
            if age < REFRESH_AFTER {
                return Some(entry.name.clone());
            }
        }
        if self.is_negative(place_id) {
            return cached.map(|e| e.name);
        }
        match self.fetch(place_id).await {
            Some(name) => {
                log::info!("[roblox-names] place {place_id} is {name:?}");
                self.remember(place_id, name.clone());
                Some(name)
            }
            None => {
                self.remember_negative(place_id);
                cached.map(|e| e.name)
            }
        }
    }

    async fn fetch(&self, place_id: &str) -> Option<String> {
        let universe_url = format!("{}/universes/v1/places/{place_id}/universe", self.apis_base);
        let body = self.get(&universe_url).await?;
        let universe_id = parse_universe(&body)?;
        let games_url = format!("{}/v1/games?universeIds={universe_id}", self.games_base);
        let body = self.get(&games_url).await?;
        parse_game_name(&body, universe_id)
    }

    /// The body of a 200, else `None` with a one-line log.
    async fn get(&self, url: &str) -> Option<String> {
        let resp = match self.http.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("[roblox-names] request failed: {e}");
                return None;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            log::info!("[roblox-names] the API answered {status}");
            return None;
        }
        resp.text().await.ok()
    }
}

/// `{"universeId": N}`.
pub(crate) fn parse_universe(body: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("universeId")?.as_u64()
}

/// The `name` of the entry whose `id` is `universe_id` in
/// `{"data": [{"id": N, "name": "..."}]}`.
pub(crate) fn parse_game_name(body: &str, universe_id: u64) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let name = v
        .get("data")?
        .as_array()?
        .iter()
        .find(|g| g.get("id").and_then(|id| id.as_u64()) == Some(universe_id))?
        .get("name")?
        .as_str()?
        .trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return None;
    }
    Some(name.to_string())
}

fn load(path: &Path) -> HashMap<String, Entry> {
    let Ok(bytes) = std::fs::read(path) else {
        return HashMap::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        log::warn!("[roblox-names] cannot read the cache, starting empty: {e}");
        HashMap::new()
    })
}

/// Best-effort: a failed write costs one lookup after the next start.
fn save(path: &Path, names: &HashMap<String, Entry>) {
    let Ok(bytes) = serde_json::to_vec(names) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PLACE: &str = "107778070777162";
    const UNIVERSE: u64 = 10_563_114_921;
    const UNIVERSE_BODY: &str = r#"{"universeId":10563114921}"#;
    const GAMES_BODY: &str = r#"{"data":[{"id":10563114921,"rootPlaceId":107778070777162,"name":"Grow a Garden","description":"x","creator":{"id":1,"name":"c","type":"User"},"playing":12,"visits":3}]}"#;

    /// A fake of both hosts on one port. `known` is false for a place the
    /// API does not know.
    struct FakeRoblox {
        base: String,
        requests: Arc<AtomicUsize>,
        server: Arc<tiny_http::Server>,
    }

    impl FakeRoblox {
        fn start(known: bool) -> Self {
            let server =
                Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind fake roblox"));
            let port = server
                .server_addr()
                .to_ip()
                .map(|a| a.port())
                .expect("ip addr");
            let requests = Arc::new(AtomicUsize::new(0));
            let (s, count) = (server.clone(), requests.clone());
            std::thread::spawn(move || {
                for req in s.incoming_requests() {
                    count.fetch_add(1, Ordering::SeqCst);
                    let url = req.url().to_string();
                    let resp = if !known {
                        tiny_http::Response::from_string("{}")
                            .with_status_code(404)
                            .boxed()
                    } else if url == format!("/universes/v1/places/{PLACE}/universe") {
                        tiny_http::Response::from_string(UNIVERSE_BODY).boxed()
                    } else if url == format!("/v1/games?universeIds={UNIVERSE}") {
                        tiny_http::Response::from_string(GAMES_BODY).boxed()
                    } else {
                        tiny_http::Response::from_string("{}")
                            .with_status_code(404)
                            .boxed()
                    };
                    let _ = req.respond(resp);
                }
            });
            Self {
                base: format!("http://127.0.0.1:{port}"),
                requests,
                server,
            }
        }

        fn requests(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    impl Drop for FakeRoblox {
        fn drop(&mut self) {
            self.server.unblock();
        }
    }

    fn file(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("m3llo-roblox-names-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join(FILE_NAME)
    }

    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn the_two_answers_parse_to_a_name() {
        assert_eq!(parse_universe(UNIVERSE_BODY), Some(UNIVERSE));
        assert_eq!(parse_universe(r#"{"errors":[]}"#), None);
        assert_eq!(
            parse_game_name(GAMES_BODY, UNIVERSE).as_deref(),
            Some("Grow a Garden")
        );
        assert_eq!(parse_game_name(GAMES_BODY, 1), None, "another universe");
        assert_eq!(parse_game_name(r#"{"data":[]}"#, UNIVERSE), None);
        assert_eq!(
            parse_game_name(r#"{"data":[{"id":10563114921,"name":"  "}]}"#, UNIVERSE),
            None
        );
    }

    #[test]
    fn a_place_id_that_is_not_digits_never_reaches_the_network() {
        let fake = FakeRoblox::start(true);
        let s = RobloxNames::new(&fake.base, &fake.base, None);
        assert_eq!(run(s.name("../etc")), None);
        assert_eq!(run(s.name("")), None);
        assert_eq!(fake.requests(), 0);
    }

    #[test]
    fn a_name_is_fetched_once_and_found_on_disk_by_the_next_store() {
        let fake = FakeRoblox::start(true);
        let path = file("hit");
        let s = RobloxNames::new(&fake.base, &fake.base, Some(path.clone()));
        assert_eq!(s.cached(PLACE), None);
        assert_eq!(run(s.name(PLACE)).as_deref(), Some("Grow a Garden"));
        assert_eq!(fake.requests(), 2, "the universe, then the game");
        assert_eq!(run(s.name(PLACE)).as_deref(), Some("Grow a Garden"));
        assert_eq!(fake.requests(), 2, "the second lookup is the cache");

        let again = RobloxNames::new(&fake.base, &fake.base, Some(path.clone()));
        assert_eq!(again.cached(PLACE).as_deref(), Some("Grow a Garden"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_unknown_place_is_asked_once_an_hour() {
        let fake = FakeRoblox::start(false);
        let s = RobloxNames::new(&fake.base, &fake.base, None);
        assert_eq!(run(s.name(PLACE)), None);
        assert_eq!(run(s.name(PLACE)), None);
        assert_eq!(fake.requests(), 1, "the miss is remembered");
    }

    #[test]
    fn a_dead_host_is_a_miss_not_a_panic() {
        let s = RobloxNames::new("http://127.0.0.1:1", "http://127.0.0.1:1", None);
        assert_eq!(run(s.name(PLACE)), None);
    }

    #[test]
    fn an_old_name_is_used_while_the_refresh_fails() {
        let path = file("stale");
        let old = HashMap::from([(
            PLACE.to_string(),
            Entry {
                name: "Old Name".into(),
                fetched_ms: now_ms() - REFRESH_AFTER.as_millis() as i64 - 1,
            },
        )]);
        save(&path, &old);
        let s = RobloxNames::new(
            "http://127.0.0.1:1",
            "http://127.0.0.1:1",
            Some(path.clone()),
        );
        assert_eq!(run(s.name(PLACE)).as_deref(), Some("Old Name"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_env_names_one_host_for_both() {
        let (a, g) = bases_from_env();
        assert_eq!(a, DEFAULT_APIS_BASE);
        assert_eq!(g, DEFAULT_GAMES_BASE);
    }
}
