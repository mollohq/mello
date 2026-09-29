//! The names the UI shows for crew members, keyed by user ID.
//!
//! A realtime channel presence event and a realtime chat message carry the user
//! ID and the random Nakama username. They do not carry the display name (#84).
//! `MemberNames` maps a user ID to the display name. `list_group_users` fills it
//! when a crew is selected. A user who is not in it (a member who joined the
//! crew after that) is fetched once with `GET /v2/user`, then kept. So a chat
//! message never costs a round trip of its own.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{RwLock, RwLockReadGuard};

use super::types::ApiUsers;

/// The session token, shared by `NakamaClient` and the WS reader task. A token
/// refresh replaces it in place, so the reader always uses the current token.
pub(crate) type SharedToken = Arc<std::sync::RwLock<Option<String>>>;

/// The time limit for one user lookup. The WS reader waits for it, so it is short.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// The name to show for a user: the display name, or the username when the
/// display name is empty.
pub(crate) fn shown_name(display_name: &str, username: &str) -> String {
    if display_name.is_empty() {
        username.to_string()
    } else {
        display_name.to_string()
    }
}

/// Gets the name to show for one user from the server.
#[async_trait::async_trait]
pub(crate) trait UserLookup: Send + Sync {
    /// Returns the name to show for `user_id`, or `None` when the lookup fails.
    async fn shown_name(&self, user_id: &str) -> Option<String>;
}

/// user_id -> name to show. Shared by `NakamaClient` and the WS reader task.
#[derive(Clone)]
pub(crate) struct MemberNames {
    names: Arc<RwLock<HashMap<String, String>>>,
    lookup: Arc<dyn UserLookup>,
}

impl MemberNames {
    pub(crate) fn new(lookup: Arc<dyn UserLookup>) -> Self {
        Self {
            names: Arc::new(RwLock::new(HashMap::new())),
            lookup,
        }
    }

    /// The cached names, for code that resolves many messages at once.
    pub(crate) async fn read(&self) -> RwLockReadGuard<'_, HashMap<String, String>> {
        self.names.read().await
    }

    pub(crate) async fn insert(&self, user_id: String, name: String) {
        self.names.write().await.insert(user_id, name);
    }

    /// Returns the name to show for `user_id`. On a cache miss, looks the user
    /// up once and keeps the result. When the lookup fails, returns `fallback`
    /// (the username from the event) and keeps nothing, so a later event tries again.
    pub(crate) async fn resolve(&self, user_id: &str, fallback: &str) -> String {
        if user_id.is_empty() {
            return fallback.to_string();
        }
        if let Some(name) = self.names.read().await.get(user_id) {
            return name.clone();
        }
        match self.lookup.shown_name(user_id).await {
            Some(name) => {
                log::info!("member name resolved: {} -> {}", user_id, name);
                self.insert(user_id.to_string(), name.clone()).await;
                name
            }
            None => {
                log::warn!(
                    "member name lookup failed for {}; showing the username",
                    user_id
                );
                fallback.to_string()
            }
        }
    }

    /// Looks up each user ID that is not cached, so a following [`Self::read`]
    /// can resolve it. Used for mentioned users, who can be outside the crew
    /// list (a member who left, or history loaded before the crew list).
    pub(crate) async fn ensure(&self, user_ids: &[String]) {
        let missing: std::collections::BTreeSet<&str> = {
            let names = self.names.read().await;
            user_ids
                .iter()
                .map(String::as_str)
                .filter(|id| !id.is_empty() && !names.contains_key(*id))
                .collect()
        };
        for id in missing {
            self.resolve(id, "").await;
        }
    }
}

/// `UserLookup` over Nakama's `GET /v2/user?ids=`.
pub(crate) struct HttpUserLookup {
    pub(crate) http: reqwest::Client,
    pub(crate) http_base: String,
    pub(crate) token: SharedToken,
}

