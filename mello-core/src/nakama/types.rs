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
    pub avatar_url: Option<String>,
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

// --- Health / version ---

#[derive(Debug, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    #[serde(default)]
    pub protocol_version: Option<u32>,
    #[serde(default)]
    pub min_client_protocol: Option<u32>,
}

// --- RPC request/response types ---

#[derive(Debug, Serialize)]
pub struct CreateCrewPayload {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invite_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub invite_user_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateCrewResult {
    pub crew_id: String,
    pub name: String,
    pub invite_code: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchUsersResult {
    #[serde(default)]
    pub users: Vec<SearchUserEntry>,
}

#[derive(Debug, Deserialize)]
pub struct SearchUserEntry {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub is_friend: bool,
}

#[derive(Debug, Deserialize)]
pub struct JoinByInviteCodeResult {
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
    pub update_time: Option<String>,
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
    pub update_time: Option<String>,
    pub code: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct ChatContent {
    pub text: Option<String>,
}

// --- Nakama storage ---

#[derive(Debug, Deserialize)]
pub struct ApiStorageObjects {
    pub objects: Option<Vec<ApiStorageObject>>,
}

#[derive(Debug, Deserialize)]
pub struct ApiStorageObject {
    pub collection: Option<String>,
    pub key: Option<String>,
    pub user_id: Option<String>,
    pub value: Option<String>,
}
