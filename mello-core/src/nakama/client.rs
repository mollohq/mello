use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::tungstenite::Message;

use super::types::*;
use crate::config::Config;
use crate::crew::{Crew, Member};
use crate::crew_state::CrewState;
use crate::events::{ChatMessage, Event, User};
use crate::presence::{Activity, PresenceStatus};
use crate::{Error, Result};

#[derive(Default)]
struct WsShared {
    channel_id: Option<String>,
    local_user_id: Option<String>,
}

/// Internal signal from a peer, received via Nakama channel message
#[derive(Debug)]
pub struct InternalSignal {
    pub from: String,
    pub payload: String,
}

/// Channel presence change forwarded to the client run loop for voice wiring
#[derive(Debug)]
pub enum InternalPresence {
    Joined { user_id: String },
    Left { user_id: String },
}

pub struct NakamaClient {
    config: Config,
    http: reqwest::Client,
    token: Option<String>,
    refresh_token: Option<String>,
    current_user: Option<User>,
    active_crew_id: Option<String>,
    ws_tx: Option<mpsc::Sender<String>>,
    ws_shared: Arc<RwLock<WsShared>>,
    next_cid: u64,
    signal_rx: Option<mpsc::Receiver<InternalSignal>>,
    signal_tx_template: Option<mpsc::Sender<InternalSignal>>,
    presence_rx: Option<mpsc::Receiver<InternalPresence>>,
    presence_tx_template: Option<mpsc::Sender<InternalPresence>>,
    /// user_id -> display_name cache, shared with the WS reader task
    member_names: Arc<RwLock<HashMap<String, String>>>,
}

impl NakamaClient {
    pub fn new(config: Config) -> Self {
        let (sig_tx, sig_rx) = mpsc::channel(256);
        let (pres_tx, pres_rx) = mpsc::channel(256);
        Self {
            config,
            http: reqwest::Client::new(),
            token: None,
            refresh_token: None,
            current_user: None,
            active_crew_id: None,
            ws_tx: None,
            ws_shared: Arc::new(RwLock::new(WsShared::default())),
            next_cid: 1,
            signal_rx: Some(sig_rx),
            signal_tx_template: Some(sig_tx),
            member_names: Arc::new(RwLock::new(HashMap::new())),
            presence_rx: Some(pres_rx),
            presence_tx_template: Some(pres_tx),
        }
    }

    fn next_cid(&mut self) -> String {
        let cid = self.next_cid.to_string();
        self.next_cid += 1;
        cid
    }

