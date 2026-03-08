use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    TryRestore,
    Login { email: String, password: String },
    Logout,
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
}
