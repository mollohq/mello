use serde::{Deserialize, Serialize};

use crate::presence::{Activity, PresenceStatus};

fn default_preset() -> u32 {
    2
} // Medium

fn default_clip_seconds() -> f32 {
    30.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    TryRestore,
    DeviceAuth {
        device_id: String,
    },
    Login {
        email: String,
        password: String,
    },
    LinkEmail {
        email: String,
        password: String,
    },
    Logout,

    // Social auth (login screen — creates or logs into account)
    AuthSteam,
    AuthGoogle,
    AuthTwitch,
    AuthDiscord,
    AuthApple,

    // Social link (onboarding step 3 — links identity to existing device account)
    LinkGoogle,
    LinkDiscord,

    // Onboarding
    DiscoverCrews {
        #[serde(default)]
        cursor: Option<String>,
    },
    FinalizeOnboarding {
        crew_id: Option<String>,
        crew_name: Option<String>,
        #[serde(default)]
        crew_description: Option<String>,
        #[serde(default)]
        crew_open: Option<bool>,
        #[serde(default)]
        crew_avatar: Option<String>,
        display_name: String,
        #[serde(default)]
        avatar_data: Option<String>,
        #[serde(default)]
        avatar_format: Option<String>,
        #[serde(default)]
        avatar_style: Option<String>,
        #[serde(default)]
        avatar_seed: Option<String>,
    },
    LoadMyCrews,
    JoinCrew {
        crew_id: String,
    },
    CreateCrew {
        name: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        open: bool,
        #[serde(default)]
        avatar: Option<String>,
        #[serde(default)]
        invite_user_ids: Vec<String>,
    },
    FetchCrewAvatars {
        crew_ids: Vec<String>,
    },
    FetchUserAvatar {
        user_id: String,
    },
    FetchUserAvatars {
        user_ids: Vec<String>,
    },
    SearchUsers {
        query: String,
    },
    JoinByInviteCode {
        code: String,
    },
    ResolveCrewInvite {
        code: String,
    },
    SelectCrew {
        crew_id: String,
    },
    LeaveCrew,
    SendMessage {
        content: String,
        #[serde(default)]
        reply_to: Option<String>,
    },
    SendGif {
        gif: crate::chat::GifData,
        #[serde(default)]
        body: String,
    },
    EditMessage {
        message_id: String,
        new_body: String,
    },
    DeleteMessage {
        message_id: String,
    },
    LoadHistory {
        cursor: Option<String>,
    },
    SearchGifs {
        query: String,
    },
    LoadTrendingGifs,
    JoinVoice {
        channel_id: String,
    },
    LeaveVoice,
    VoiceSpeaking {
        speaking: bool,
    },
    SetMute {
        muted: bool,
    },
    SetDeafen {
        deafened: bool,
    },
    BroadcastMuteState {
        muted: bool,
        deafened: bool,
    },
    CheckMicPermission,
    RequestMicPermission,
    ListAudioDevices,
    SetCaptureDevice {
        id: String,
    },
    SetPlaybackDevice {
        id: String,
    },
    SetEchoCancellation {
        enabled: bool,
    },
    SetAgc {
        enabled: bool,
    },
    SetNoiseSuppression {
        enabled: bool,
    },
    SetInputVolume {
        volume: f32,
    },
    SetOutputVolume {
        volume: f32,
    },
    SetLoopback {
        enabled: bool,
    },
    SetDebugMode {
        enabled: bool,
    },
    UpdateProfile {
        display_name: String,
        avatar_data: Option<String>,
        avatar_format: Option<String>,
        avatar_style: Option<String>,
        avatar_seed: Option<String>,
    },

    // --- Streaming ---
    ListCaptureSources,
    StartThumbnailRefresh,
    StopThumbnailRefresh,
    StartStream {
        crew_id: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        capture_mode: String,
        #[serde(default)]
        monitor_index: Option<u32>,
        #[serde(default)]
        hwnd: Option<u64>,
        #[serde(default)]
        pid: Option<u32>,
        /// Quality preset index: 0=Ultra, 1=High, 2=Medium, 3=Low, 4=Potato
        #[serde(default = "default_preset")]
        preset: u32,
    },
    StopStream,
    WatchStream {
        host_id: String,
        #[serde(default)]
        session_id: String,
        #[serde(default)]
        width: u32,
        #[serde(default)]
        height: u32,
    },
    StopWatching,

    // --- Voice channels CRUD ---
    CreateVoiceChannel {
        crew_id: String,
        name: String,
    },
    RenameVoiceChannel {
        crew_id: String,
        channel_id: String,
        name: String,
    },
    DeleteVoiceChannel {
        crew_id: String,
        channel_id: String,
    },

    // --- Presence & crew state ---
    UpdatePresence {
        status: PresenceStatus,
        #[serde(default)]
        activity: Option<Activity>,
    },
    SetActiveCrew {
        crew_id: String,
    },
    SubscribeSidebar {
        crew_ids: Vec<String>,
    },

    // --- Clips ---
    StartClipBuffer,
    StopClipBuffer,
    CaptureClip {
        #[serde(default = "default_clip_seconds")]
        seconds: f32,
    },
    PostClip {
        crew_id: String,
        clip_id: String,
        duration_seconds: f64,
        #[serde(default)]
        local_path: String,
    },
    UploadClip {
        crew_id: String,
        clip_id: String,
        wav_path: String,
    },
    PlayClip {
        path: String,
    },
    PauseClip,
    ResumeClip,
    SeekClip {
        position_ms: u32,
    },
    StopClipPlayback,
    LoadCrewTimeline {
        crew_id: String,
        #[serde(default)]
        cursor: Option<String>,
    },

    // --- Crew events (event ledger) ---
    CrewCatchup {
        crew_id: String,
        #[serde(default)]
        last_seen: i64,
    },
    PostMoment {
        crew_id: String,
        sentiment: String,
        #[serde(default)]
        text: String,
        #[serde(default)]
        game_name: String,
    },
    GameSessionEnd {
        crew_id: String,
        game_name: String,
        #[serde(default)]
        duration_min: u32,
    },
}