    fn bearer(&self) -> Result<String> {
        self.token.clone().ok_or(Error::NotConnected)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    // --- Authentication ---

    pub async fn login_email(&mut self, email: &str, password: &str) -> Result<User> {
        let url = format!(
            "{}/v2/account/authenticate/email?create=true",
            self.config.http_base()
        );

        let resp = self
            .http
            .post(&url)
            .basic_auth(&self.config.nakama_key, Some(""))
            .json(&serde_json::json!({
                "email": email,
                "password": password
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let err: ApiError = resp.json().await.unwrap_or(ApiError {
                error: Some("Unknown error".into()),
                message: None,
                code: None,
            });
            return Err(Error::AuthFailed(
                err.message.or(err.error).unwrap_or_default(),
            ));
        }

        let session: ApiSession = resp.json().await?;
        self.token = Some(session.token.clone());
        self.refresh_token = session.refresh_token;

        let user = self.get_account().await?;
        self.current_user = Some(user.clone());
        Ok(user)
    }

    /// Exchange a Google authorization code + PKCE verifier for an id_token.
    pub async fn google_exchange_code(&self, code: &str, pkce_verifier: &str) -> Result<String> {
        let google_client_id = self
            .config
            .google_client_id
            .as_deref()
            .ok_or_else(|| Error::AuthFailed("GOOGLE_CLIENT_ID not configured".into()))?;
        let google_client_secret = self
            .config
            .google_client_secret
            .as_deref()
            .ok_or_else(|| Error::AuthFailed("GOOGLE_CLIENT_SECRET not configured".into()))?;

        #[derive(serde::Deserialize)]
        struct TokenResponse {
            id_token: Option<String>,
            error: Option<String>,
            error_description: Option<String>,
        }

        let token_resp = self
            .http
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("code", code),
                ("client_id", google_client_id),
                ("client_secret", google_client_secret),
                ("redirect_uri", crate::oauth::REDIRECT_URI),
                ("grant_type", "authorization_code"),
                ("code_verifier", pkce_verifier),
            ])
            .send()
            .await?;

        let tokens: TokenResponse = token_resp
            .json()
            .await
            .map_err(|_| Error::AuthFailed("Google token exchange failed".into()))?;

        if let Some(err) = tokens.error {
            let desc = tokens.error_description.unwrap_or_default();
            return Err(Error::AuthFailed(format!("Google: {err} — {desc}")));
        }

        tokens
            .id_token
            .ok_or_else(|| Error::AuthFailed("No id_token from Google".into()))
    }

    /// Authenticate with Nakama using a Google id_token (creates or logs into account).
    pub async fn authenticate_google(&mut self, id_token: &str) -> Result<User> {
        let url = format!(
            "{}/v2/account/authenticate/google?create=true",
            self.config.http_base()
        );

        let resp = self
            .http
            .post(&url)
            .basic_auth(&self.config.nakama_key, Some(""))
            .json(&serde_json::json!({ "token": id_token }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let err: ApiError = resp.json().await.unwrap_or(ApiError {
                error: Some("Unknown error".into()),
                message: None,
                code: None,
            });
            return Err(Error::AuthFailed(
                err.message.or(err.error).unwrap_or_default(),
            ));
        }

        let session: ApiSession = resp.json().await?;
        self.token = Some(session.token.clone());
        self.refresh_token = session.refresh_token;

        let user = self.get_account().await?;
        self.current_user = Some(user.clone());
        Ok(user)
    }

    /// Authenticate with a provider token via Nakama's custom auth endpoint.
    /// Used for Discord and Twitch whose tokens are validated by the backend hook.
    pub async fn authenticate_custom(&mut self, token: &str, provider: &str) -> Result<User> {
        let url = format!(
            "{}/v2/account/authenticate/custom?create=true",
            self.config.http_base()
        );

        let resp = self
            .http
            .post(&url)
            .basic_auth(&self.config.nakama_key, Some(""))
            .json(&serde_json::json!({
                "id": token,
                "vars": { "provider": provider }
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let err: ApiError = resp.json().await.unwrap_or(ApiError {
                error: Some("Unknown error".into()),
                message: None,
                code: None,
            });
            return Err(Error::AuthFailed(
                err.message.or(err.error).unwrap_or_default(),
            ));
        }

        let session: ApiSession = resp.json().await?;
        self.token = Some(session.token.clone());
        self.refresh_token = session.refresh_token;

        let user = self.get_account().await?;
        self.current_user = Some(user.clone());
        Ok(user)
    }

    /// Link a Google identity to the current (device-authed) account using an id_token.
    pub async fn link_google(&self, id_token: &str) -> Result<()> {
        let token = self.bearer()?;
        let url = format!("{}/v2/account/link/google", self.config.http_base());

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({ "token": id_token }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let err: ApiError = resp.json().await.unwrap_or(ApiError {
                error: Some("Unknown error".into()),
                message: None,
                code: None,
            });
            return Err(Error::AuthFailed(
                err.message.or(err.error).unwrap_or_default(),
            ));
        }

        Ok(())
    }

    /// Link a custom provider identity (Discord, Twitch) to the current account.
    pub async fn link_custom(&self, token: &str, provider: &str) -> Result<()> {
        let bearer = self.bearer()?;
        let url = format!("{}/v2/account/link/custom", self.config.http_base());

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&bearer)
            .json(&serde_json::json!({
                "id": token,
                "vars": { "provider": provider }
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let err: ApiError = resp.json().await.unwrap_or(ApiError {
                error: Some("Unknown error".into()),
                message: None,
                code: None,
            });
            return Err(Error::AuthFailed(
                err.message.or(err.error).unwrap_or_default(),
            ));
        }

        Ok(())
    }

    /// Returns (User, created) where `created` is true when Nakama just created the account.
    pub async fn authenticate_device(&mut self, device_id: &str) -> Result<(User, bool)> {
        let url = format!(
            "{}/v2/account/authenticate/device?create=true",
            self.config.http_base()
        );

        let resp = self
            .http
            .post(&url)
            .basic_auth(&self.config.nakama_key, Some(""))
            .json(&serde_json::json!({ "id": device_id }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let err: ApiError = resp.json().await.unwrap_or(ApiError {
                error: Some("Unknown error".into()),
                message: None,
                code: None,
            });
            return Err(Error::AuthFailed(
                err.message.or(err.error).unwrap_or_default(),
            ));
        }

        let session: ApiSession = resp.json().await?;
        let created = session.created.unwrap_or(false);
        self.token = Some(session.token.clone());
        self.refresh_token = session.refresh_token;

        let user = self.get_account().await?;
        self.current_user = Some(user.clone());
        Ok((user, created))
    }

    pub async fn refresh_session(&mut self, refresh_token: &str) -> Result<User> {
        let url = format!("{}/v2/account/session/refresh", self.config.http_base());

        let resp = self
            .http
            .post(&url)
            .basic_auth(&self.config.nakama_key, Some(""))
            .json(&serde_json::json!({ "token": refresh_token }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            log::warn!("Refresh failed: {} -- {}", status, body);
            return Err(Error::AuthFailed(format!("refresh failed ({})", status)));
        }

        let session: ApiSession = resp.json().await?;
        self.token = Some(session.token.clone());
        self.refresh_token = session.refresh_token;

        let user = self.get_account().await?;
        self.current_user = Some(user.clone());
        Ok(user)
    }

    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh_token.as_deref()
    }

    async fn get_account(&self) -> Result<User> {
        let token = self.bearer()?;
        let url = format!("{}/v2/account", self.config.http_base());

        let resp = self.http.get(&url).bearer_auth(&token).send().await?;

        if !resp.status().is_success() {
            return Err(Error::Server("Failed to get account".into()));
        }

        let account: ApiAccount = resp.json().await?;
        let api_user = account
            .user
            .ok_or(Error::Internal("No user in account".into()))?;

        let mut tag = String::new();
        if let Some(meta_str) = &api_user.metadata {
            if let Ok(meta) = serde_json::from_str::<UserMetadata>(meta_str) {
                tag = meta.tag.unwrap_or_default();
            }
        }

        Ok(User {
            id: api_user.id,
            username: api_user.username.unwrap_or_default(),
            display_name: api_user.display_name.unwrap_or_default(),
            tag,
        })
    }

    pub fn current_user(&self) -> Option<&User> {
        self.current_user.as_ref()
    }

    // --- WebSocket ---

    pub async fn connect_ws(&mut self, event_tx: std::sync::mpsc::Sender<Event>) -> Result<()> {
        let token = self.bearer()?;
        let url = self.config.ws_url(&token);

        let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| Error::WebSocket(e.to_string()))?;

        let (write, read) = ws_stream.split();

        let (ws_tx, ws_rx) = mpsc::channel::<String>(256);
        self.ws_tx = Some(ws_tx);

        let shared = self.ws_shared.clone();
        if let Some(user) = &self.current_user {
            shared.write().await.local_user_id = Some(user.id.clone());
        }
        let signal_tx = self.signal_tx_template.clone().unwrap();
        let presence_tx = self.presence_tx_template.clone().unwrap();

        tokio::spawn(ws_writer_task(ws_rx, write));
        tokio::spawn(ws_reader_task(
            read,
            event_tx,
            shared,
            signal_tx,
            presence_tx,
            self.member_names.clone(),
        ));

        log::info!("WebSocket connected");
        Ok(())
    }

    /// Take the signal receiver (call once, from the client run loop)
    pub fn take_signal_rx(&mut self) -> Option<mpsc::Receiver<InternalSignal>> {
        self.signal_rx.take()
    }

    /// Take the presence receiver (call once, from the client run loop)
    pub fn take_presence_rx(&mut self) -> Option<mpsc::Receiver<InternalPresence>> {
        self.presence_rx.take()
    }

    async fn ws_send(&self, msg: String) -> Result<()> {
        if let Some(tx) = &self.ws_tx {
            tx.send(msg)
                .await
                .map_err(|e| Error::WebSocket(e.to_string()))?;
        }
        Ok(())
    }

    // --- Crews ---

    pub async fn list_user_groups(&self) -> Result<Vec<Crew>> {
        let token = self.bearer()?;
        let user_id = self
            .current_user
            .as_ref()
            .ok_or(Error::NotConnected)?
            .id
            .clone();
        let url = format!("{}/v2/user/{}/group", self.config.http_base(), user_id);

        let resp = self.http.get(&url).bearer_auth(&token).send().await?;

        if !resp.status().is_success() {
            return Err(Error::Server("Failed to list groups".into()));
        }

        let list: ApiUserGroupList = resp.json().await?;
        let crews = list
            .user_groups
            .unwrap_or_default()
            .into_iter()
            .filter_map(|ug| {
                let g = ug.group?;
                Some(Crew {
                    id: g.id?,
                    name: g.name.unwrap_or_default(),
                    description: g.description.unwrap_or_default(),
                    member_count: g.edge_count.unwrap_or(0),
                    max_members: g.max_count.unwrap_or(6),
                    open: g.open.unwrap_or(false),
                    avatar_url: g.avatar_url.filter(|s| !s.is_empty()),
                })
            })
            .collect();

        Ok(crews)
    }

    /// List open crews via the `discover_crews` RPC (no user session needed).
    /// Returns (crews, next_cursor). next_cursor is None when there are no more pages.
    pub async fn discover_crews_public(
        &self,
        _limit: u32,
        cursor: Option<&str>,
    ) -> Result<(Vec<Crew>, Option<String>)> {
        let url = format!(
            "{}/v2/rpc/discover_crews?http_key={}",
            self.config.http_base(),
            self.config.nakama_http_key,
        );

        let body = match cursor {
            Some(c) => serde_json::json!({ "cursor": c }).to_string(),
            None => String::new(),
        };
        let body = serde_json::Value::String(body);

        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Server(format!(
                "discover_crews RPC failed ({}): {}",
                status, body
            )));
        }

        #[derive(serde::Deserialize)]
        struct RpcResponse {
            payload: String,
        }
        #[derive(serde::Deserialize)]
        struct DiscoverPayload {
            #[serde(default)]
            crews: Vec<DiscoverCrew>,
            #[serde(default)]
            cursor: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct DiscoverCrew {
            id: String,
            #[serde(default)]
            name: String,
            #[serde(default)]
            description: String,
            #[serde(default)]
            member_count: i32,
            #[serde(default)]
            max_members: i32,
            #[serde(default)]
            open: bool,
            #[serde(default)]
            avatar_url: Option<String>,
        }

        let rpc: RpcResponse = resp.json().await?;
        let payload: DiscoverPayload = serde_json::from_str(&rpc.payload)?;

        let next_cursor = payload.cursor.filter(|c| !c.is_empty());
        let crews = payload
            .crews
            .into_iter()
            .map(|c| Crew {
                id: c.id,
                name: c.name,
                description: c.description,
                member_count: c.member_count,
                max_members: c.max_members,
                open: c.open,
                avatar_url: c.avatar_url.filter(|s| !s.is_empty()),
            })
            .collect();

        Ok((crews, next_cursor))
    }

    pub async fn list_groups(&self, limit: u32) -> Result<Vec<Crew>> {
        let token = self.bearer()?;
        let url = format!(
            "{}/v2/group?limit={}&open=true",
            self.config.http_base(),
            limit
        );

        let resp = self.http.get(&url).bearer_auth(&token).send().await?;

        if !resp.status().is_success() {
            return Err(Error::Server("Failed to list groups".into()));
        }

        let list: ApiGroupList = resp.json().await?;
        let crews = list
            .groups
            .unwrap_or_default()
            .into_iter()
            .filter_map(|g| {
                Some(Crew {
                    id: g.id?,
                    name: g.name.unwrap_or_default(),
                    description: g.description.unwrap_or_default(),
                    member_count: g.edge_count.unwrap_or(0),
                    max_members: g.max_count.unwrap_or(6),
                    open: g.open.unwrap_or(false),
                    avatar_url: g.avatar_url.filter(|s| !s.is_empty()),
                })
            })
            .collect();

        Ok(crews)
    }

    pub async fn join_group(&self, group_id: &str) -> Result<()> {
        let token = self.bearer()?;
        let url = format!("{}/v2/group/{}/join", self.config.http_base(), group_id);

        let resp = self.http.post(&url).bearer_auth(&token).send().await?;

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(Error::Server(err_text));
        }

        Ok(())
    }

    pub async fn update_account(&self, display_name: &str) -> Result<()> {
        let token = self.bearer()?;
        let url = format!("{}/v2/account", self.config.http_base());

        let resp = self
            .http
            .put(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "display_name": display_name
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(Error::Server(err_text));
        }

        Ok(())
    }

    pub async fn link_email(&self, email: &str, password: &str) -> Result<()> {
        let token = self.bearer()?;
        let url = format!("{}/v2/account/link/email", self.config.http_base());

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "email": email,
                "password": password
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let err: ApiError = resp.json().await.unwrap_or(ApiError {
                error: Some("Unknown error".into()),
                message: None,
                code: None,
            });
            return Err(Error::AuthFailed(
                err.message.or(err.error).unwrap_or_default(),
            ));
        }

        Ok(())
    }

    /// Returns (Crew, invite_code).
    pub async fn create_crew(
        &self,
        name: &str,
        description: &str,
        open: bool,
        avatar: Option<&str>,
        invite_user_ids: &[String],
    ) -> Result<(Crew, Option<String>)> {
        let token = self.bearer()?;
        let url = format!("{}/v2/rpc/create_crew", self.config.http_base());

        let avatar_len = avatar.map(|a| a.len()).unwrap_or(0);
        log::info!(
            "[nakama] create_crew RPC name={:?} avatar_bytes={} invites={}",
            name,
            avatar_len,
            invite_user_ids.len()
        );

        let payload = serde_json::to_string(&CreateCrewPayload {
            name: name.to_string(),
            description: if description.is_empty() {
                None
            } else {
                Some(description.to_string())
            },
            invite_only: Some(!open),
            avatar: avatar.map(|s| s.to_string()),
            invite_user_ids: invite_user_ids.to_vec(),
        })?;
        let body = serde_json::Value::String(payload);

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            log::error!("[nakama] create_crew RPC failed: {}", err_text);
            return Err(Error::Server(err_text));
        }

        let rpc_resp: ApiRpcResponse = resp.json().await?;
        let raw = rpc_resp.payload.unwrap_or_default();
        log::debug!("[nakama] create_crew RPC response: {}", raw);
        let result: CreateCrewResult = serde_json::from_str(&raw)?;

        let crew = Crew {
            id: result.crew_id,
            name: result.name,
            description: description.to_string(),
            member_count: 1,
            max_members: 6,
            open,
            avatar_url: None,
        };
        Ok((crew, result.invite_code))
    }

    /// Fetch crew avatar via server-side RPC. Works both authed and pre-auth (http_key).
    pub async fn get_crew_avatar(&self, crew_id: &str) -> Result<String> {
        let payload = serde_json::json!({ "crew_id": crew_id });
        let body = serde_json::Value::String(payload.to_string());

        let req = if let Ok(token) = self.bearer() {
            let url = format!("{}/v2/rpc/get_crew_avatar", self.config.http_base());
            self.http.post(&url).bearer_auth(&token).json(&body)
        } else {
            let url = format!(
                "{}/v2/rpc/get_crew_avatar?http_key={}",
                self.config.http_base(),
                self.config.nakama_http_key,
            );
            self.http.post(&url).json(&body)
        };

        let resp = req.send().await?;
        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(Error::Server(err_text));
        }
        let rpc_resp: ApiRpcResponse = resp.json().await?;
        Ok(rpc_resp.payload.unwrap_or_default())
    }

