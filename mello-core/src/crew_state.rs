use serde::{Deserialize, Serialize};

use crate::presence::{Activity, GamePresence, PresenceStatus, UserPresence};

// ---------------------------------------------------------------------------
// Full crew state (returned by crew_state_get for the active crew)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrewState {
    pub crew_id: String,
    pub name: String,
    pub counts: CrewCounts,
    #[serde(default)]
    pub members: Option<Vec<CrewMember>>,
    pub voice: VoiceState,
    #[serde(default)]
    pub voice_channels: Vec<VoiceChannelState>,
    #[serde(default)]
    pub stream: Option<StreamState>,
    #[serde(default)]
    pub active_games: Vec<ActiveGameInfo>,
    #[serde(default)]
    pub recent_messages: Vec<MessagePreview>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub my_role: i32,
    #[serde(default)]
    pub sfu_enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrewCounts {
    pub online: u32,
    pub total: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrewMember {
    pub user_id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub presence: Option<UserPresence>,
}

// ---------------------------------------------------------------------------
// Voice state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VoiceState {
    pub active: bool,
    #[serde(default)]
    pub members: Vec<VoiceMember>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VoiceMember {
    pub user_id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub speaking: Option<bool>,
    #[serde(default)]
    pub muted: Option<bool>,
    #[serde(default)]
    pub deafened: Option<bool>,
    #[serde(default)]
    pub joined_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// Voice join RPC response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct VoiceJoinResponse {
    pub channel_id: String,
    pub voice_state: VoiceSnapshot,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub sfu_endpoint: Option<String>,
    #[serde(default)]
    pub sfu_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VoiceSnapshot {
    pub channel_id: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub members: Vec<VoiceMember>,
}

// ---------------------------------------------------------------------------
// Voice channel state (multi-channel)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VoiceChannelState {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub members: Vec<VoiceMember>,
}

// ---------------------------------------------------------------------------
// Stream state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamState {
    pub active: bool,
    #[serde(default)]
    pub stream_id: Option<String>,
    #[serde(default)]
    pub streamer_id: Option<String>,
    #[serde(default)]
    pub streamer_username: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub viewer_count: u32,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

// ---------------------------------------------------------------------------
// Message preview
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessagePreview {
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub timestamp: String,
}

// ---------------------------------------------------------------------------
// Sidebar state (lighter view returned by crew_state_get_sidebar)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrewSidebarState {
    pub crew_id: String,
    #[serde(default)]
    pub name: String,
    pub counts: CrewCounts,
    #[serde(default)]
    pub voice: Option<VoiceState>,
    #[serde(default)]
    pub voice_channels: Vec<VoiceChannelState>,
    #[serde(default)]
    pub stream: Option<StreamState>,
    #[serde(default)]
    pub active_games: Vec<ActiveGameInfo>,
    #[serde(default)]
    pub recent_messages: Vec<MessagePreview>,
    #[serde(default)]
    pub idle: bool,
    #[serde(default)]
    pub sfu_enabled: bool,
}

