use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    Login { email: String, password: String },
    CreateCrew { name: String },
    SelectCrew { crew_id: String },
    LeaveCrew,
    SendMessage { content: String },
    SetMute { muted: bool },
    SetDeafen { deafened: bool },
}
