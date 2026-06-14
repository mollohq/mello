use serde::{Deserialize, Serialize};

use crate::presence::{Activity, PresenceStatus};
use crate::voice::NsMode;

fn default_preset() -> u32 {
    2
} // Medium

fn default_clip_seconds() -> f32 {
    30.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
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
    /// Authenticate (login or create) with an Apple identity token (JWT) obtained
    /// natively on the client. Desktop has no native flow → sends an empty token.
    AuthApple {
        identity_token: String,
    },

    // Social link (onboarding step 3 — links identity to existing device account)
    LinkGoogle,
    LinkDiscord,
    /// Link an Apple identity (native identity token) onto the current session.
    LinkApple {
        identity_token: String,
    },
    /// Link a Google identity using an id_token obtained natively on the client
    /// (iOS ASWebAuthenticationSession). Mirrors `LinkGoogle` but skips the in-core
    /// browser flow. Falls back to authenticate if already linked elsewhere.
    LinkGoogleToken {
        id_token: String,
    },
    /// Link a custom-provider identity (Discord, Twitch) using a token obtained
    /// natively on the client. Falls back to authenticate if already linked.
    LinkCustomToken {
        token: String,
        provider: String,
    },

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
    CreateInviteCode {
        crew_id: String,
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
    /// Enable/disable auto-joining a crew's voice channel on `SelectCrew`.
    /// Defaults to enabled (desktop). iOS disables it so voice (and the mic
    /// permission prompt) only starts on an explicit join.
    SetVoiceAutoJoin {
        enabled: bool,
    },
    VoiceSpeaking {
        speaking: bool,
    },
    SetMute {
        muted: bool,
    },
    SetPushToTalk {
        enabled: bool,
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
    SetNsMode {
        mode: NsMode,
    },
    SetTransientSuppression {
        enabled: bool,
    },
    SetHighPassFilter {
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
    StartVoiceCaptureInject,
    InjectCaptureFrame {
        samples: Vec<i16>,
    },
    StopVoiceCaptureInject,
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

    // --- Crew admin ---
    UpdateCrew {
        crew_id: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        avatar: Option<String>,
        #[serde(default)]
        open: Option<bool>,
        #[serde(default)]
        invite_policy: Option<String>,
    },
    DeleteCrew {
        crew_id: String,
    },
    ChangeCrewRole {
        crew_id: String,
        user_id: String,
        new_role: i32,
    },
    KickCrewMember {
        crew_id: String,
        user_id: String,
    },

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
    // Server-curated feed (this_week + memory sections). Primary feed load;
    // LoadCrewTimeline stays for later deep-scroll pagination.
    LoadCrewFeed {
        crew_id: String,
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

    // --- Test/dev fault injection (feature-gated; never compiled into prod) ---
    /// Force the realtime Nakama WebSocket down so the supervisor's reconnect
    /// path is exercised.
    #[cfg(feature = "test-faults")]
    FaultNakamaDisconnect,
    /// Force the SFU voice session into a disconnected state so the voice
    /// tick's reconnect scheduler rebuilds it.
    #[cfg(feature = "test-faults")]
    FaultSfuDisconnect,
    /// Backdate the liveness clock so the next connection tick detects a
    /// sleep/wake gap and triggers a full reconnect + resync.
    #[cfg(feature = "test-faults")]
    FaultSimulateSuspend,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The FFI boundary (Swift -> core) relies on adjacently-tagged JSON:
    /// `{ "type": <Variant>, "data": { ...fields } }`. Lock that shape so a
    /// future enum change can't silently break the Swift `Codable` mirror.
    #[test]
    fn struct_variant_is_adjacently_tagged() {
        let json = serde_json::to_value(Command::DeviceAuth {
            device_id: "dev_123".into(),
        })
        .unwrap();
        assert_eq!(json["type"], "DeviceAuth");
        assert_eq!(json["data"]["device_id"], "dev_123");
    }

    #[test]
    fn unit_variant_has_type_only() {
        let json = serde_json::to_value(Command::TryRestore).unwrap();
        assert_eq!(json["type"], "TryRestore");
        assert!(json.get("data").is_none());
    }

    #[test]
    fn deserializes_from_swift_shape() {
        let cmd: Command =
            serde_json::from_str(r#"{"type":"SelectCrew","data":{"crew_id":"crew_abc"}}"#).unwrap();
        assert!(matches!(cmd, Command::SelectCrew { crew_id } if crew_id == "crew_abc"));
    }
}
