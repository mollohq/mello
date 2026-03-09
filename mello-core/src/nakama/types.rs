use serde::{Deserialize, Serialize};

// --- REST API types ---

#[derive(Debug, Deserialize)]
pub struct ApiSession {
    pub token: String,
    #[serde(alias = "refreshToken")]
    pub refresh_token: Option<String>,
    pub created: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ApiAccount {
    pub user: Option<ApiUser>,
    pub email: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ApiUser {
    pub id: String,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub metadata: Option<String>,
    pub online: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ApiUserGroupList {
    pub user_groups: Option<Vec<ApiUserGroup>>,
}

#[derive(Debug, Deserialize)]
pub struct ApiUserGroup {
    pub group: Option<ApiGroup>,
    pub state: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct ApiGroup {
    pub id: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub max_count: Option<i32>,
    pub metadata: Option<String>,
    pub open: Option<bool>,
    pub edge_count: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct ApiGroupUserList {
    pub group_users: Option<Vec<ApiGroupUser>>,
}

#[derive(Debug, Deserialize)]
pub struct ApiGroupUser {
    pub user: Option<ApiUser>,
    pub state: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct ApiRpcResponse {
    pub payload: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ApiError {
    pub error: Option<String>,
    pub message: Option<String>,
    pub code: Option<i32>,
}

// --- User metadata (stored as JSON string in Nakama) ---

#[derive(Debug, Deserialize)]
pub struct UserMetadata {
    pub tag: Option<String>,
    pub created_at: Option<i64>,
}

// --- RPC request/response types ---

#[derive(Debug, Serialize)]
pub struct CreateCrewPayload {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateCrewResult {
    pub crew_id: String,
    pub name: String,
}

// --- WebSocket types ---

#[derive(Debug, Deserialize)]
pub struct WsEnvelope {
    pub cid: Option<String>,
    pub channel: Option<WsChannel>,
    pub channel_message: Option<WsChannelMessage>,
    pub channel_presence_event: Option<WsChannelPresenceEvent>,
    pub status_presence_event: Option<WsStatusPresenceEvent>,
    pub notifications: Option<WsNotificationList>,
    pub error: Option<WsError>,
}

#[derive(Debug, Deserialize)]
pub struct WsChannel {
    pub id: Option<String>,
    pub presences: Option<Vec<WsUserPresence>>,
    #[serde(rename = "self")]
    pub self_presence: Option<WsUserPresence>,
}

#[derive(Debug, Deserialize)]
pub struct WsChannelMessage {
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
    pub sender_id: Option<String>,
    pub username: Option<String>,
    pub content: Option<String>,
    pub create_time: Option<String>,
    pub code: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct WsChannelPresenceEvent {
    pub channel_id: Option<String>,
    pub joins: Option<Vec<WsUserPresence>>,
    pub leaves: Option<Vec<WsUserPresence>>,
}

#[derive(Debug, Deserialize)]
pub struct WsStatusPresenceEvent {
    pub joins: Option<Vec<WsStatusPresence>>,
    pub leaves: Option<Vec<WsStatusPresence>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WsUserPresence {
    pub user_id: Option<String>,
    pub username: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WsStatusPresence {
    pub user_id: Option<String>,
    pub username: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WsNotificationList {
    pub notifications: Option<Vec<WsNotification>>,
}

#[derive(Debug, Deserialize)]
pub struct WsNotification {
    pub id: Option<String>,
    pub subject: Option<String>,
    pub content: Option<String>,
    pub code: Option<i32>,
    pub sender_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WsError {
    pub code: Option<i32>,
    pub message: Option<String>,
}

// --- REST API: group listing ---

#[derive(Debug, Deserialize)]
pub struct ApiGroupList {
    pub groups: Option<Vec<ApiGroup>>,
    pub cursor: Option<String>,
}

// --- REST API: channel message history ---

#[derive(Debug, Deserialize)]
pub struct ApiChannelMessageList {
    pub messages: Option<Vec<ApiChannelMessage>>,
    pub next_cursor: Option<String>,
    pub prev_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ApiChannelMessage {
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
    pub sender_id: Option<String>,
    pub username: Option<String>,
    pub content: Option<String>,
    pub create_time: Option<String>,
    pub code: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct ChatContent {
    pub text: Option<String>,
}
