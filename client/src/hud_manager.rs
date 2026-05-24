use std::sync::mpsc;

use serde::{Deserialize, Serialize};

// ── IPC protocol types (kept for state builder / serde compatibility) ────────

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
    Overlay,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudCrew {
    pub name: String,
    pub initials: String,
    pub online_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_rgba: Option<String>,
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
    pub overlay_opacity: f32,
    pub show_clip_toasts: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HudAction {
    #[serde(rename = "action")]
    Action { action: HudActionKind },
}

// ── HudManager ────────────────────────────────────────────────────────────

/// Manages the in-process HUD overlay thread.
pub struct HudManager {
    tx: mpsc::Sender<HudMessage>,
    enabled: bool,
}

impl HudManager {
    /// Start the HUD manager. If enabled (and on Windows), spawns the overlay
    /// thread. Returns the manager handle.
    pub fn start(enabled: bool) -> Self {
        let enabled = enabled && cfg!(target_os = "windows");

        let tx = if enabled {
            #[cfg(target_os = "windows")]
            {
                let sender = crate::hud_overlay::spawn();
                log::info!("[hud_mgr] overlay thread spawned");
                sender
            }
            #[cfg(not(target_os = "windows"))]
            {
                // Unreachable due to the check above, but keeps the compiler happy
                let (sender, _) = mpsc::channel();
                sender
            }
        } else {
            let (sender, _) = mpsc::channel();
            sender
        };

        Self { tx, enabled }
    }

    /// Push a state update to the overlay.
    pub fn push_state(&self, state: HudState) {
        if !self.enabled {
            return;
        }
        log::debug!(
            "[hud_mgr] push_state: mode={:?} crew={} voice_members={}",
            state.mode,
            state.crew.is_some(),
            state.voice.as_ref().map_or(0, |v| v.members.len()),
        );
        if let Err(e) = self.tx.send(HudMessage::State(Box::new(state))) {
            log::error!("[hud_mgr] send failed (overlay thread dead?): {}", e);
        }
    }

    /// Push a settings update to the overlay.
    pub fn push_settings(&self, settings: HudSettings) {
        if !self.enabled {
            return;
        }
        let _ = self.tx.send(HudMessage::Settings(settings));
    }

    /// Poll for user actions from the HUD (currently unused -- overlay is click-through).
    pub fn poll_action(&self) -> Option<HudAction> {
        None
    }

    /// Send shutdown to the overlay thread.
    pub fn shutdown(&self) {
        let _ = self.tx.send(HudMessage::Shutdown);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// Derive initials from a display name (max 2 chars, uppercase).
pub fn derive_initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    match words.len() {
        0 => "??".to_string(),
        1 => {
            let chars: Vec<char> = words[0].chars().collect();
            if chars.len() >= 2 {
                format!("{}{}", chars[0], chars[1]).to_uppercase()
            } else {
                format!("{}", chars[0]).to_uppercase()
            }
        }
        _ => {
            let first = words[0].chars().next().unwrap_or('?');
            let second = words[1].chars().next().unwrap_or('?');
            format!("{}{}", first, second).to_uppercase()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_two_words() {
        assert_eq!(derive_initials("Koji Tech"), "KT");
    }

    #[test]
    fn initials_single_word() {
        assert_eq!(derive_initials("nova"), "NO");
    }

    #[test]
    fn initials_empty() {
        assert_eq!(derive_initials(""), "??");
    }

    #[test]
    fn initials_single_char() {
        assert_eq!(derive_initials("X"), "X");
    }

    #[test]
    fn hud_state_serialization() {
        let state = HudState {
            mode: HudMode::Overlay,
            crew: Some(HudCrew {
                name: "The Vanguard".into(),
                initials: "TV".into(),
                online_count: 5,
                avatar_rgba: None,
            }),
            voice: Some(HudVoice {
                channel_name: "General".into(),
                members: vec![HudVoiceMember {
                    id: "abc".into(),
                    display_name: "k0ji_tech".into(),
                    initials: "KT".into(),
                    avatar_rgba: None,
                    speaking: true,
                    muted: false,
                    is_self: false,
                }],
                self_muted: false,
            }),
            recent_messages: None,
            stream_card: None,
            clip_toast: None,
        };
        let msg = HudMessage::State(Box::new(state));
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"state\""));
        assert!(json.contains("\"mode\":\"overlay\""));
        assert!(json.contains("k0ji_tech"));

        // Roundtrip
        let parsed: HudMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            HudMessage::State(ref s) => {
                assert_eq!(s.mode, HudMode::Overlay);
                assert!(s.crew.is_some());
            }
            _ => panic!("expected State"),
        }
    }

    #[test]
    fn hud_action_serialization() {
        let action = HudAction::Action {
            action: HudActionKind::MuteToggle,
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains("mute_toggle"));

        let parsed: HudAction = serde_json::from_str(&json).unwrap();
        match parsed {
            HudAction::Action { action } => {
                assert_eq!(action, HudActionKind::MuteToggle);
            }
        }
    }
}
