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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_message_round_trip() {
        let msg = HudMessage::State(Box::new(HudState {
            mode: HudMode::Overlay,
            crew: Some(HudCrew {
                name: "TestCrew".into(),
                initials: "TC".into(),
                online_count: 3,
            }),
            voice: Some(HudVoice {
                channel_name: "General".into(),
                members: vec![HudVoiceMember {
                    id: "u1".into(),
                    display_name: "alice".into(),
                    initials: "AL".into(),
                    avatar_rgba: None,
                    speaking: true,
                    muted: false,
                    is_self: true,
                }],
                self_muted: false,
            }),
            recent_messages: None,
            stream_card: Some(HudStreamCard {
                streamer: "bob".into(),
                title: "Ranked grind".into(),
            }),
            clip_toast: None,
        }));

        let json = serde_json::to_string(&msg).unwrap();
        let parsed: HudMessage = serde_json::from_str(&json).unwrap();

        match parsed {
            HudMessage::State(s) => {
                assert_eq!(s.mode, HudMode::Overlay);
                assert_eq!(s.crew.as_ref().unwrap().name, "TestCrew");
                assert_eq!(s.voice.as_ref().unwrap().members.len(), 1);
                assert!(s.voice.as_ref().unwrap().members[0].speaking);
                assert!(s.stream_card.is_some());
                assert!(s.clip_toast.is_none());
            }
            _ => panic!("expected State"),
        }
    }

    #[test]
    fn settings_defaults_on_missing_fields() {
        let json = r#"{"type":"settings"}"#;
        let msg: HudMessage = serde_json::from_str(json).unwrap();
        match msg {
            HudMessage::Settings(s) => {
                assert!((s.overlay_opacity - 0.8).abs() < f32::EPSILON);
                assert!(s.show_clip_toasts);
                assert!(s.overlay_enabled);
            }
            _ => panic!("expected Settings"),
        }
    }

    #[test]
    fn action_round_trip() {
        let actions = [
            HudActionKind::MuteToggle,
            HudActionKind::DeafenToggle,
            HudActionKind::LeaveVoice,
            HudActionKind::OpenCrew,
            HudActionKind::OpenStream,
            HudActionKind::OpenSettings,
        ];
        for kind in actions {
            let msg = HudAction::Action { action: kind };
            let json = serde_json::to_string(&msg).unwrap();
            let parsed: HudAction = serde_json::from_str(&json).unwrap();
            match parsed {
                HudAction::Action { action } => assert_eq!(action, kind),
            }
        }
    }

    #[test]
    fn shutdown_message() {
        let json = r#"{"type":"shutdown"}"#;
        let msg: HudMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, HudMessage::Shutdown));
    }

    #[test]
    fn optional_fields_omitted_when_none() {
        let state = HudState {
            mode: HudMode::Hidden,
            ..Default::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("crew"));
        assert!(!json.contains("voice"));
        assert!(!json.contains("stream_card"));
    }
}