    pub async fn search_users(&self, query: &str) -> Result<Vec<crate::events::UserSearchResult>> {
        let payload = serde_json::json!({ "query": query });
        let resp_str = self.rpc("search_users", &payload).await?;
        let result: SearchUsersResult = serde_json::from_str(&resp_str)?;
        Ok(result
            .users
            .into_iter()
            .map(|u| crate::events::UserSearchResult {
                id: u.id,
                display_name: u.display_name,
                is_friend: u.is_friend,
            })
            .collect())
    }

    pub async fn join_by_invite_code(&self, code: &str) -> Result<(String, String)> {
        let payload = serde_json::json!({ "code": code });
        let resp_str = self.rpc("join_by_invite_code", &payload).await?;
        let result: JoinByInviteCodeResult = serde_json::from_str(&resp_str)?;
        Ok((result.crew_id, result.name))
    }

    // --- Generic RPC ---

    pub async fn rpc(&self, id: &str, payload: &serde_json::Value) -> Result<String> {
        let token = self.bearer()?;
        let url = format!("{}/v2/rpc/{}", self.config.http_base(), id);

        let body = serde_json::Value::String(payload.to_string());

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(Error::Server(err_text));
        }

        let rpc_resp: ApiRpcResponse = resp.json().await?;
        Ok(rpc_resp.payload.unwrap_or_default())
    }

    // --- Storage ---

    /// Read a single object from Nakama storage. Returns the value string.
    pub async fn read_storage(&self, collection: &str, key: &str, user_id: &str) -> Result<String> {
        let token = self.bearer()?;
        let url = format!("{}/v2/storage", self.config.http_base());

        let body = serde_json::json!({
            "object_ids": [{
                "collection": collection,
                "key": key,
                "user_id": user_id,
            }]
        });

        log::debug!("[nakama] read_storage {}/{}/{}", collection, key, user_id);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            log::warn!(
                "[nakama] storage read failed {}/{}: {}",
                collection,
                key,
                err_text
            );
            return Err(Error::Server(format!("storage read failed: {}", err_text)));
        }

        let objects: ApiStorageObjects = resp.json().await?;
        let value = objects
            .objects
            .and_then(|mut v| v.pop())
            .and_then(|o| o.value)
            .unwrap_or_default();
        log::debug!(
            "[nakama] read_storage {}/{} -> {} bytes",
            collection,
            key,
            value.len()
        );
        Ok(value)
    }

    // --- Presence RPCs ---

    pub async fn presence_update(
        &self,
        status: &PresenceStatus,
        activity: Option<&Activity>,
    ) -> Result<()> {
        let payload = serde_json::json!({
            "status": status,
            "activity": activity,
        });
        self.rpc("presence_update", &payload).await?;
        Ok(())
    }

    pub async fn presence_get(&self, user_ids: &[String]) -> Result<String> {
        let payload = serde_json::json!({ "user_ids": user_ids });
        self.rpc("presence_get", &payload).await
    }

    // --- Crew state RPCs ---

    pub async fn set_active_crew(&self, crew_id: &str) -> Result<CrewState> {
        let payload = serde_json::json!({ "crew_id": crew_id });
        let resp = self.rpc("set_active_crew", &payload).await?;

        #[derive(serde::Deserialize)]
        struct Resp {
            state: CrewState,
        }
        let parsed: Resp = serde_json::from_str(&resp)?;
        Ok(parsed.state)
    }

    pub async fn subscribe_sidebar(
        &self,
        crew_ids: &[String],
    ) -> Result<Vec<crate::crew_state::CrewSidebarState>> {
        let payload = serde_json::json!({ "crew_ids": crew_ids });
        let resp = self.rpc("subscribe_sidebar", &payload).await?;

        #[derive(serde::Deserialize)]
        struct Resp {
            crews: Vec<crate::crew_state::CrewSidebarState>,
        }
        let parsed: Resp = serde_json::from_str(&resp)?;
        Ok(parsed.crews)
    }

    // --- Health / version RPCs ---

    pub async fn health_check(&self) -> Result<HealthResponse> {
        let resp = self.rpc("health", &serde_json::json!({})).await?;
        let parsed: HealthResponse = serde_json::from_str(&resp)?;
        Ok(parsed)
    }

    // --- ICE / Voice RPCs ---

    pub async fn get_ice_servers(&self) -> Result<Vec<String>> {
        let resp = self.rpc("get_ice_servers", &serde_json::json!({})).await?;

        #[derive(serde::Deserialize)]
        struct IceServer {
            urls: Vec<String>,
            #[serde(default)]
            username: String,
            #[serde(default)]
            credential: String,
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            ice_servers: Vec<IceServer>,
        }

        let parsed: Resp = serde_json::from_str(&resp)?;
        let mut urls = Vec::new();
        for server in parsed.ice_servers {
            if !server.username.is_empty() && !server.credential.is_empty() {
                // Percent-encode `:` and `@` in credentials so libdatachannel's
                // `user:pass@host` URL parser splits correctly.
                let enc_user = server
                    .username
                    .replace('%', "%25")
                    .replace(':', "%3A")
                    .replace('@', "%40");
                let enc_cred = server
                    .credential
                    .replace('%', "%25")
                    .replace(':', "%3A")
                    .replace('@', "%40");
                for url in &server.urls {
                    if let Some(host) = url.strip_prefix("turn:") {
                        urls.push(format!("turn:{}:{}@{}", enc_user, enc_cred, host));
                    } else if let Some(host) = url.strip_prefix("turns:") {
                        urls.push(format!("turns:{}:{}@{}", enc_user, enc_cred, host));
                    } else {
                        urls.push(url.clone());
                    }
                }
            } else {
                urls.extend(server.urls);
            }
        }
        Ok(urls)
    }

    pub async fn voice_join(
        &self,
        crew_id: &str,
        channel_id: &str,
    ) -> Result<crate::crew_state::VoiceJoinResponse> {
        let payload = serde_json::json!({ "crew_id": crew_id, "channel_id": channel_id });
        let resp = self.rpc("voice_join", &payload).await?;
        let parsed: crate::crew_state::VoiceJoinResponse = serde_json::from_str(&resp)?;
        Ok(parsed)
    }

    pub async fn voice_leave(&self, crew_id: &str) -> Result<()> {
        let payload = serde_json::json!({ "crew_id": crew_id });
        self.rpc("voice_leave", &payload).await?;
        Ok(())
    }

    pub async fn voice_speaking(&self, crew_id: &str, speaking: bool) -> Result<()> {
        let payload = serde_json::json!({ "crew_id": crew_id, "speaking": speaking });
        self.rpc("voice_speaking", &payload).await?;
        Ok(())
    }

    // --- Voice channel CRUD ---

    pub async fn channel_create(
        &self,
        crew_id: &str,
        name: &str,
    ) -> Result<crate::crew_state::VoiceChannelState> {
        let payload = serde_json::json!({ "crew_id": crew_id, "name": name });
        let resp = self.rpc("channel_create", &payload).await?;
        log::debug!("[nakama] channel_create response: {}", resp);
        let channel: crate::crew_state::VoiceChannelState = serde_json::from_str(&resp)?;
        Ok(channel)
    }

    pub async fn channel_rename(&self, crew_id: &str, channel_id: &str, name: &str) -> Result<()> {
        let payload =
            serde_json::json!({ "crew_id": crew_id, "channel_id": channel_id, "name": name });
        self.rpc("channel_rename", &payload).await?;
        Ok(())
    }

    pub async fn channel_delete(&self, crew_id: &str, channel_id: &str) -> Result<()> {
        let payload = serde_json::json!({ "crew_id": crew_id, "channel_id": channel_id });
        self.rpc("channel_delete", &payload).await?;
        Ok(())
    }

    pub async fn channel_list(
        &self,
        crew_id: &str,
    ) -> Result<Vec<crate::crew_state::VoiceChannelState>> {
        let payload = serde_json::json!({ "crew_id": crew_id });
        let resp = self.rpc("channel_list", &payload).await?;
        let list: Vec<crate::crew_state::VoiceChannelState> = serde_json::from_str(&resp)?;
        Ok(list)
    }

    // --- Channel ---

    pub async fn join_crew_channel(&mut self, crew_id: &str) -> Result<()> {
        self.active_crew_id = Some(crew_id.to_string());

        {
            let mut shared = self.ws_shared.write().await;
            shared.channel_id = None;
        }

        let cid = self.next_cid();
        let msg = serde_json::json!({
            "cid": cid,
            "channel_join": {
                "target": crew_id,
                "type": 3,
                "persistence": true,
                "hidden": false
            }
        })
        .to_string();

        self.ws_send(msg).await
    }

    pub async fn leave_crew_channel(&mut self) -> Result<()> {
        let channel_id = self.ws_shared.read().await.channel_id.clone();
        if let Some(channel_id) = channel_id {
            let cid = self.next_cid();
            let msg = serde_json::json!({
                "cid": cid,
                "channel_leave": {
                    "channel_id": channel_id
                }
            })
            .to_string();
            self.ws_send(msg).await?;
        }

        self.active_crew_id = None;
        self.ws_shared.write().await.channel_id = None;
        Ok(())
    }

    // --- Chat ---

    pub async fn send_chat_message(&self, text: &str) -> Result<()> {
        let envelope = crate::chat::MessageEnvelope::text(text, None);
        let json = serde_json::to_string(&envelope).map_err(|e| Error::Server(e.to_string()))?;
        self.send_raw_chat_message(&json).await
    }

    /// Send a pre-serialized JSON envelope as a channel message.
    pub async fn send_raw_chat_message(&self, content_json: &str) -> Result<()> {
        let channel_id = self
            .ws_shared
            .read()
            .await
            .channel_id
            .clone()
            .ok_or(Error::NotConnected)?;

        let msg = serde_json::json!({
            "channel_message_send": {
                "channel_id": channel_id,
                "content": content_json
            }
        })
        .to_string();

        self.ws_send(msg).await
    }

    /// Update an existing channel message (edit).
    pub async fn update_chat_message(&self, message_id: &str, content_json: &str) -> Result<()> {
        let channel_id = self
            .ws_shared
            .read()
            .await
            .channel_id
            .clone()
            .ok_or(Error::NotConnected)?;

        let msg = serde_json::json!({
            "channel_message_update": {
                "channel_id": channel_id,
                "message_id": message_id,
                "content": content_json
            }
        })
        .to_string();

        self.ws_send(msg).await
    }

    /// Remove a channel message (soft delete).
    pub async fn remove_chat_message(&self, message_id: &str) -> Result<()> {
        let channel_id = self
            .ws_shared
            .read()
            .await
            .channel_id
            .clone()
            .ok_or(Error::NotConnected)?;

        let msg = serde_json::json!({
            "channel_message_remove": {
                "channel_id": channel_id,
                "message_id": message_id
            }
        })
        .to_string();

        self.ws_send(msg).await
    }

    pub async fn list_channel_messages(
        &self,
        channel_id: &str,
        limit: u32,
    ) -> Result<Vec<ChatMessage>> {
        let (msgs, _) = self
            .list_channel_messages_with_cursor(channel_id, limit, None)
            .await?;
        Ok(msgs)
    }

    /// Fetch message history with optional pagination cursor.
    /// Returns (messages, next_cursor).
    pub async fn list_channel_messages_with_cursor(
        &self,
        channel_id: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<(Vec<ChatMessage>, Option<String>)> {
        let token = self.bearer()?;
        let mut url = format!(
            "{}/v2/channel/{}?limit={}&forward=false",
            self.config.http_base(),
            channel_id,
            limit
        );
        if let Some(c) = cursor {
            if !c.is_empty() {
                url.push_str(&format!("&cursor={}", c));
            }
        }

        let resp = self.http.get(&url).bearer_auth(&token).send().await?;

        if !resp.status().is_success() {
            return Err(Error::Server("Failed to list channel messages".into()));
        }

        let list: ApiChannelMessageList = resp.json().await?;
        let next_cursor = list.next_cursor.clone().filter(|c| !c.is_empty());
        let names = self.member_names.read().await;
        Ok((parse_channel_messages(list, &names), next_cursor))
    }

    // --- Crew members ---

    pub async fn list_group_users(&self, group_id: &str) -> Result<Vec<Member>> {
        let token = self.bearer()?;
        let url = format!("{}/v2/group/{}/user", self.config.http_base(), group_id);

        let resp = self.http.get(&url).bearer_auth(&token).send().await?;

        if !resp.status().is_success() {
            return Err(Error::Server("Failed to list group users".into()));
        }

        let list: ApiGroupUserList = resp.json().await?;
        let members: Vec<Member> = list
            .group_users
            .unwrap_or_default()
            .into_iter()
            .filter_map(|gu| {
                let u = gu.user?;
                Some(Member {
                    id: u.id,
                    username: u.username.unwrap_or_default(),
                    display_name: u.display_name.unwrap_or_default(),
                    online: u.online.unwrap_or(false),
                })
            })
            .collect();

        {
            let mut names = self.member_names.write().await;
            for m in &members {
                let name = if m.display_name.is_empty() {
                    &m.username
                } else {
                    &m.display_name
                };
                names.insert(m.id.clone(), name.clone());
            }
        }

        Ok(members)
    }

    // --- Status ---

    pub async fn follow_users(&self, user_ids: &[String]) -> Result<()> {
        let msg = serde_json::json!({
            "status_follow": {
                "user_ids": user_ids
            }
        })
        .to_string();
        self.ws_send(msg).await
    }

    pub async fn channel_id(&self) -> Option<String> {
        self.ws_shared.read().await.channel_id.clone()
    }

    pub fn active_crew_id(&self) -> Option<&str> {
        self.active_crew_id.as_deref()
    }

    pub fn current_user_id(&self) -> Option<&str> {
        self.current_user.as_ref().map(|u| u.id.as_str())
    }

    /// Send a P2P signaling message through the Nakama channel.
    /// The message is a channel message with a special "signal" field.
    pub async fn send_signal(&self, to: &str, payload: &str) -> Result<()> {
        let channel_id = self
            .ws_shared
            .read()
            .await
            .channel_id
            .clone()
            .ok_or(Error::NotConnected)?;

        let content = serde_json::json!({
            "signal": true,
            "to": to,
            "data": payload
        })
        .to_string();

        let msg = serde_json::json!({
            "channel_message_send": {
                "channel_id": channel_id,
                "content": content
            }
        })
        .to_string();

        self.ws_send(msg).await
    }
}

