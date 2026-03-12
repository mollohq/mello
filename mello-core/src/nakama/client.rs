use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use crate::config::Config;
use crate::crew::{Crew, Member};
use crate::crew_state::CrewState;
use crate::events::{ChatMessage, Event, User};
use crate::presence::{Activity, PresenceStatus};
use crate::{Error, Result};
use super::types::*;

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
}

impl NakamaClient {
    pub fn new(config: Config) -> Self {
        let (sig_tx, sig_rx) = mpsc::channel(256);
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

    // --- Authentication ---

    pub async fn login_email(&mut self, email: &str, password: &str) -> Result<User> {
        let url = format!(
            "{}/v2/account/authenticate/email?create=true",
            self.config.http_base()
        );

        let resp = self.http.post(&url)
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

    /// Returns (User, created) where `created` is true when Nakama just created the account.
    pub async fn authenticate_device(&mut self, device_id: &str) -> Result<(User, bool)> {
        let url = format!(
            "{}/v2/account/authenticate/device?create=true",
            self.config.http_base()
        );

        let resp = self.http.post(&url)
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
        let url = format!(
            "{}/v2/account/session/refresh",
            self.config.http_base()
        );

        let resp = self.http.post(&url)
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

        let resp = self.http.get(&url)
            .bearer_auth(&token)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(Error::Server("Failed to get account".into()));
        }

        let account: ApiAccount = resp.json().await?;
        let api_user = account.user.ok_or(Error::Internal("No user in account".into()))?;

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

        tokio::spawn(ws_writer_task(ws_rx, write));
        tokio::spawn(ws_reader_task(read, event_tx, shared, signal_tx));

        log::info!("WebSocket connected");
        Ok(())
    }

    /// Take the signal receiver (call once, from the client run loop)
    pub fn take_signal_rx(&mut self) -> Option<mpsc::Receiver<InternalSignal>> {
        self.signal_rx.take()
    }

    async fn ws_send(&self, msg: String) -> Result<()> {
        if let Some(tx) = &self.ws_tx {
            tx.send(msg).await.map_err(|e| Error::WebSocket(e.to_string()))?;
        }
        Ok(())
    }

    // --- Crews ---

    pub async fn list_user_groups(&self) -> Result<Vec<Crew>> {
        let token = self.bearer()?;
        let user_id = self.current_user.as_ref()
            .ok_or(Error::NotConnected)?
            .id.clone();
        let url = format!("{}/v2/user/{}/group", self.config.http_base(), user_id);

        let resp = self.http.get(&url)
            .bearer_auth(&token)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(Error::Server("Failed to list groups".into()));
        }

        let list: ApiUserGroupList = resp.json().await?;
        let crews = list.user_groups.unwrap_or_default()
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
                })
            })
            .collect();

        Ok(crews)
    }

    pub async fn list_groups(&self, limit: u32) -> Result<Vec<Crew>> {
        let token = self.bearer()?;
        let url = format!(
            "{}/v2/group?limit={}&open=true",
            self.config.http_base(),
            limit
        );

        let resp = self.http.get(&url)
            .bearer_auth(&token)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(Error::Server("Failed to list groups".into()));
        }

        let list: ApiGroupList = resp.json().await?;
        let crews = list.groups.unwrap_or_default()
            .into_iter()
            .filter_map(|g| {
                Some(Crew {
                    id: g.id?,
                    name: g.name.unwrap_or_default(),
                    description: g.description.unwrap_or_default(),
                    member_count: g.edge_count.unwrap_or(0),
                    max_members: g.max_count.unwrap_or(6),
                    open: g.open.unwrap_or(false),
                })
            })
            .collect();

        Ok(crews)
    }

    pub async fn join_group(&self, group_id: &str) -> Result<()> {
        let token = self.bearer()?;
        let url = format!("{}/v2/group/{}/join", self.config.http_base(), group_id);

        let resp = self.http.post(&url)
            .bearer_auth(&token)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(Error::Server(err_text));
        }

        Ok(())
    }

    pub async fn update_account(&self, display_name: &str) -> Result<()> {
        let token = self.bearer()?;
        let url = format!("{}/v2/account", self.config.http_base());

        let resp = self.http.put(&url)
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

        let resp = self.http.post(&url)
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

    pub async fn create_crew(&self, name: &str) -> Result<Crew> {
        let token = self.bearer()?;
        let url = format!("{}/v2/rpc/create_crew", self.config.http_base());

        let payload = serde_json::to_string(&CreateCrewPayload {
            name: name.to_string(),
        })?;
        let body = serde_json::Value::String(payload);

        let resp = self.http.post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(Error::Server(err_text));
        }

        let rpc_resp: ApiRpcResponse = resp.json().await?;
        let result: CreateCrewResult = serde_json::from_str(
            &rpc_resp.payload.unwrap_or_default(),
        )?;

        Ok(Crew {
            id: result.crew_id,
            name: result.name,
            description: String::new(),
            member_count: 1,
            max_members: 6,
            open: true,
        })
    }

    // --- Generic RPC ---

    pub async fn rpc(&self, id: &str, payload: &serde_json::Value) -> Result<String> {
        let token = self.bearer()?;
        let url = format!("{}/v2/rpc/{}", self.config.http_base(), id);

        let body = serde_json::Value::String(payload.to_string());

        let resp = self.http.post(&url)
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

    // --- Presence RPCs ---

    pub async fn presence_update(&self, status: &PresenceStatus, activity: Option<&Activity>) -> Result<()> {
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

    pub async fn subscribe_sidebar(&self, crew_ids: &[String]) -> Result<Vec<crate::crew_state::CrewSidebarState>> {
        let payload = serde_json::json!({ "crew_ids": crew_ids });
        let resp = self.rpc("subscribe_sidebar", &payload).await?;

        #[derive(serde::Deserialize)]
        struct Resp {
            crews: Vec<crate::crew_state::CrewSidebarState>,
        }
        let parsed: Resp = serde_json::from_str(&resp)?;
        Ok(parsed.crews)
    }

    // --- Voice RPCs ---

    pub async fn voice_join(&self, crew_id: &str) -> Result<()> {
        let payload = serde_json::json!({ "crew_id": crew_id });
        self.rpc("voice_join", &payload).await?;
        Ok(())
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
        }).to_string();

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
            }).to_string();
            self.ws_send(msg).await?;
        }

        self.active_crew_id = None;
        self.ws_shared.write().await.channel_id = None;
        Ok(())
    }

    // --- Chat ---

    pub async fn send_chat_message(&self, text: &str) -> Result<()> {
        let channel_id = self.ws_shared.read().await.channel_id.clone()
            .ok_or(Error::NotConnected)?;

        let content = serde_json::json!({"text": text}).to_string();

        let msg = serde_json::json!({
            "channel_message_send": {
                "channel_id": channel_id,
                "content": content
            }
        }).to_string();

        self.ws_send(msg).await
    }

    pub async fn list_channel_messages(&self, channel_id: &str, limit: u32) -> Result<Vec<ChatMessage>> {
        let token = self.bearer()?;
        let url = format!(
            "{}/v2/channel/{}?limit={}&forward=false",
            self.config.http_base(),
            channel_id,
            limit
        );

        let resp = self.http.get(&url)
            .bearer_auth(&token)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(Error::Server("Failed to list channel messages".into()));
        }

        let list: ApiChannelMessageList = resp.json().await?;
        Ok(parse_channel_messages(list))
    }

    // --- Crew members ---

    pub async fn list_group_users(&self, group_id: &str) -> Result<Vec<Member>> {
        let token = self.bearer()?;
        let url = format!("{}/v2/group/{}/user", self.config.http_base(), group_id);

        let resp = self.http.get(&url)
            .bearer_auth(&token)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(Error::Server("Failed to list group users".into()));
        }

        let list: ApiGroupUserList = resp.json().await?;
        let members = list.group_users.unwrap_or_default()
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

        Ok(members)
    }

    // --- Status ---

    pub async fn follow_users(&self, user_ids: &[String]) -> Result<()> {
        let msg = serde_json::json!({
            "status_follow": {
                "user_ids": user_ids
            }
        }).to_string();
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
        let channel_id = self.ws_shared.read().await.channel_id.clone()
            .ok_or(Error::NotConnected)?;

        let content = serde_json::json!({
            "signal": true,
            "to": to,
            "data": payload
        }).to_string();

        let msg = serde_json::json!({
            "channel_message_send": {
                "channel_id": channel_id,
                "content": content
            }
        }).to_string();

        self.ws_send(msg).await
    }
}

