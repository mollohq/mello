use crate::crew::{Crew, Member};
use crate::crew_state::{
    CrewEvent, CrewSidebarState, CrewState, MessagePreview, PresenceChange, VoiceMember,
};
use crate::voice::AudioDevice;

#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub tag: String,
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub message_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub content: String,
    pub timestamp: String,
}

#[derive(Debug, Clone)]
pub enum Event {
    Restoring,
    LoggedIn { user: User },
    LoginFailed { reason: String },

    DeviceAuthed { user: User, created: bool },
    DiscoverCrewsLoaded { crews: Vec<Crew> },
    EmailLinked,
    EmailLinkFailed { reason: String },

    CrewCreated { crew: Crew },
    CrewCreateFailed { reason: String },
    CrewsLoaded { crews: Vec<Crew> },
    CrewJoined { crew_id: String },
    CrewLeft { crew_id: String },

    MemberJoined { crew_id: String, member: Member },
    MemberLeft { crew_id: String, member_id: String },
    PresenceUpdated { user_id: String, online: bool },

    MessageReceived { message: ChatMessage },
    MessagesLoaded { messages: Vec<ChatMessage> },

    VoiceStateChanged { in_call: bool },
    VoiceConnected { peer_id: String },
    VoiceDisconnected { peer_id: String },
    VoiceActivity { member_id: String, speaking: bool },

    AudioDevicesListed {
        capture: Vec<AudioDevice>,
        playback: Vec<AudioDevice>,
    },

    MicLevel { level: f32 },

    AudioDebugStats {
        input_level: f32,
        silero_vad_prob: f32,
        rnnoise_prob: f32,
        is_speaking: bool,
        is_capturing: bool,
        is_muted: bool,
        is_deafened: bool,
        packets_encoded: u32,
    },

    SignalReceived { from: String, payload: String },

    // --- Presence & crew state ---

    /// Full crew state loaded for the active crew.
    CrewStateLoaded { state: CrewState },
    /// Batched sidebar update for non-active crews.
    SidebarUpdated { crews: Vec<CrewSidebarState> },
    /// Priority crew event (stream_started, voice_joined, etc.).
    CrewEventReceived { event: CrewEvent },
    /// A member's presence changed in the active crew.
    PresenceChanged { change: PresenceChange },
    /// Voice state update for the active crew (includes speaking).
    VoiceUpdated { crew_id: String, members: Vec<VoiceMember> },
    /// Throttled message preview for a sidebar crew.
    MessagePreviewUpdated { crew_id: String, messages: Vec<MessagePreview> },

    Error { message: String },
}