// --- Message parsing (extracted for testability) ---

pub(crate) fn parse_channel_messages(
    list: ApiChannelMessageList,
    member_names: &HashMap<String, String>,
) -> Vec<ChatMessage> {
    list.messages
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| {
            let content_str = m.content.as_deref().unwrap_or("");

            // Skip signaling messages
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content_str) {
                if parsed.get("signal").and_then(|v| v.as_bool()) == Some(true) {
                    return None;
                }
            }

            // Parse with structured envelope, falling back to legacy format
            let envelope = crate::chat::parse_content(content_str)?;
            let text = envelope.body;
            let gif = envelope.gif;

            let sender_id = m.sender_id.unwrap_or_default();
            let sender_name = member_names
                .get(&sender_id)
                .cloned()
                .unwrap_or_else(|| m.username.unwrap_or_default());

            let create_time = m.create_time.unwrap_or_default();
            let update_time = m.update_time.unwrap_or_default();
            Some(ChatMessage {
                message_id: m.message_id.unwrap_or_default(),
                sender_id,
                sender_name,
                content: text,
                timestamp: create_time.clone(),
                create_time,
                update_time,
                gif,
            })
        })
        .collect()
}

// --- WebSocket background tasks ---

async fn ws_writer_task(
    mut rx: mpsc::Receiver<String>,
    mut write: futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
) {
    while let Some(msg) = rx.recv().await {
        if let Err(e) = write.send(Message::Text(msg)).await {
            log::error!("WebSocket write error: {}", e);
            break;
        }
    }
}

