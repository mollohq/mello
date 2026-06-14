use serde::{Deserialize, Serialize};

use crate::crew::{Crew, Member, ResolvedInvite};
use crate::crew_state::{
    CrewEvent, CrewSidebarState, CrewState, MessagePreview, PresenceChange, VoiceChannelState,
    VoiceMember,
};
use crate::voice::AudioDevice;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub tag: String,
    /// Account creation time (unix seconds), from the user's Nakama metadata.
    pub created_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub message_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub content: String,
    pub timestamp: String,
    pub create_time: String,
    pub update_time: String,
    pub gif: Option<crate::chat::GifData>,
    pub reply_to: Option<String>,
    pub is_system: bool,
    pub is_edited: bool,
    pub is_deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSearchResult {
    pub id: String,
    pub display_name: String,
    pub is_friend: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
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
    UserAvatarLoaded {
        user_id: String,
        data: String,
    },
    ProfileUpdated {
        display_name: String,
        avatar_data: Option<String>,
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
    CrewInviteResolved {
        code: String,
        invite: ResolvedInvite,
    },
    CrewInviteResolveFailed {
        reason: String,
    },
    InviteCodeCreated {
        code: String,
    },
    InviteCodeCreateFailed {
        reason: String,
    },

    CrewUpdated {
        crew_id: String,
    },
    CrewUpdateFailed {
        reason: String,
    },
    CrewDeleted {
        crew_id: String,
    },
    CrewDeleteFailed {
        reason: String,
    },
    CrewRoleChanged {
        crew_id: String,
        user_id: String,
        new_role: i32,
    },
    CrewRoleChangeFailed {
        reason: String,
    },
    CrewMemberKicked {
        crew_id: String,
        user_id: String,
    },
    CrewMemberKickFailed {
        reason: String,
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
        /// `true` when Nakama returned a pagination cursor (older messages exist).
        has_more_history: bool,
    },
    HistoryLoaded {
        messages: Vec<ChatMessage>,
        cursor: Option<String>,
    },
    HistoryLoadFailed,
    ChatMessageEdited {
        message_id: String,
        new_content: String,
        update_time: String,
    },
    ChatMessageDeleted {
        message_id: String,
    },
    GifsLoaded {
        gifs: Vec<crate::chat::GifData>,
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

    AudioDeviceFallback {
        capture_fell_back: bool,
        playback_fell_back: bool,
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
        echo_cancellation_enabled: bool,
        agc_enabled: bool,
        noise_suppression_enabled: bool,
        packets_encoded: u32,
        aec_capture_frames: u32,
        aec_render_frames: u32,
        incoming_streams: i32,
        underrun_count: i32,
        rtp_recv_total: i32,
        pipeline_delay_ms: f32,
        rtt_ms: f32,
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
    WindowThumbnailsUpdated {
        /// (window_id, rgba_pixels, width, height)
        thumbnails: Vec<(String, Vec<u8>, u32, u32)>,
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
    StreamDebugStats {
        mode: String,
        transport_packets: u64,
        transport_bytes: u64,
        transport_truncations: u64,
        frames_presented: u64,
        present_fps: f32,
        ingress_kbps: f32,
    },
    StreamHostPacingStats {
        mode: String,
        target_kbps: u32,
        out_kbps: f32,
        paced_bytes: u64,
        sleep_count: u64,
        sleep_ms_total: u64,
        sleep_count_delta: u64,
        sleep_ms_delta: u64,
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
    /// SFU signaling reported a member joined or left; UI should refresh voice state.
    VoiceMembershipChanged {
        crew_id: String,
    },
    /// SFU voice connection was lost; client should attempt to re-join.
    VoiceSfuDisconnected {
        crew_id: String,
        reason: String,
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

    // --- Clips ---
    /// Clip buffer started (recording mixed audio).
    ClipBufferStarted,
    /// Clip buffer stopped.
    ClipBufferStopped,
    /// Clip captured and saved to local disk.
    ClipCaptured {
        clip_id: String,
        path: String,
        duration_seconds: f32,
    },
    /// Clip capture failed.
    ClipCaptureFailed {
        reason: String,
    },
    /// Clip metadata posted to backend.
    ClipPosted {
        clip_id: String,
        event_id: String,
    },
    /// Clip MP4 uploaded to cloud storage.
    ClipUploaded {
        clip_id: String,
        media_url: String,
    },
    /// Clip playback started.
    ClipPlaybackStarted {
        clip_path: String,
        duration_ms: u32,
    },
    /// Clip playback progress (polled from client).
    ClipPlaybackProgress {
        position_ms: u32,
        duration_ms: u32,
    },
    /// Clip playback finished (reached end or stopped).
    ClipPlaybackFinished,
    /// Crew feed timeline loaded.
    TimelineLoaded {
        response: crate::crew_events::TimelineResponse,
    },
    /// Server-curated feed loaded (sectioned: this_week + memory).
    FeedLoaded {
        response: crate::crew_events::FeedResponse,
    },

    // --- Crew events (event ledger) ---
    /// Catch-up data loaded for a crew.
    CatchupLoaded {
        response: crate::crew_events::CatchupResponse,
    },
    /// Moment posted successfully.
    MomentPosted {
        event_id: String,
    },
    /// Moment post failed.
    MomentPostFailed {
        reason: String,
    },

    // --- Game sensing ---
    /// A game process was detected.
    GameDetected {
        game_id: String,
        game_name: String,
        short_name: String,
        color: String,
        pid: u32,
    },
    /// A game process exited.
    GameEnded {
        game_id: String,
        game_name: String,
        short_name: String,
        duration_min: u32,
    },
    /// Post-game prompt timed out without interaction.
    PostGameTimeout,

    /// Client-server protocol version mismatch.
    ProtocolMismatch {
        message: String,
        /// true = client too old (needs update), false = server too old
        client_outdated: bool,
    },

    /// Realtime (Nakama WebSocket) connection state changed. Lets the UI show a
    /// "reconnecting" banner instead of silently freezing on stale state.
    ConnectionStateChanged {
        /// True when the realtime socket is up.
        connected: bool,
        /// True while a reconnect attempt is in flight.
        reconnecting: bool,
    },

    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The FFI boundary (core -> Swift) serializes events as adjacently-tagged
    /// JSON: `{ "type": <Variant>, "data": { ...fields } }`. Lock that shape.
    #[test]
    fn struct_variant_is_adjacently_tagged() {
        let json = serde_json::to_value(Event::LoggedIn {
            user: User {
                id: "u1".into(),
                username: "vex".into(),
                display_name: "Vex".into(),
                tag: "0001".into(),
                created_at: None,
            },
        })
        .unwrap();
        assert_eq!(json["type"], "LoggedIn");
        assert_eq!(json["data"]["user"]["id"], "u1");
    }

    #[test]
    fn unit_variant_has_type_only() {
        let json = serde_json::to_value(Event::Restoring).unwrap();
        assert_eq!(json["type"], "Restoring");
        assert!(json.get("data").is_none());
    }
}
