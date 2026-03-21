use serde::{Deserialize, Serialize};

use crate::presence::{Activity, PresenceStatus};

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
    DiscoverCrews,
    FinalizeOnboarding {
        crew_id: Option<String>,
        crew_name: Option<String>,
        display_name: String,
        avatar: u8,
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
    },
    FetchCrewAvatars {
        crew_ids: Vec<String>,
    },
    SelectCrew {
        crew_id: String,
    },
    LeaveCrew,
    SendMessage {
        content: String,
    },
    JoinVoice {
        channel_id: String,
    },
    LeaveVoice,
    SetMute {
        muted: bool,
    },
    SetDeafen {
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
    SetLoopback {
        enabled: bool,
    },
    SetDebugMode {
        enabled: bool,
    },
    UpdateProfile {
        display_name: String,
    },

    // --- Streaming ---
    StartStream {
        crew_id: String,
        #[serde(default)]
        title: String,
    },
    StopStream,
    WatchStream {
        host_id: String,
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
}