async fn ws_reader_task(
    mut read: futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    event_tx: std::sync::mpsc::Sender<Event>,
    shared: Arc<RwLock<WsShared>>,
    signal_tx: mpsc::Sender<InternalSignal>,
    presence_tx: mpsc::Sender<InternalPresence>,
    member_names: Arc<RwLock<HashMap<String, String>>>,
) {
    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                handle_ws_message(
                    &text,
                    &event_tx,
                    &shared,
                    &signal_tx,
                    &presence_tx,
                    &member_names,
                )
                .await;
            }
            Ok(Message::Close(_)) => {
                log::info!("WebSocket closed by server");
                break;
            }
            Err(e) => {
                log::error!("WebSocket read error: {}", e);
                let _ = event_tx.send(Event::Error {
                    message: format!("WebSocket error: {}", e),
                });
                break;
            }
            _ => {}
        }
    }
}

async fn handle_ws_message(
    text: &str,
    event_tx: &std::sync::mpsc::Sender<Event>,
    shared: &Arc<RwLock<WsShared>>,
    signal_tx: &mpsc::Sender<InternalSignal>,
    presence_tx: &mpsc::Sender<InternalPresence>,
    member_names: &Arc<RwLock<HashMap<String, String>>>,
) {
    let envelope: WsEnvelope = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(e) => {
            log::warn!("Failed to parse WS message: {} -- {}", e, text);
            return;
        }
    };

    // Channel join response
    if let Some(channel) = envelope.channel {
        if let Some(id) = channel.id {
            log::info!("Joined channel: {}", id);
            shared.write().await.channel_id = Some(id.clone());

            if let Some(presences) = channel.presences {
                for p in presences {
                    let _ = event_tx.send(Event::PresenceUpdated {
                        user_id: p.user_id.unwrap_or_default(),
                        online: true,
                    });
                }
            }
        }
    }

    // Channel message
    if let Some(msg) = envelope.channel_message {
        let content_str = msg.content.unwrap_or_default();

        // Check if this is a signaling message -- route to internal signal channel
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content_str) {
            if parsed.get("signal").and_then(|v| v.as_bool()) == Some(true) {
                let from = msg.sender_id.unwrap_or_default();
                let to = parsed.get("to").and_then(|v| v.as_str()).unwrap_or("");
                let our_id = shared
                    .read()
                    .await
                    .local_user_id
                    .clone()
                    .unwrap_or_default();
                if from == our_id || (!to.is_empty() && to != our_id) {
                    return;
                }

                let data = parsed
                    .get("data")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let _ = signal_tx.try_send(InternalSignal {
                    from,
                    payload: data,
                });
                return;
            }
        }

        let envelope = match crate::chat::parse_content(&content_str) {
            Some(e) => e,
            None => return,
        };
        let text = envelope.body;
        let gif = envelope.gif;

        let sender_id = msg.sender_id.unwrap_or_default();
        let sender_name = {
            let names = member_names.read().await;
            names
                .get(&sender_id)
                .cloned()
                .unwrap_or_else(|| msg.username.unwrap_or_default())
        };

        let create_time = msg.create_time.unwrap_or_default();
        let update_time = msg.update_time.unwrap_or_default();
        let _ = event_tx.send(Event::MessageReceived {
            message: ChatMessage {
                message_id: msg.message_id.unwrap_or_default(),
                sender_id,
                sender_name,
                content: text,
                timestamp: create_time.clone(),
                create_time,
                update_time,
                gif,
            },
        });
    }

    // Channel presence
    if let Some(presence) = envelope.channel_presence_event {
        if let Some(joins) = presence.joins {
            for p in joins {
                let user_id = p.user_id.clone().unwrap_or_default();
                let _ = event_tx.send(Event::MemberJoined {
                    crew_id: presence.channel_id.clone().unwrap_or_default(),
                    member: Member {
                        id: user_id.clone(),
                        username: p.username.clone().unwrap_or_default(),
                        display_name: p.username.unwrap_or_default(),
                        online: true,
                    },
                });
                let _ = presence_tx.try_send(InternalPresence::Joined { user_id });
            }
        }
        if let Some(leaves) = presence.leaves {
            for p in leaves {
                let user_id = p.user_id.unwrap_or_default();
                let _ = event_tx.send(Event::MemberLeft {
                    crew_id: presence.channel_id.clone().unwrap_or_default(),
                    member_id: user_id.clone(),
                });
                let _ = presence_tx.try_send(InternalPresence::Left { user_id });
            }
        }
    }

    // Status presence
    if let Some(status) = envelope.status_presence_event {
        if let Some(joins) = status.joins {
            for p in joins {
                let _ = event_tx.send(Event::PresenceUpdated {
                    user_id: p.user_id.unwrap_or_default(),
                    online: true,
                });
            }
        }
        if let Some(leaves) = status.leaves {
            for p in leaves {
                let _ = event_tx.send(Event::PresenceUpdated {
                    user_id: p.user_id.unwrap_or_default(),
                    online: false,
                });
            }
        }
    }

    // Notifications (push system: codes 110-115)
    if let Some(notif_list) = envelope.notifications {
        if let Some(notifications) = notif_list.notifications {
            for notif in notifications {
                let code = notif.code.unwrap_or(0);
                let content = notif.content.as_deref().unwrap_or("{}");
                handle_notification(code, content, event_tx);
            }
        }
    }

    // Error
    if let Some(err) = envelope.error {
        let _ = event_tx.send(Event::Error {
            message: err.message.unwrap_or_default(),
        });
    }
}