#[async_trait::async_trait]
impl UserLookup for HttpUserLookup {
    async fn shown_name(&self, user_id: &str) -> Option<String> {
        let token = self
            .token
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        let url = format!(
            "{}/v2/user?ids={}",
            self.http_base,
            urlencoding::encode(user_id)
        );
        let resp = match self
            .http
            .get(&url)
            .bearer_auth(&token)
            .timeout(LOOKUP_TIMEOUT)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                log::warn!("GET /v2/user for {} failed: {}", user_id, e);
                return None;
            }
        };
        if !resp.status().is_success() {
            log::warn!("GET /v2/user for {} failed: {}", user_id, resp.status());
            return None;
        }
        let users: ApiUsers = match resp.json().await {
            Ok(u) => u,
            Err(e) => {
                log::warn!("GET /v2/user for {}: bad response: {}", user_id, e);
                return None;
            }
        };
        let user = users.users?.into_iter().find(|u| u.id == user_id)?;
        let name = shown_name(
            user.display_name.as_deref().unwrap_or_default(),
            user.username.as_deref().unwrap_or_default(),
        );
        (!name.is_empty()).then_some(name)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::sync::Mutex;

    /// A `UserLookup` with fixed answers that records each call.
    #[derive(Default)]
    pub(crate) struct FakeLookup {
        pub(crate) names: HashMap<String, String>,
        pub(crate) calls: Mutex<Vec<String>>,
    }

    impl FakeLookup {
        pub(crate) fn with(names: &[(&str, &str)]) -> Arc<Self> {
            Arc::new(Self {
                names: names
                    .iter()
                    .map(|(id, n)| (id.to_string(), n.to_string()))
                    .collect(),
                calls: Mutex::new(Vec::new()),
            })
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl UserLookup for FakeLookup {
        async fn shown_name(&self, user_id: &str) -> Option<String> {
            self.calls.lock().unwrap().push(user_id.to_string());
            self.names.get(user_id).cloned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeLookup;
    use super::*;

    #[test]
    fn shown_name_prefers_the_display_name() {
        assert_eq!(shown_name("Bob", "UXljDftxYv"), "Bob");
        assert_eq!(shown_name("", "UXljDftxYv"), "UXljDftxYv");
    }

    #[tokio::test]
    async fn resolve_looks_up_a_user_once_then_uses_the_cache() {
        let lookup = FakeLookup::with(&[("u-bob", "Bob")]);
        let names = MemberNames::new(lookup.clone());
        assert_eq!(names.resolve("u-bob", "UXljDftxYv").await, "Bob");
        assert_eq!(names.resolve("u-bob", "UXljDftxYv").await, "Bob");
        assert_eq!(lookup.calls(), vec!["u-bob".to_string()]);
    }

    #[tokio::test]
    async fn resolve_does_not_look_up_a_known_user() {
        let lookup = FakeLookup::with(&[]);
        let names = MemberNames::new(lookup.clone());
        names.insert("u-alice".into(), "Alice".into()).await;
        assert_eq!(names.resolve("u-alice", "xYzRandom").await, "Alice");
        assert!(lookup.calls().is_empty());
    }

    #[tokio::test]
    async fn resolve_falls_back_to_the_username_and_retries_after_a_failed_lookup() {
        let lookup = FakeLookup::with(&[]);
        let names = MemberNames::new(lookup.clone());
        assert_eq!(names.resolve("u-bob", "UXljDftxYv").await, "UXljDftxYv");
        assert_eq!(names.resolve("u-bob", "UXljDftxYv").await, "UXljDftxYv");
        assert_eq!(lookup.calls().len(), 2);
    }

    #[tokio::test]
    async fn ensure_looks_up_each_unknown_user_once_and_caches_it() {
        let lookup = FakeLookup::with(&[("u-gone", "Gone Member")]);
        let names = MemberNames::new(lookup.clone());
        names.insert("u-alice".into(), "Alice".into()).await;
        let ids: Vec<String> = ["u-alice", "u-gone", "u-gone", ""]
            .iter()
            .map(|s| s.to_string())
            .collect();
        names.ensure(&ids).await;
        assert_eq!(lookup.calls(), vec!["u-gone".to_string()]);
        assert_eq!(
            names.read().await.get("u-gone").map(String::as_str),
            Some("Gone Member")
        );
    }

    #[tokio::test]
    async fn resolve_does_not_look_up_an_empty_user_id() {
        let lookup = FakeLookup::with(&[]);
        let names = MemberNames::new(lookup.clone());
        assert_eq!(names.resolve("", "someone").await, "someone");
        assert!(lookup.calls().is_empty());
    }
}
