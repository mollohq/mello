use crate::crew::{Crew, Member};
use crate::crew_state::{
    CrewEvent, CrewSidebarState, CrewState, MessagePreview, PresenceChange, VoiceChannelState,
    VoiceMember,
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
pub struct CaptureSource {
    pub id: String,
    pub name: String,
    pub mode: String,
    pub monitor_index: Option<u32>,
    pub hwnd: Option<u64>,
    pub pid: Option<u32>,
    pub exe: String,
    pub is_fullscreen: bool,
    pub resolution: String,
}

#[derive(Debug, Clone)]
pub struct UserSearchResult {
    pub id: String,
    pub display_name: String,
    pub is_friend: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    Restoring,
    LoggedIn {
        user: User,
    },
    LoginFailed {
        reason: String,
    },

    DeviceAuthed {
        user: User,
        created: bool,
    },
    DiscoverCrewsLoaded {
        crews: Vec<Crew>,
        cursor: Option<String>,
    },
    OnboardingReady {
        user: User,
    },
    OnboardingFailed {
        reason: String,
    },
    EmailLinked,
    EmailLinkFailed {
        reason: String,
    },
    SocialLinked,
    SocialLinkFailed {
        reason: String,
    },

    CrewCreated {
        crew: Crew,
        invite_code: Option<String>,
    },
    CrewCreateFailed {
        reason: String,
    },
    CrewAvatarLoaded {
        crew_id: String,
        data: String,
    },
    UserSearchResults {
        users: Vec<UserSearchResult>,
    },
    CrewsLoaded {
        crews: Vec<Crew>,
    },
    CrewJoined {
        crew_id: String,
    },
    CrewLeft {
        crew_id: String,
    },

    MemberJoined {
        crew_id: String,
        member: Member,
    },
    MemberLeft {
        crew_id: String,
        member_id: String,
    },
    PresenceUpdated {
        user_id: String,
        online: bool,
    },

    MessageReceived {
        message: ChatMessage,
    },
    MessagesLoaded {
        messages: Vec<ChatMessage>,
    },

    VoiceStateChanged {
        in_call: bool,
    },
    VoiceConnected {
        peer_id: String,
    },
    VoiceDisconnected {
        peer_id: String,
    },
    VoiceActivity {
        member_id: String,
        speaking: bool,
    },

    MicPermissionChanged {
        granted: bool,
        denied: bool,
    },

    AudioDevicesListed {
        capture: Vec<AudioDevice>,
        playback: Vec<AudioDevice>,
    },

    MicLevel {
        level: f32,
    },

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

    SignalReceived {
        from: String,
        payload: String,
    },

    // --- Streaming ---
    CaptureSourcesListed {
        monitors: Vec<CaptureSource>,
        games: Vec<CaptureSource>,
        windows: Vec<CaptureSource>,
    },
    StreamStarted {
        crew_id: String,
        session_id: String,
        mode: String,
    },
    StreamEnded {
        crew_id: String,
    },
    StreamViewerJoined {
        viewer_id: String,
    },
    StreamViewerLeft {
        viewer_id: String,
    },
    StreamWatching {
        host_id: String,
        width: u32,
        height: u32,
    },
    StreamWatchingStopped,
    StreamFrame {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    StreamError {
        message: String,
    },

    // --- Presence & crew state ---
    /// Full crew state loaded for the active crew.
    CrewStateLoaded {
        state: CrewState,
    },
    /// Batched sidebar update for non-active crews.
    SidebarUpdated {
        crews: Vec<CrewSidebarState>,
    },
    /// Priority crew event (stream_started, voice_joined, etc.).
    CrewEventReceived {
        event: CrewEvent,
    },
    /// A member's presence changed in the active crew.
    PresenceChanged {
        change: PresenceChange,
    },
    /// Local user successfully joined a voice channel (RPC response).
    VoiceJoined {
        crew_id: String,
        channel_id: String,
        members: Vec<VoiceMember>,
    },
    /// Voice state update for a channel in the active crew (includes speaking).
    VoiceUpdated {
        crew_id: String,
        channel_id: String,
        members: Vec<VoiceMember>,
    },
    /// Full voice channels state refreshed for the active crew.
    VoiceChannelsUpdated {
        crew_id: String,
        channels: Vec<VoiceChannelState>,
    },
    /// A voice channel was created.
    VoiceChannelCreated {
        crew_id: String,
        channel: VoiceChannelState,
    },
    /// A voice channel was renamed.
    VoiceChannelRenamed {
        crew_id: String,
        channel_id: String,
        name: String,
    },
    /// A voice channel was deleted.
    VoiceChannelDeleted {
        crew_id: String,
        channel_id: String,
    },
    /// Throttled message preview for a sidebar crew.
    MessagePreviewUpdated {
        crew_id: String,
        messages: Vec<MessagePreview>,
    },

    /// Client-server protocol version mismatch.
    ProtocolMismatch {
        message: String,
        /// true = client too old (needs update), false = server too old
        client_outdated: bool,
    },

    Error {
        message: String,
    },
}