// --- Message parsing (extracted for testability) ---

pub(crate) fn parse_channel_messages(list: ApiChannelMessageList) -> Vec<ChatMessage> {
    list.messages.unwrap_or_default()
        .into_iter()
        .filter_map(|m| {
            let content_str = m.content.as_deref().unwrap_or("");
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content_str) {
                if parsed.get("signal").and_then(|v| v.as_bool()) == Some(true) {
                    return None;
                }
            }
            let text = serde_json::from_str::<ChatContent>(content_str)
                .ok()
                .and_then(|c| c.text)
                .unwrap_or_else(|| content_str.to_string());

            Some(ChatMessage {
                message_id: m.message_id.unwrap_or_default(),
                sender_id: m.sender_id.unwrap_or_default(),
                sender_name: m.username.unwrap_or_default(),
                content: text,
                timestamp: m.create_time.unwrap_or_default(),
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
        if let Err(e) = write.send(Message::Text(msg.into())).await {
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
) {
    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                handle_ws_message(&text, &event_tx, &shared, &signal_tx).await;
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
                let our_id = shared.read().await.local_user_id.clone().unwrap_or_default();
                if from == our_id || (!to.is_empty() && to != our_id) {
                    return;
                }

                let data = parsed.get("data").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let _ = signal_tx.try_send(InternalSignal { from, payload: data });
                return;
            }
        }

        let text = serde_json::from_str::<ChatContent>(&content_str)
            .ok()
            .and_then(|c| c.text)
            .unwrap_or(content_str);

        let _ = event_tx.send(Event::MessageReceived {
            message: ChatMessage {
                message_id: msg.message_id.unwrap_or_default(),
                sender_id: msg.sender_id.unwrap_or_default(),
                sender_name: msg.username.unwrap_or_default(),
                content: text,
                timestamp: msg.create_time.unwrap_or_default(),
            },
        });
    }

    // Channel presence
    if let Some(presence) = envelope.channel_presence_event {
        if let Some(joins) = presence.joins {
            for p in joins {
                let _ = event_tx.send(Event::MemberJoined {
                    crew_id: presence.channel_id.clone().unwrap_or_default(),
                    member: Member {
                        id: p.user_id.clone().unwrap_or_default(),
                        username: p.username.clone().unwrap_or_default(),
                        display_name: p.username.unwrap_or_default(),
                        online: true,
                    },
                });
            }
        }
        if let Some(leaves) = presence.leaves {
            for p in leaves {
                let _ = event_tx.send(Event::MemberLeft {
                    crew_id: presence.channel_id.clone().unwrap_or_default(),
                    member_id: p.user_id.unwrap_or_default(),
                });
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

fn handle_notification(
    code: i32,
    content: &str,
    event_tx: &std::sync::mpsc::Sender<Event>,
) {
    use crate::crew_state;

    match code {
        // 110 = full crew state
        110 => {
            match serde_json::from_str::<crew_state::CrewState>(content) {
                Ok(state) => {
                    let _ = event_tx.send(Event::CrewStateLoaded { state });
                }
                Err(e) => log::warn!("Failed to parse crew_state notification: {}", e),
            }
        }
        // 111 = priority crew event
        111 => {
            match serde_json::from_str::<crew_state::CrewEvent>(content) {
                Ok(event) => {
                    let _ = event_tx.send(Event::CrewEventReceived { event });
                }
                Err(e) => log::warn!("Failed to parse crew_event notification: {}", e),
            }
        }
        // 112 = batched sidebar update
        112 => {
            match serde_json::from_str::<crew_state::SidebarUpdate>(content) {
                Ok(update) => {
                    let _ = event_tx.send(Event::SidebarUpdated { crews: update.crews });
                }
                Err(e) => log::warn!("Failed to parse sidebar_update notification: {}", e),
            }
        }
        // 113 = presence change
        113 => {
            match serde_json::from_str::<crew_state::PresenceChange>(content) {
                Ok(change) => {
                    let _ = event_tx.send(Event::PresenceChanged { change });
                }
                Err(e) => log::warn!("Failed to parse presence_change notification: {}", e),
            }
        }
        // 114 = voice update
        114 => {
            match serde_json::from_str::<crew_state::VoiceUpdate>(content) {
                Ok(update) => {
                    let _ = event_tx.send(Event::VoiceUpdated {
                        crew_id: update.crew_id,
                        members: update.members,
                    });
                }
                Err(e) => log::warn!("Failed to parse voice_update notification: {}", e),
            }
        }
        // 115 = throttled message preview
        115 => {
            match serde_json::from_str::<crew_state::MessagePreviewUpdate>(content) {
                Ok(update) => {
                    let _ = event_tx.send(Event::MessagePreviewUpdated {
                        crew_id: update.crew_id,
                        messages: update.messages,
                    });
                }
                Err(e) => log::warn!("Failed to parse message_preview notification: {}", e),
            }
        }
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
            code: Some(0),
        }
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
            messages: Some(vec![
                make_api_msg(r#"{"text":"hello world"}"#, "u1", "alice"),
            ]),
            next_cursor: None,
            prev_cursor: None,
        };

        let result = parse_channel_messages(list);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "hello world");
        assert_eq!(result[0].sender_name, "alice");
        assert_eq!(result[0].sender_id, "u1");
    }

    #[test]
    fn parse_filters_out_signal_messages() {
        let list = ApiChannelMessageList {
            messages: Some(vec![
                make_api_msg(r#"{"text":"hi"}"#, "u1", "alice"),
                make_api_msg(
                    r#"{"signal":true,"to":"u1","data":"{\"Offer\":{\"sdp\":\"v=0\"}}"}"#,
                    "u2", "bob",
                ),
                make_api_msg(r#"{"text":"bye"}"#, "u3", "carol"),
            ]),
            next_cursor: None,
            prev_cursor: None,
        };

        let result = parse_channel_messages(list);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "hi");
        assert_eq!(result[1].content, "bye");
    }

    #[test]
    fn parse_handles_empty_messages_list() {
        let list = ApiChannelMessageList {
            messages: None,
            next_cursor: None,
            prev_cursor: None,
        };
        assert!(parse_channel_messages(list).is_empty());

        let list2 = ApiChannelMessageList {
            messages: Some(vec![]),
            next_cursor: None,
            prev_cursor: None,
        };
        assert!(parse_channel_messages(list2).is_empty());
    }

    #[test]
    fn parse_handles_missing_fields_gracefully() {
        let list = ApiChannelMessageList {
            messages: Some(vec![ApiChannelMessage {
                channel_id: None,
                message_id: None,
                sender_id: None,
                username: None,
                content: None,
                create_time: None,
                code: None,
            }]),
            next_cursor: None,
            prev_cursor: None,
        };

        let result = parse_channel_messages(list);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "");
        assert_eq!(result[0].sender_name, "");
        assert_eq!(result[0].message_id, "");
    }

    #[test]
    fn parse_falls_back_to_raw_content_when_not_json() {
        let list = ApiChannelMessageList {
            messages: Some(vec![
                make_api_msg("plain text, not json", "u1", "alice"),
            ]),
            next_cursor: None,
            prev_cursor: None,
        };

        let result = parse_channel_messages(list);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "plain text, not json");
    }

    #[test]
    fn parse_signal_false_is_not_filtered() {
        let list = ApiChannelMessageList {
            messages: Some(vec![
                make_api_msg(r#"{"signal":false,"text":"keep me"}"#, "u1", "alice"),
            ]),
            next_cursor: None,
            prev_cursor: None,
        };

        let result = parse_channel_messages(list);
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
        let result = parse_channel_messages(list);

        assert_eq!(result.len(), 2, "signal messages should be filtered");
        assert_eq!(result[0].sender_name, "bob");
        assert_eq!(result[0].content, "hey everyone");
        assert_eq!(result[0].timestamp, "2026-03-08T11:01:00Z");
        assert_eq!(result[1].sender_name, "carol");
        assert_eq!(result[1].content, "yo bob!");
    }
}
