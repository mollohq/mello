use serde::{Deserialize, Serialize};

use crate::presence::{Activity, PresenceStatus};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    TryRestore,
    DeviceAuth { device_id: String },
    Login { email: String, password: String },
    LinkEmail { email: String, password: String },
    Logout,
    DiscoverCrews,
    LoadMyCrews,
    JoinCrew { crew_id: String },
    CreateCrew { name: String },
    SelectCrew { crew_id: String },
    LeaveCrew,
    SendMessage { content: String },
    JoinVoice,
    LeaveVoice,
    SetMute { muted: bool },
    SetDeafen { deafened: bool },
    ListAudioDevices,
    SetCaptureDevice { id: String },
    SetPlaybackDevice { id: String },
    SetLoopback { enabled: bool },
    SetDebugMode { enabled: bool },
    UpdateProfile { display_name: String },

    // --- Presence & crew state ---
    UpdatePresence {
        status: PresenceStatus,
        #[serde(default)]
        activity: Option<Activity>,
    },
    SetActiveCrew { crew_id: String },
    SubscribeSidebar { crew_ids: Vec<String> },
}