fn handle_notification(code: i32, content: &str, event_tx: &std::sync::mpsc::Sender<Event>) {
    use crate::crew_state;

    match code {
        // 110 = full crew state
        110 => match serde_json::from_str::<crew_state::CrewState>(content) {
            Ok(state) => {
                let _ = event_tx.send(Event::CrewStateLoaded { state });
            }
            Err(e) => log::warn!("Failed to parse crew_state notification: {}", e),
        },
        // 111 = priority crew event
        111 => match serde_json::from_str::<crew_state::CrewEvent>(content) {
            Ok(event) => {
                let _ = event_tx.send(Event::CrewEventReceived { event });
            }
            Err(e) => log::warn!("Failed to parse crew_event notification: {}", e),
        },
        // 112 = batched sidebar update
        112 => match serde_json::from_str::<crew_state::SidebarUpdate>(content) {
            Ok(update) => {
                let _ = event_tx.send(Event::SidebarUpdated {
                    crews: update.crews,
                });
            }
            Err(e) => log::warn!("Failed to parse sidebar_update notification: {}", e),
        },
        // 113 = presence change
        113 => match serde_json::from_str::<crew_state::PresenceChange>(content) {
            Ok(change) => {
                let _ = event_tx.send(Event::PresenceChanged { change });
            }
            Err(e) => log::warn!("Failed to parse presence_change notification: {}", e),
        },
        // 114 = voice update
        114 => match serde_json::from_str::<crew_state::VoiceUpdate>(content) {
            Ok(update) => {
                log::info!(
                    "Notification 114: voice_update crew={} channels={} members={}",
                    update.crew_id,
                    update.voice_channels.len(),
                    update.members.len()
                );
                if !update.voice_channels.is_empty() {
                    let _ = event_tx.send(Event::VoiceChannelsUpdated {
                        crew_id: update.crew_id.clone(),
                        channels: update.voice_channels,
                    });
                }
                let _ = event_tx.send(Event::VoiceUpdated {
                    crew_id: update.crew_id,
                    channel_id: update.channel_id,
                    members: update.members,
                });
            }
            Err(e) => log::warn!("Failed to parse voice_update notification: {}", e),
        },
        // 115 = throttled message preview
        115 => match serde_json::from_str::<crew_state::MessagePreviewUpdate>(content) {
            Ok(update) => {
                let _ = event_tx.send(Event::MessagePreviewUpdated {
                    crew_id: update.crew_id,
                    messages: update.messages,
                });
            }
            Err(e) => log::warn!("Failed to parse message_preview notification: {}", e),
        },
        _ => {
            log::debug!("Unhandled notification code: {}", code);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_api_msg(content: &str, sender_id: &str, username: &str) -> ApiChannelMessage {
        ApiChannelMessage {
            channel_id: Some("ch-1".into()),
            message_id: Some(format!("msg-{}", sender_id)),
            sender_id: Some(sender_id.into()),
            username: Some(username.into()),
            content: Some(content.into()),
            create_time: Some("2026-03-08T12:00:00Z".into()),
            update_time: Some("2026-03-08T12:00:00Z".into()),
            code: Some(0),
        }
    }

    fn empty_names() -> HashMap<String, String> {
        HashMap::new()
    }

    fn names_with(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn deserialize_channel_message_list() {
        let json = r#"{
            "messages": [
                {
                    "channel_id": "abc",
                    "message_id": "m1",
                    "sender_id": "u1",
                    "username": "alice",
                    "content": "{\"text\":\"hello\"}",
                    "create_time": "2026-03-08T12:00:00Z",
                    "code": 0
                }
            ],
            "next_cursor": "cur123",
            "prev_cursor": ""
        }"#;

        let list: ApiChannelMessageList = serde_json::from_str(json).unwrap();
        let msgs = list.messages.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].username.as_deref(), Some("alice"));
        assert_eq!(msgs[0].content.as_deref(), Some("{\"text\":\"hello\"}"));
    }

    #[test]
    fn parse_extracts_text_from_chat_content() {
        let list = ApiChannelMessageList {
            messages: Some(vec![make_api_msg(
                r#"{"text":"hello world"}"#,
                "u1",
                "alice",
            )]),
            next_cursor: None,
            prev_cursor: None,
        };

        let result = parse_channel_messages(list, &empty_names());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "hello world");
        assert_eq!(result[0].sender_name, "alice");
        assert_eq!(result[0].sender_id, "u1");
    }

    #[test]
    fn parse_resolves_display_name_from_cache() {
        let list = ApiChannelMessageList {
            messages: Some(vec![make_api_msg(
                r#"{"text":"hello"}"#,
                "u1",
                "VObaZMuWUa",
            )]),
            next_cursor: None,
            prev_cursor: None,
        };

        let names = names_with(&[("u1", "Bob")]);
        let result = parse_channel_messages(list, &names);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].sender_name, "Bob");
    }

    #[test]
    fn parse_filters_out_signal_messages() {
        let list = ApiChannelMessageList {
            messages: Some(vec![
                make_api_msg(r#"{"text":"hi"}"#, "u1", "alice"),
                make_api_msg(
                    r#"{"signal":true,"to":"u1","data":"{\"Offer\":{\"sdp\":\"v=0\"}}"}"#,
                    "u2",
                    "bob",
                ),
                make_api_msg(r#"{"text":"bye"}"#, "u3", "carol"),
            ]),
            next_cursor: None,
            prev_cursor: None,
        };

        let result = parse_channel_messages(list, &empty_names());
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "hi");
        assert_eq!(result[1].content, "bye");
    }

    #[test]
    fn parse_filters_out_empty_json_messages() {
        let list = ApiChannelMessageList {
            messages: Some(vec![
                make_api_msg(r#"{"text":"real msg"}"#, "u1", "alice"),
                make_api_msg(r#"{}"#, "u2", "bob"),
                make_api_msg(r#"{"text":"also real"}"#, "u3", "carol"),
            ]),
            next_cursor: None,
            prev_cursor: None,
        };

        let result = parse_channel_messages(list, &empty_names());
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "real msg");
        assert_eq!(result[1].content, "also real");
    }

    #[test]
    fn parse_handles_empty_messages_list() {
        let list = ApiChannelMessageList {
            messages: None,
            next_cursor: None,
            prev_cursor: None,
        };
        assert!(parse_channel_messages(list, &empty_names()).is_empty());

        let list2 = ApiChannelMessageList {
            messages: Some(vec![]),
            next_cursor: None,
            prev_cursor: None,
        };
        assert!(parse_channel_messages(list2, &empty_names()).is_empty());
    }

    #[test]
    fn parse_skips_messages_with_no_text_field() {
        let list = ApiChannelMessageList {
            messages: Some(vec![ApiChannelMessage {
                channel_id: None,
                message_id: None,
                sender_id: None,
                username: None,
                content: None,
                create_time: None,
                update_time: None,
                code: None,
            }]),
            next_cursor: None,
            prev_cursor: None,
        };

        let result = parse_channel_messages(list, &empty_names());
        assert!(result.is_empty());
    }

    #[test]
    fn parse_skips_non_json_content() {
        let list = ApiChannelMessageList {
            messages: Some(vec![make_api_msg("plain text, not json", "u1", "alice")]),
            next_cursor: None,
            prev_cursor: None,
        };

        let result = parse_channel_messages(list, &empty_names());
        assert!(result.is_empty());
    }

    #[test]
    fn parse_signal_false_is_not_filtered() {
        let list = ApiChannelMessageList {
            messages: Some(vec![make_api_msg(
                r#"{"signal":false,"text":"keep me"}"#,
                "u1",
                "alice",
            )]),
            next_cursor: None,
            prev_cursor: None,
        };

        let result = parse_channel_messages(list, &empty_names());
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn full_nakama_response_roundtrip() {
        let json = r#"{
            "messages": [
                {
                    "channel_id": "group-abc-123",
                    "message_id": "msg-001",
                    "sender_id": "user-aaa",
                    "username": "alice",
                    "content": "{\"signal\":true,\"to\":\"user-bbb\",\"data\":\"{}\"}",
                    "create_time": "2026-03-08T11:00:00Z",
                    "code": 0
                },
                {
                    "channel_id": "group-abc-123",
                    "message_id": "msg-002",
                    "sender_id": "user-bbb",
                    "username": "bob",
                    "content": "{\"text\":\"hey everyone\"}",
                    "create_time": "2026-03-08T11:01:00Z",
                    "code": 0
                },
                {
                    "channel_id": "group-abc-123",
                    "message_id": "msg-003",
                    "sender_id": "user-aaa",
                    "username": "alice",
                    "content": "{\"signal\":true,\"to\":\"user-ccc\",\"data\":\"{\\\"IceCandidate\\\":{}}\"}",
                    "create_time": "2026-03-08T11:02:00Z",
                    "code": 0
                },
                {
                    "channel_id": "group-abc-123",
                    "message_id": "msg-004",
                    "sender_id": "user-ccc",
                    "username": "carol",
                    "content": "{\"text\":\"yo bob!\"}",
                    "create_time": "2026-03-08T11:03:00Z",
                    "code": 0
                }
            ],
            "next_cursor": "",
            "prev_cursor": "cursor-prev-xyz"
        }"#;

        let list: ApiChannelMessageList = serde_json::from_str(json).unwrap();
        let result = parse_channel_messages(list, &empty_names());

        assert_eq!(result.len(), 2, "signal messages should be filtered");
        assert_eq!(result[0].sender_name, "bob");
        assert_eq!(result[0].content, "hey everyone");
        assert_eq!(result[0].timestamp, "2026-03-08T11:01:00Z");
        assert_eq!(result[1].sender_name, "carol");
        assert_eq!(result[1].content, "yo bob!");
    }
}
