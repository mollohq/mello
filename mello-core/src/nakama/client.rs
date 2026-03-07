use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use crate::config::Config;
use crate::crew::{Crew, Member};
use crate::events::{ChatMessage, Event, User};
use crate::{Error, Result};
use super::types::*;

#[derive(Default)]
struct WsShared {
    channel_id: Option<String>,
}

pub struct NakamaClient {
    config: Config,
    http: reqwest::Client,
    token: Option<String>,
    current_user: Option<User>,
    active_crew_id: Option<String>,
    ws_tx: Option<mpsc::Sender<String>>,
    ws_shared: Arc<RwLock<WsShared>>,
    next_cid: u64,
}

impl NakamaClient {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            token: None,
            current_user: None,
            active_crew_id: None,
            ws_tx: None,
            ws_shared: Arc::new(RwLock::new(WsShared::default())),
            next_cid: 1,
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

        let user = self.get_account().await?;
        self.current_user = Some(user.clone());
        Ok(user)
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

        tokio::spawn(ws_writer_task(ws_rx, write));
        tokio::spawn(ws_reader_task(read, event_tx, shared));

        log::info!("WebSocket connected");
        Ok(())
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
                    member_count: g.edge_count.unwrap_or(0),
                    max_members: g.max_count.unwrap_or(6),
                    open: g.open.unwrap_or(false),
                })
            })
            .collect();

        Ok(crews)
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
            member_count: 1,
            max_members: 6,
            open: true,
        })
    }

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

    pub fn active_crew_id(&self) -> Option<&str> {
        self.active_crew_id.as_deref()
    }
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
) {
    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                handle_ws_message(&text, &event_tx, &shared).await;
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

    // Error
    if let Some(err) = envelope.error {
        let _ = event_tx.send(Event::Error {
            message: err.message.unwrap_or_default(),
        });
    }
}
