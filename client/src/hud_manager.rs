use std::cell::{Cell, RefCell};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

/// The HUD overlay ships only when this is true. It is off (2026-11-01): it
/// cannot draw over fullscreen games, users report it as a distraction, and it
/// costs 7 - 11 MB of private working set in a voice call, which breaks the
/// 80 MB target. The overlay waits for a redesign (spec features/HUD-OVERLAY.md).
/// While it is off, no overlay thread starts and the settings tab hides the HUD
/// section, whatever the stored `hud_enabled` setting says.
pub const HUD_AVAILABLE: bool = false;

/// How long the overlay thread stays up after the HUD hides. A switch between
/// the game and the client must not rebuild the window and its GPU device each
/// time; a HUD that stays hidden must not keep them (about 7 MB of private
/// working set, spec features/HUD-OVERLAY.md section 2.1).
pub const HIDE_GRACE: Duration = Duration::from_secs(30);

/// Starts an overlay thread and returns the sender of its channel.
type Spawn = Box<dyn Fn() -> mpsc::Sender<HudMessage>>;

/// Manages the in-process HUD overlay thread. The thread, its window and its
/// D3D11 / Direct2D stack exist only while the HUD shows, and for
/// [`HIDE_GRACE`] after it hides.
pub struct HudManager {
    enabled: bool,
    spawn: Spawn,
    overlay: RefCell<Option<mpsc::Sender<HudMessage>>>,
    /// The last settings, sent again to each new overlay thread.
    settings: RefCell<Option<HudSettings>>,
    /// When the HUD hid, while the overlay thread still runs.
    hidden_since: Cell<Option<Instant>>,
}

impl HudManager {
    /// The HUD manager. Nothing starts here: the overlay thread starts with the
    /// first state that shows the HUD. Disabled, or not on Windows, it does nothing.
    pub fn start(enabled: bool) -> Self {
        #[cfg(target_os = "windows")]
        let spawn: Spawn = Box::new(crate::hud_overlay::spawn);
        #[cfg(not(target_os = "windows"))]
        let spawn: Spawn = Box::new(|| mpsc::channel().0);
        Self::with_spawner(enabled && cfg!(target_os = "windows"), spawn)
    }

    fn with_spawner(enabled: bool, spawn: Spawn) -> Self {
        Self {
            enabled,
            spawn,
            overlay: RefCell::new(None),
            settings: RefCell::new(None),
            hidden_since: Cell::new(None),
        }
    }

