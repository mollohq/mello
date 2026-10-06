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
    /// Permanently delete the signed-in account, then clear the local session.
    ///
    /// Irreversible. Crews the account owns are **not** removed — send
    /// [`Command::DeleteCrew`] first if they should go too, otherwise they
    /// outlive the account as empty groups.
    DeleteAccount,

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
    /// Authenticate (login or create) with a Google id_token obtained natively on
    /// the client (iOS). Sign-in counterpart to `LinkGoogleToken` — no browser flow.
    AuthGoogleToken {
        id_token: String,
    },
    /// Authenticate (login or create) with a custom-provider token (Discord/Twitch)
    /// obtained natively on the client (iOS). Sign-in counterpart to `LinkCustomToken`.
    AuthCustomToken {
        token: String,
        provider: String,
    },

    // Social link (onboarding step 3 — links identity to existing device account)
    LinkGoogle,
    LinkDiscord,
    /// Link a Steam identity (OpenID) to the current device account.
    ///
    /// Distinct from [`Command::AuthSteam`], which *signs in* with `create=false`
    /// and therefore fails for anyone who has not linked Steam before. Onboarding
    /// sent that one because this did not exist.
    LinkSteam,
    /// Link a Twitch identity (OAuth) to the current device account. Same gap as
    /// [`Command::LinkSteam`].
    LinkTwitch,
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
        /// Stable per-install device identity. **Must** be the same value on
        /// every attempt: it is what makes finalize idempotent. A fresh id here
        /// authenticates as a new device and Nakama creates another account.
        device_id: String,
        /// The invite code of the link that opened a fresh install. When set,
        /// finalize joins that crew with `join_by_invite_code`: the code is
        /// the authorization, so a private crew works too. It wins over
        /// `crew_id` and `crew_name`.
        #[serde(default)]
        invite_code: Option<String>,
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
        /// The composer text, with mentions written as `@name`.
        content: String,
        #[serde(default)]
        reply_to: Option<String>,
        /// The members the user picked from mention autocomplete. The core
        /// turns each picked `@name` in `content` into a `<@user_id>` token.
        #[serde(default)]
        mentions: Vec<crate::chat::MentionRef>,
    },
    /// Register this device for remote push (spec 23 §3). Best-effort: a
    /// failed RPC is logged. The core keeps the token so `Logout` can remove it.
    RegisterPushToken {
        token: String,
        /// `"ios"` or `"android"`.
        platform: String,
        /// `"production"` or `"sandbox"` (APNs). Defaults to production.
        #[serde(default)]
        environment: Option<String>,
    },
    /// The app window's state, sent periodically by the UI (spec 23 §6.2). The
    /// core combines it with voice, stream and game state into one "active"
    /// flag and reports changes to the server.
    SetWindowActivity {
        /// Desktop: the window is visible and focused. iOS: the app is in the foreground.
        foreground: bool,
        /// Seconds since the last system-wide keyboard or mouse input (0 if unknown).
        #[serde(default)]
        input_idle_secs: u64,
    },
    SendGif {
        gif: crate::chat::GifData,
        #[serde(default)]
        body: String,
    },
    EditMessage {
        message_id: String,
        /// The edited text, with mentions written as `@name`.
        new_body: String,
        /// The message's existing mentions plus any new picks. See `SendMessage::mentions`.
        #[serde(default)]
        mentions: Vec<crate::chat::MentionRef>,
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
    SetEchoSuppression {
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
    /// Input sensitivity of the speech gate (spec 10 section 8). `auto`: the
    /// gate tracks the ambient noise floor. Otherwise it opens when the raw
    /// microphone level reaches `db` dBFS (-100..0).
    SetInputSensitivity {
        auto: bool,
        db: f32,
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
    /// Toggle diagnostic capture: raises libmello log verbosity and writes
    /// per-frame audio stats to the log so a user can self-capture a repro.
    /// The client side bumps the Rust log filter + slices/uploads the file.
    SetDiagnosticCapture {
        enabled: bool,
    },
    /// Upload a sliced diagnostic log file to private storage via a presigned
    /// URL. Driven by the client after it stops a capture and writes the slice.
    UploadDiagnosticLog {
        local_path: String,
        capture_id: String,
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
        /// Game executable name (e.g. "Heaven.exe"), for the hook policy
        /// decision. Empty when the source is not a game.
        #[serde(default)]
        exe: String,
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
        #[serde(default)]
        waveform: String,
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
        game_id: String,
        #[serde(default)]
        duration_min: u32,
        #[serde(default)]
        wins: u32,
        #[serde(default)]
        losses: u32,
        #[serde(default)]
        draws: u32,
    },
    /// Share an exe-extracted game icon with the crew (raw PNG bytes;
    /// base64-encoded for the RPC). Best-effort; server keeps the first
    /// upload per game id.
    #[serde(skip)]
    UploadGameIcon {
        game_id: String,
        png: Vec<u8>,
    },
    /// Fetch a crew-shared icon for a game id with no local art. Replies with
    /// `Event::GameIconLoaded` on success; quiet when none exists.
    FetchGameIcon {
        game_id: String,
    },
    /// Fetch the viewer's own per-game stats (for the personal "You strip").
    GetUserGameStats,

    // --- Games settings / integrations ---
    /// Load per-game integration info (adapter registry + install detection)
    /// and the Riot link status for the Games settings page.
    LoadGamesSettings,
    /// Replace the set of game integrations the user has switched off.
    /// Disabled integrations skip config installs and active transports.
    SetGameIntegrations {
        #[serde(default)]
        disabled: Vec<String>,
    },
    /// Toggle whether sensed play is shared with crews (presence + session-end).
    SetShareGameActivity {
        enabled: bool,
    },
    /// Link the user's Riot ID ("GameName#TAG") for server-verified results.
    RiotLink {
        riot_id: String,
        region: String,
    },
    RiotUnlink,
    /// Query the Riot link state (emits Event::RiotStatus).
    LoadRiotStatus,

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
    /// Test only: hold the command loop for `ms` milliseconds with a blocking
    /// sleep, as a hung native call or a slow RPC holds it. `token` names the
    /// hold for `client::loop_hold`.
    #[cfg(test)]
    TestHoldLoop {
        token: u64,
        ms: u64,
    },
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

    /// The exact JSON `Command.swift` sends today (no `environment`) must
    /// still decode, so an older iOS build keeps registering.
    #[test]
    fn register_push_token_decodes_the_swift_shape() {
        let cmd: Command = serde_json::from_str(
            r#"{"type":"RegisterPushToken","data":{"token":"ab12","platform":"ios"}}"#,
        )
        .unwrap();
        assert!(matches!(
            cmd,
            Command::RegisterPushToken { ref token, ref platform, environment: None }
                if token == "ab12" && platform == "ios"
        ));

        let cmd: Command = serde_json::from_str(
            r#"{"type":"RegisterPushToken","data":{"token":"ab12","platform":"ios","environment":"sandbox"}}"#,
        )
        .unwrap();
        assert!(matches!(
            cmd,
            Command::RegisterPushToken { environment: Some(ref e), .. } if e == "sandbox"
        ));
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