// ---------------------------------------------------------------------------
// Crew event (priority push from server)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrewEvent {
    pub crew_id: String,
    pub event: String,
    #[serde(default)]
    pub data: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Active games (computed from member presences)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActiveGameInfo {
    pub game_id: String,
    pub game_name: String,
    #[serde(default)]
    pub short_name: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub players: Vec<PlayerInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayerInfo {
    pub user_id: String,
    pub username: String,
}

// ---------------------------------------------------------------------------
// Presence change push
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceChange {
    pub crew_id: String,
    pub user_id: String,
    pub presence: PresenceInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceInfo {
    pub status: PresenceStatus,
    #[serde(default)]
    pub activity: Option<Activity>,
    #[serde(default)]
    pub game: Option<GamePresence>,
}

// ---------------------------------------------------------------------------
// Voice update push
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceUpdate {
    pub crew_id: String,
    #[serde(default)]
    pub channel_id: String,
    #[serde(default)]
    pub members: Vec<VoiceMember>,
    #[serde(default)]
    pub voice_channels: Vec<VoiceChannelState>,
}

// ---------------------------------------------------------------------------
// Message preview push
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagePreviewUpdate {
    pub crew_id: String,
    #[serde(default)]
    pub messages: Vec<MessagePreview>,
}

// ---------------------------------------------------------------------------
// Sidebar batch push
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarUpdate {
    pub crews: Vec<CrewSidebarState>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crew_state_full_json() {
        // Simulate what set_active_crew RPC returns
        let json = r#"{
            "crew_id": "crew_xyz",
            "name": "The Vanguard",
            "counts": { "online": 8, "total": 12 },
            "members": [
                {
                    "user_id": "user_a",
                    "username": "k0ji_tech",
                    "avatar": "https://example.com/avatar.png",
                    "presence": {
                        "user_id": "user_a",
                        "status": "online",
                        "activity": { "type": "streaming", "crew_id": "crew_xyz", "stream_id": "s1", "stream_title": "AVALON" }
                    }
                }
            ],
            "voice": {
                "active": true,
                "members": [
                    { "user_id": "user_a", "username": "k0ji_tech", "speaking": false },
                    { "user_id": "user_b", "username": "ash_22" }
                ]
            },
            "stream": {
                "active": true,
                "stream_id": "stream_123",
                "streamer_id": "user_a",
                "streamer_username": "k0ji_tech",
                "title": "PROJECT AVALON",
                "viewer_count": 3,
                "thumbnail_url": "https://example.com/thumb.jpg"
            },
            "recent_messages": [
                { "username": "ash_22", "preview": "status check?", "timestamp": "2026-03-08T14:15:00Z" }
            ],
            "updated_at": "2026-03-08T14:16:00Z"
        }"#;

        let state: CrewState = serde_json::from_str(json).unwrap();
        assert_eq!(state.crew_id, "crew_xyz");
        assert_eq!(state.name, "The Vanguard");
        assert_eq!(state.counts.online, 8);
        assert_eq!(state.counts.total, 12);
        assert!(state.members.is_some());
        assert_eq!(state.members.as_ref().unwrap().len(), 1);
        assert!(state.voice.active);
        assert_eq!(state.voice.members.len(), 2);
        assert!(state.stream.is_some());
        assert_eq!(state.recent_messages.len(), 1);
    }

    #[test]
    fn crew_state_no_members() {
        // Sidebar-style: no members array
        let json = r#"{
            "crew_id": "crew_1",
            "name": "Crew",
            "counts": { "online": 0, "total": 5 },
            "voice": { "active": false, "members": [] },
            "recent_messages": []
        }"#;

        let state: CrewState = serde_json::from_str(json).unwrap();
        assert!(state.members.is_none());
        assert!(state.stream.is_none());
        assert!(!state.voice.active);
    }

    #[test]
    fn sidebar_state_json() {
        let json = r#"{
            "crew_id": "crew_abc",
            "name": "Neon Syndicate",
            "counts": { "online": 4, "total": 20 },
            "voice": {
                "active": true,
                "members": [
                    { "user_id": "u1", "username": "vex_r" },
                    { "user_id": "u2", "username": "lune" }
                ]
            },
            "stream": null,
            "recent_messages": [
                { "username": "vex_r", "preview": "yo who has the stash...", "timestamp": "2026-03-08T14:15:00Z" }
            ]
        }"#;

        let sidebar: CrewSidebarState = serde_json::from_str(json).unwrap();
        assert_eq!(sidebar.crew_id, "crew_abc");
        assert_eq!(sidebar.counts.online, 4);
        assert!(sidebar.voice.is_some());
        assert_eq!(sidebar.voice.as_ref().unwrap().members.len(), 2);
        assert!(sidebar.stream.is_none());
        assert!(!sidebar.idle);
    }

    #[test]
    fn sidebar_state_idle_crew() {
        let json = r#"{
            "crew_id": "crew_123",
            "name": "Ghost Recon",
            "counts": { "online": 0, "total": 8 },
            "idle": true
        }"#;

        let sidebar: CrewSidebarState = serde_json::from_str(json).unwrap();
        assert!(sidebar.idle);
        assert_eq!(sidebar.counts.online, 0);
    }

    #[test]
    fn crew_event_json() {
        let json = r#"{
            "crew_id": "crew_xyz",
            "event": "stream_started",
            "data": {
                "stream_id": "stream_123",
                "streamer_id": "user_a",
                "title": "PROJECT AVALON"
            }
        }"#;

        let event: CrewEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.crew_id, "crew_xyz");
        assert_eq!(event.event, "stream_started");
        assert_eq!(event.data["stream_id"], "stream_123");
    }

    #[test]
    fn presence_change_json() {
        let json = r#"{
            "crew_id": "crew_xyz",
            "user_id": "user_a",
            "presence": {
                "status": "online",
                "activity": { "type": "in_voice", "crew_id": "crew_xyz" }
            }
        }"#;

        let change: PresenceChange = serde_json::from_str(json).unwrap();
        assert_eq!(change.crew_id, "crew_xyz");
        assert_eq!(change.user_id, "user_a");
        assert_eq!(
            change.presence.status,
            crate::presence::PresenceStatus::Online
        );
    }

    #[test]
    fn voice_update_json() {
        let json = r#"{
            "crew_id": "crew_xyz",
            "members": [
                { "user_id": "user_a", "username": "vex_r", "speaking": true },
                { "user_id": "user_b", "username": "lune", "speaking": false }
            ]
        }"#;

        let update: VoiceUpdate = serde_json::from_str(json).unwrap();
        assert_eq!(update.crew_id, "crew_xyz");
        assert_eq!(update.members.len(), 2);
        assert_eq!(update.members[0].speaking, Some(true));
        assert_eq!(update.members[1].speaking, Some(false));
    }

    #[test]
    fn message_preview_update_json() {
        let json = r#"{
            "crew_id": "crew_abc",
            "messages": [
                { "username": "vex_r", "preview": "new msg here...", "timestamp": "2026-03-08T14:15:00Z" },
                { "username": "lune", "preview": "previous msg...", "timestamp": "2026-03-08T14:14:30Z" }
            ]
        }"#;

        let update: MessagePreviewUpdate = serde_json::from_str(json).unwrap();
        assert_eq!(update.crew_id, "crew_abc");
        assert_eq!(update.messages.len(), 2);
        assert_eq!(update.messages[0].username, "vex_r");
    }

    #[test]
    fn sidebar_update_json() {
        let json = r#"{
            "crews": [
                {
                    "crew_id": "crew_abc",
                    "name": "Neon Syndicate",
                    "counts": { "online": 4, "total": 20 }
                },
                {
                    "crew_id": "crew_def",
                    "name": "Deep Space",
                    "counts": { "online": 0, "total": 6 },
                    "idle": true
                }
            ]
        }"#;

        let update: SidebarUpdate = serde_json::from_str(json).unwrap();
        assert_eq!(update.crews.len(), 2);
        assert_eq!(update.crews[0].crew_id, "crew_abc");
        assert!(update.crews[1].idle);
    }

    #[test]
    fn voice_member_without_speaking() {
        // Sidebar voice members don't include speaking
        let json = r#"{ "user_id": "u1", "username": "alice" }"#;
        let vm: VoiceMember = serde_json::from_str(json).unwrap();
        assert_eq!(vm.user_id, "u1");
        assert!(vm.speaking.is_none());
    }

    #[test]
    fn stream_state_inactive() {
        let json = r#"{ "active": false }"#;
        let ss: StreamState = serde_json::from_str(json).unwrap();
        assert!(!ss.active);
        assert!(ss.stream_id.is_none());
        assert!(ss.thumbnail_url.is_none());
    }

    // ── Voice Channel Tests ─────────────────────────────────────────

    #[test]
    fn voice_channel_state_json() {
        // Backend may send extra fields (sort_order, active) — serde should ignore them
        let json = r#"{
            "id": "ch_abc12345",
            "name": "General",
            "is_default": true,
            "sort_order": 0,
            "active": true,
            "members": [
                { "user_id": "user_a", "username": "alice", "speaking": true },
                { "user_id": "user_b", "username": "bob" }
            ]
        }"#;

        let ch: VoiceChannelState = serde_json::from_str(json).unwrap();
        assert_eq!(ch.id, "ch_abc12345");
        assert_eq!(ch.name, "General");
        assert!(ch.is_default);
        assert_eq!(ch.members.len(), 2);
        assert_eq!(ch.members[0].speaking, Some(true));
        assert!(ch.members[1].speaking.is_none());
    }

    #[test]
    fn voice_channel_state_empty_members() {
        let json = r#"{
            "id": "ch_empty",
            "name": "AFK",
            "is_default": false
        }"#;

        let ch: VoiceChannelState = serde_json::from_str(json).unwrap();
        assert_eq!(ch.id, "ch_empty");
        assert!(!ch.is_default);
        assert!(ch.members.is_empty());
    }

    #[test]
    fn crew_state_with_voice_channels() {
        let json = r#"{
            "crew_id": "crew_vc",
            "name": "Vanguard",
            "counts": { "online": 3, "total": 10 },
            "voice": { "active": true, "members": [] },
            "voice_channels": [
                {
                    "id": "ch_gen",
                    "name": "General",
                    "is_default": true,
                    "members": [
                        { "user_id": "u1", "username": "alice", "speaking": false }
                    ]
                },
                {
                    "id": "ch_strat",
                    "name": "Strategy",
                    "is_default": false,
                    "members": []
                }
            ],
            "recent_messages": []
        }"#;

        let state: CrewState = serde_json::from_str(json).unwrap();
        assert_eq!(state.voice_channels.len(), 2);
        assert_eq!(state.voice_channels[0].name, "General");
        assert!(state.voice_channels[0].is_default);
        assert_eq!(state.voice_channels[0].members.len(), 1);
        assert_eq!(state.voice_channels[1].name, "Strategy");
        assert!(!state.voice_channels[1].is_default);
    }

    #[test]
    fn crew_state_without_voice_channels_defaults_empty() {
        // Backward compat: old JSON without voice_channels field
        let json = r#"{
            "crew_id": "crew_old",
            "name": "Legacy",
            "counts": { "online": 1, "total": 5 },
            "voice": { "active": false, "members": [] },
            "recent_messages": []
        }"#;

        let state: CrewState = serde_json::from_str(json).unwrap();
        assert!(state.voice_channels.is_empty());
    }

    #[test]
    fn sidebar_state_with_voice_channels() {
        let json = r#"{
            "crew_id": "crew_sb",
            "name": "Sidebar Crew",
            "counts": { "online": 2, "total": 8 },
            "voice_channels": [
                {
                    "id": "ch_1",
                    "name": "Lobby",
                    "is_default": true,
                    "sort_order": 0,
                    "active": true,
                    "members": [
                        { "user_id": "u1", "username": "vex" }
                    ]
                }
            ]
        }"#;

        let sidebar: CrewSidebarState = serde_json::from_str(json).unwrap();
        assert_eq!(sidebar.voice_channels.len(), 1);
        assert_eq!(sidebar.voice_channels[0].name, "Lobby");
        assert!(sidebar.voice_channels[0].is_default);
    }

    #[test]
    fn voice_update_with_channel_id() {
        let json = r#"{
            "crew_id": "crew_xyz",
            "channel_id": "ch_general",
            "members": [
                { "user_id": "user_a", "username": "alice", "speaking": true }
            ]
        }"#;

        let update: VoiceUpdate = serde_json::from_str(json).unwrap();
        assert_eq!(update.crew_id, "crew_xyz");
        assert_eq!(update.channel_id, "ch_general");
        assert_eq!(update.members.len(), 1);
    }

    #[test]
    fn voice_channel_state_roundtrip() {
        let ch = VoiceChannelState {
            id: "ch_test".to_string(),
            name: "Test".to_string(),
            is_default: false,
            members: vec![VoiceMember {
                user_id: "u1".to_string(),
                username: "alice".to_string(),
                speaking: Some(true),
                muted: None,
                deafened: None,
                joined_at: Some(1_700_000_000_000),
            }],
        };

        let serialized = serde_json::to_string(&ch).unwrap();
        let deserialized: VoiceChannelState = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.id, "ch_test");
        assert_eq!(deserialized.name, "Test");
        assert!(!deserialized.is_default);
        assert_eq!(deserialized.members.len(), 1);
        assert_eq!(deserialized.members[0].speaking, Some(true));
    }
}
