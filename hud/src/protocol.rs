use serde::{Deserialize, Serialize};

/// IPC message sent from the main client to the HUD process.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HudMessage {
    #[serde(rename = "state")]
    State(Box<HudState>),
    #[serde(rename = "settings")]
    Settings(HudSettings),
    #[serde(rename = "shutdown")]
    Shutdown,
}

/// Full HUD state pushed from the main client.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HudState {
    pub mode: HudMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crew: Option<HudCrew>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<HudVoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_messages: Option<Vec<HudChatMessage>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_card: Option<HudStreamCard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip_toast: Option<HudClipToast>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HudMode {
    #[default]
    Hidden,
    MiniPlayer,
    Overlay,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudCrew {
    pub name: String,
    pub initials: String,
    pub online_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudVoice {
    pub channel_name: String,
    pub members: Vec<HudVoiceMember>,
    pub self_muted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudVoiceMember {
    pub id: String,
    pub display_name: String,
    pub initials: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_rgba: Option<String>,
    pub speaking: bool,
    pub muted: bool,
    #[serde(default)]
    pub is_self: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudChatMessage {
    pub display_name: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudStreamCard {
    pub streamer: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudClipToast {
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudSettings {
    #[serde(default = "default_overlay_opacity")]
    pub overlay_opacity: f32,
    #[serde(default = "default_true")]
    pub show_clip_toasts: bool,
    #[serde(default = "default_true")]
    pub overlay_enabled: bool,
}

fn default_overlay_opacity() -> f32 {
    0.8
}
fn default_true() -> bool {
    true
}

impl Default for HudSettings {
    fn default() -> Self {
        Self {
            overlay_opacity: 0.8,
            show_clip_toasts: true,
            overlay_enabled: true,
        }
    }
}

/// IPC message sent from the HUD process back to the main client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HudAction {
    #[serde(rename = "action")]
    Action { action: HudActionKind },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HudActionKind {
    MuteToggle,
    DeafenToggle,
    LeaveVoice,
    OpenCrew,
    OpenStream,
    OpenSettings,
}

pub const STATE_PIPE_NAME: &str = r"\\.\pipe\m3llo-hud-state";
pub const ACTION_PIPE_NAME: &str = r"\\.\pipe\m3llo-hud-action";
