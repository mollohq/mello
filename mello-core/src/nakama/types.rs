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

/// `GET /v2/user` response.
#[derive(Debug, Deserialize)]
pub struct ApiUsers {
    pub users: Option<Vec<ApiUser>>,
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

// --- Stream viewer ---

#[derive(Debug, Deserialize)]
pub struct WatchStreamResponse {
    #[serde(default = "default_p2p")]
    pub mode: String,
    #[serde(default)]
    pub sfu_endpoint: Option<String>,
    #[serde(default)]
    pub sfu_token: Option<String>,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub bitrate_kbps: u32,
}

fn default_p2p() -> String {
    "p2p".to_string()
}

// --- Riot account linking ---

/// Response of `riot_status` / `riot_link`: whether the server has a Riot key
/// configured and whether this user has linked their Riot ID.
#[derive(Debug, Default, Deserialize)]
pub struct RiotStatus {
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub linked: bool,
    #[serde(default)]
    pub riot_id: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
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

#[derive(Debug, Deserialize)]
pub struct ResolveCrewInviteResult {
    pub crew_name: String,
    pub avatar_seed: String,
    pub crew_id: String,
    #[serde(default)]
    pub highlight: String,
    #[serde(default)]
    pub member_count: i32,
    #[serde(default)]
    pub members: Vec<InviteMemberPreview>,
    #[serde(default)]
    pub inviter_display_name: String,
    #[serde(default)]
    pub inviter_avatar_seed: String,
}

#[derive(Debug, Deserialize)]
pub struct InviteMemberPreview {
    pub display_name: String,
    #[serde(default)]
    pub avatar_seed: String,
}

impl From<ResolveCrewInviteResult> for crate::crew::ResolvedInvite {
    fn from(r: ResolveCrewInviteResult) -> Self {
        let person = |display_name: String, avatar_seed: String| crate::crew::InvitePerson {
            avatar_seed: if avatar_seed.is_empty() {
                display_name.clone()
            } else {
                avatar_seed
            },
            display_name,
        };
        // Every code made by `create_crew` or `create_invite_code` has an
        // inviter. An older code, or an inviter whose account is gone, has none.
        let inviter = (!r.inviter_display_name.trim().is_empty())
            .then(|| person(r.inviter_display_name, r.inviter_avatar_seed));
        Self {
            crew_name: r.crew_name,
            avatar_seed: r.avatar_seed,
            crew_id: r.crew_id,
            highlight: r.highlight,
            member_count: r.member_count,
            members: r
                .members
                .into_iter()
                .filter(|m| !m.display_name.trim().is_empty())
                .map(|m| person(m.display_name, m.avatar_seed))
                .collect(),
            inviter,
        }
    }
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

#[cfg(test)]
mod invite_tests {
    use super::ResolveCrewInviteResult;
    use crate::crew::{InvitePerson, ResolvedInvite};

    fn resolve(json: &str) -> ResolvedInvite {
        serde_json::from_str::<ResolveCrewInviteResult>(json)
            .expect("deserialize")
            .into()
    }

    /// The welcome screen and the join modal show the inviter and the
    /// members. The client dropped them before.
    #[test]
    fn a_resolved_invite_keeps_the_inviter_and_the_members() {
        let invite = resolve(
            r#"{"crew_name":"Night Owls","avatar_seed":"Night Owls","crew_id":"c1",
                "highlight":"7h hangout","member_count":4,
                "members":[{"display_name":"alice","avatar_seed":"alice"},
                           {"display_name":"bo","avatar_seed":""}],
                "inviter_display_name":"alice","inviter_avatar_seed":"alice",
                "top_game":"CS2"}"#,
        );
        assert_eq!(invite.member_count, 4);
        assert_eq!(
            invite.inviter,
            Some(InvitePerson {
                display_name: "alice".into(),
                avatar_seed: "alice".into()
            })
        );
        assert_eq!(invite.members.len(), 2);
        assert_eq!(
            invite.members[1].avatar_seed, "bo",
            "an empty seed falls back to the name"
        );
    }

    /// A code with no inviter still resolves.
    #[test]
    fn a_resolved_invite_without_an_inviter_has_none() {
        let invite =
            resolve(r#"{"crew_name":"Night Owls","avatar_seed":"Night Owls","crew_id":"c1"}"#);
        assert_eq!(invite.inviter, None);
        assert_eq!(invite.member_count, 0);
        assert!(invite.members.is_empty());
    }
}

#[cfg(test)]
mod stream_tests {
    use super::WatchStreamResponse;

    #[test]
    fn watch_stream_response_parses_bitrate() {
        let response: WatchStreamResponse = serde_json::from_str(
            r#"{"mode":"sfu","width":1920,"height":1080,"bitrate_kbps":4500}"#,
        )
        .expect("deserialize");
        assert_eq!(response.bitrate_kbps, 4_500);
    }

    #[test]
    fn legacy_watch_stream_response_defaults_bitrate_to_zero() {
        let response: WatchStreamResponse =
            serde_json::from_str(r#"{"mode":"p2p"}"#).expect("deserialize");
        assert_eq!(response.bitrate_kbps, 0);
    }
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
