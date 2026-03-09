use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    TryRestore,
    DeviceAuth { device_id: String },
    Login { email: String, password: String },
    LinkEmail { email: String, password: String },
    Logout,
    DiscoverCrews,
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
}