    /// Push a state update to the overlay. A state that shows the HUD starts the
    /// overlay thread if it does not run.
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
        if state.mode == HudMode::Overlay {
            self.hidden_since.set(None);
            self.ensure_overlay();
        } else if self.overlay.borrow().is_some() && self.hidden_since.get().is_none() {
            self.hidden_since.set(Some(Instant::now()));
        }
        self.send(HudMessage::State(Box::new(state)));
    }

    /// Push a settings update to the overlay. Kept for the next overlay thread.
    pub fn push_settings(&self, settings: HudSettings) {
        if !self.enabled {
            return;
        }
        *self.settings.borrow_mut() = Some(settings.clone());
        self.send(HudMessage::Settings(settings));
    }

    /// Call often (the poll loop does): stops the overlay thread once the HUD has
    /// been hidden for [`HIDE_GRACE`].
    pub fn tick(&self, now: Instant) {
        if let Some(since) = self.hidden_since.get() {
            if now.duration_since(since) >= HIDE_GRACE {
                self.stop_overlay("hidden for 30 s");
            }
        }
    }

    /// Poll for user actions from the HUD (currently unused -- overlay is click-through).
    pub fn poll_action(&self) -> Option<HudAction> {
        None
    }

    /// Send shutdown to the overlay thread.
    pub fn shutdown(&self) {
        self.stop_overlay("client shutdown");
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// True while the overlay thread runs.
    pub fn overlay_running(&self) -> bool {
        self.overlay.borrow().is_some()
    }

    fn ensure_overlay(&self) {
        if self.overlay.borrow().is_some() {
            return;
        }
        let tx = (self.spawn)();
        log::info!("[hud_mgr] overlay thread spawned (HUD shown)");
        if let Some(settings) = self.settings.borrow().clone() {
            let _ = tx.send(HudMessage::Settings(settings));
        }
        *self.overlay.borrow_mut() = Some(tx);
    }

    fn stop_overlay(&self, why: &str) {
        self.hidden_since.set(None);
        if let Some(tx) = self.overlay.borrow_mut().take() {
            let _ = tx.send(HudMessage::Shutdown);
            log::info!("[hud_mgr] overlay thread stopped ({why})");
        }
    }

    /// Sends to the overlay thread if it runs. A dead thread (a panic, a lost
    /// device) is forgotten, so the next state that shows the HUD starts a new one.
    fn send(&self, msg: HudMessage) {
        let failed = match self.overlay.borrow().as_ref() {
            Some(tx) => tx.send(msg).is_err(),
            None => false,
        };
        if failed {
            log::error!("[hud_mgr] overlay thread is gone; the next shown state starts a new one");
            *self.overlay.borrow_mut() = None;
            self.hidden_since.set(None);
        }
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
mod lifetime_tests {
    //! The overlay thread exists only while the HUD shows, and for HIDE_GRACE
    //! after it hides. A fake spawner stands in for the Win32 overlay thread.

    use super::*;
    use std::rc::Rc;

    struct Spawned {
        rx: Vec<mpsc::Receiver<HudMessage>>,
    }

    fn manager() -> (HudManager, Rc<RefCell<Spawned>>) {
        let spawned = Rc::new(RefCell::new(Spawned { rx: Vec::new() }));
        let s = spawned.clone();
        let spawn: Spawn = Box::new(move || {
            let (tx, rx) = mpsc::channel();
            s.borrow_mut().rx.push(rx);
            tx
        });
        (HudManager::with_spawner(true, spawn), spawned)
    }

    fn state(mode: HudMode) -> HudState {
        HudState {
            mode,
            ..Default::default()
        }
    }

    fn settings() -> HudSettings {
        HudSettings {
            overlay_opacity: 0.8,
            show_clip_toasts: true,
        }
    }

    fn drain(rx: &mpsc::Receiver<HudMessage>) -> Vec<&'static str> {
        rx.try_iter()
            .map(|m| match m {
                HudMessage::State(s) if s.mode == HudMode::Overlay => "shown",
                HudMessage::State(_) => "hidden",
                HudMessage::Settings(_) => "settings",
                HudMessage::Shutdown => "shutdown",
            })
            .collect()
    }

    #[test]
    fn nothing_starts_while_the_hud_stays_hidden() {
        let (hud, spawned) = manager();
        hud.push_settings(settings());
        hud.push_state(state(HudMode::Hidden));
        hud.tick(Instant::now() + HIDE_GRACE * 2);
        assert!(!hud.overlay_running());
        assert_eq!(spawned.borrow().rx.len(), 0);
    }

    #[test]
    fn the_first_shown_state_starts_the_overlay_with_the_last_settings() {
        let (hud, spawned) = manager();
        hud.push_settings(settings());
        hud.push_state(state(HudMode::Overlay));
        assert!(hud.overlay_running());
        let spawned = spawned.borrow();
        assert_eq!(spawned.rx.len(), 1);
        assert_eq!(drain(&spawned.rx[0]), ["settings", "shown"]);
    }

    #[test]
    fn a_hidden_hud_stops_the_overlay_after_the_grace_and_not_before() {
        let (hud, spawned) = manager();
        hud.push_state(state(HudMode::Overlay));
        hud.push_state(state(HudMode::Hidden));
        let hidden_at = Instant::now();
        hud.tick(hidden_at + HIDE_GRACE / 2);
        assert!(
            hud.overlay_running(),
            "a short switch away keeps the overlay"
        );
        hud.tick(hidden_at + HIDE_GRACE + Duration::from_secs(1));
        assert!(!hud.overlay_running());
        assert_eq!(
            drain(&spawned.borrow().rx[0]),
            ["shown", "hidden", "shutdown"]
        );
    }

    #[test]
    fn showing_again_within_the_grace_keeps_the_same_overlay() {
        let (hud, spawned) = manager();
        hud.push_state(state(HudMode::Overlay));
        hud.push_state(state(HudMode::Hidden));
        hud.push_state(state(HudMode::Overlay));
        hud.tick(Instant::now() + HIDE_GRACE * 2);
        assert!(hud.overlay_running());
        assert_eq!(spawned.borrow().rx.len(), 1);
    }

    #[test]
    fn showing_after_a_stop_starts_a_new_overlay() {
        let (hud, spawned) = manager();
        hud.push_settings(settings());
        hud.push_state(state(HudMode::Overlay));
        hud.push_state(state(HudMode::Hidden));
        hud.tick(Instant::now() + HIDE_GRACE * 2);
        hud.push_state(state(HudMode::Overlay));
        let spawned = spawned.borrow();
        assert_eq!(spawned.rx.len(), 2);
        assert_eq!(drain(&spawned.rx[1]), ["settings", "shown"]);
    }

    #[test]
    fn a_dead_overlay_thread_is_replaced_by_the_next_shown_state() {
        let (hud, spawned) = manager();
        hud.push_state(state(HudMode::Overlay));
        spawned.borrow_mut().rx.clear(); // the thread died: its receiver is gone
        hud.push_state(state(HudMode::Overlay));
        assert!(!hud.overlay_running(), "the failed send forgets the thread");
        hud.push_state(state(HudMode::Overlay));
        assert!(hud.overlay_running());
        assert_eq!(spawned.borrow().rx.len(), 1);
    }

    #[test]
    fn a_disabled_manager_never_starts_the_overlay() {
        let spawn: Spawn = Box::new(|| panic!("must not spawn"));
        let hud = HudManager::with_spawner(false, spawn);
        hud.push_state(state(HudMode::Overlay));
        assert!(!hud.overlay_running());
    }

    #[test]
    fn shutdown_stops_a_running_overlay() {
        let (hud, spawned) = manager();
        hud.push_state(state(HudMode::Overlay));
        hud.shutdown();
        assert!(!hud.overlay_running());
        assert_eq!(drain(&spawned.borrow().rx[0]), ["shown", "shutdown"]);
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
