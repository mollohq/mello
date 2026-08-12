use serde::{Deserialize, Serialize};

const APP_NAME: &str = "mello";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub capture_device_id: Option<String>,
    pub playback_device_id: Option<String>,
    pub dark_theme: bool,
    pub device_id: Option<String>,
    pub onboarding_step: u8,
    pub last_crew_id: Option<String>,
    pub pending_crew_id: Option<String>,
    pub pending_crew_name: Option<String>,
    pub pending_crew_description: Option<String>,
    pub pending_crew_open: Option<bool>,
    pub start_on_boot: bool,
    pub ptt_key: Option<String>,
    // General tab
    pub start_minimized: bool,
    pub close_to_tray: bool,
    pub auto_connect: bool,
    pub minimize_on_join: bool,
    pub hardware_acceleration: bool,
    // Audio tab
    pub input_volume: f32,
    pub output_volume: f32,
    pub noise_suppression: bool,
    pub echo_cancellation: bool,
    pub input_mode: String, // "voice_activity" or "push_to_talk"
    pub vad_threshold: f32,
    // HUD tab
    pub hud_enabled: bool,
    pub hud_show_overlay_in_game: bool,
    pub hud_overlay_opacity: f32,
    pub hud_show_clip_toasts: bool,
    pub hud_overlay_x: Option<i32>,
    pub hud_overlay_y: Option<i32>,
    pub hud_miniplayer_x: Option<i32>,
    pub hud_miniplayer_y: Option<i32>,
    pub hidden_invite_crew_ids: Vec<String>,
    pub seen_session_ids: Vec<String>,
    // Games tab: integrations the user switched off (consent is default-on;
    // absence from this list means enabled).
    pub disabled_game_integrations: Vec<String>,
    /// User dismissed the post-game "connect Riot account" CTA; don't re-ask.
    pub riot_prompt_dismissed: bool,
    /// User-confirmed games outside the bundled DB (the "track it?" flow).
    pub custom_games: Vec<CustomGameSetting>,
    /// Lowercase exe names the user marked "not a game"; never re-prompted.
    pub unknown_game_dismissed: Vec<String>,
}

/// Persisted form of a user-confirmed custom game (mirrors
/// `mello_core::game_db::CustomGame`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomGameSetting {
    pub id: String,
    pub name: String,
    pub short_name: String,
    pub exe: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            capture_device_id: None,
            playback_device_id: None,
            dark_theme: true,
            device_id: None,
            onboarding_step: 0,
            last_crew_id: None,
            pending_crew_id: None,
            pending_crew_name: None,
            pending_crew_description: None,
            pending_crew_open: None,
            start_on_boot: false,
            ptt_key: None,
            start_minimized: false,
            close_to_tray: true,
            auto_connect: false,
            minimize_on_join: false,
            hardware_acceleration: true,
            input_volume: 1.0,
            output_volume: 1.0,
            noise_suppression: true,
            echo_cancellation: true,
            input_mode: "voice_activity".into(),
            vad_threshold: -40.0,
            hud_enabled: true,
            hud_show_overlay_in_game: true,
            hud_overlay_opacity: 0.8,
            hud_show_clip_toasts: true,
            hud_overlay_x: None,
            hud_overlay_y: None,
            hud_miniplayer_x: None,
            hud_miniplayer_y: None,
            hidden_invite_crew_ids: Vec::new(),
            seen_session_ids: Vec::new(),
            disabled_game_integrations: Vec::new(),
            riot_prompt_dismissed: false,
            custom_games: Vec::new(),
            unknown_game_dismissed: Vec::new(),
        }
    }
}

/// Environment variable redirecting settings storage to a specific directory.
///
/// Onboarding calls [`Settings::save`] on nearly every step transition, so any
/// test that drives onboarding would otherwise rewrite the developer's (or CI
/// runner's) real config. Tests point this at a temp dir.
pub const CONFIG_DIR_ENV: &str = "MELLO_CONFIG_DIR";

impl Settings {
    /// Settings file path for a given config-dir override value.
    ///
    /// Split from the environment lookup so the path composition is testable
    /// without mutating process-global env state.
    fn override_path(dir: Option<String>) -> Option<std::path::PathBuf> {
        dir.filter(|v| !v.is_empty())
            .map(|dir| std::path::PathBuf::from(dir).join(format!("{APP_NAME}.toml")))
    }

    /// `None` means "use confy's platform default location".
    fn configured_path() -> Option<std::path::PathBuf> {
        Self::override_path(std::env::var(CONFIG_DIR_ENV).ok())
    }

    pub fn load() -> Self {
        let loaded = match Self::configured_path() {
            Some(path) => confy::load_path::<Settings>(&path),
            None => confy::load::<Settings>(APP_NAME, None),
        };

        match loaded {
            Ok(s) => {
                log::info!("Settings loaded");
                s
            }
            Err(e) => {
                log::warn!("Failed to load settings, using defaults: {}", e);
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        let stored = match Self::configured_path() {
            Some(path) => confy::store_path(&path, self),
            None => confy::store(APP_NAME, None, self),
        };

        if let Err(e) = stored {
            log::warn!("Failed to save settings: {}", e);
        }
    }

    /// The stable device identity for Nakama device auth, creating and
    /// persisting one on first use.
    ///
    /// This must be stable across attempts and across restarts. Onboarding used
    /// to mint a fresh random id inside every `FinalizeOnboarding`, and
    /// `device_id` was never written, so each attempt authenticated as a *new*
    /// device and Nakama duly created a *new* account (`create=true`). Any
    /// retry — or any restart before onboarding was marked complete — left
    /// another orphan user behind. Production users accumulated five or six
    /// accounts each this way.
    ///
    /// Reusing the id makes finalize idempotent: a second attempt authenticates
    /// back into the same account instead of creating another.
    pub fn device_id_or_create(&mut self) -> String {
        if let Some(existing) = self.device_id.as_ref().filter(|id| !id.is_empty()) {
            return existing.clone();
        }

        use rand::Rng;
        let bytes: [u8; 16] = rand::thread_rng().gen();
        let id: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();

        self.device_id = Some(id.clone());
        self.save();
        log::info!("[auth] generated device id for this install");
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_values() {
        let s = Settings::default();
        assert!(s.capture_device_id.is_none());
        assert!(s.playback_device_id.is_none());
        assert!(s.dark_theme);
    }

    #[test]
    fn config_dir_override_composes_settings_path() {
        let path = Settings::override_path(Some("/tmp/mello-test".into()))
            .expect("an override dir should yield a path");
        assert_eq!(path, std::path::PathBuf::from("/tmp/mello-test/mello.toml"));
    }

    /// Unset or empty must fall through to confy's platform default rather than
    /// resolving to a bare relative filename in the process's cwd.
    #[test]
    fn absent_or_empty_config_dir_uses_platform_default() {
        assert!(Settings::override_path(None).is_none());
        assert!(Settings::override_path(Some(String::new())).is_none());
    }

    /// Settings must survive a real write/read cycle through the override path.
    /// Onboarding saves on nearly every step transition, so this is the
    /// mechanism that keeps tests off the developer's real config file.
    #[test]
    fn settings_roundtrip_through_override_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = Settings::override_path(Some(dir.path().to_string_lossy().into_owned()))
            .expect("override path");

        let saved = Settings {
            onboarding_step: 3,
            pending_crew_id: Some("crew-42".into()),
            dark_theme: false,
            ..Default::default()
        };
        confy::store_path(&path, &saved).expect("store");

        assert!(
            path.exists(),
            "settings file should exist at the override path"
        );

        let loaded: Settings = confy::load_path(&path).expect("load");
        assert_eq!(loaded.onboarding_step, 3);
        assert_eq!(loaded.pending_crew_id.as_deref(), Some("crew-42"));
        assert!(!loaded.dark_theme);
    }

    #[test]
    fn settings_toml_roundtrip() {
        let s = Settings {
            capture_device_id: Some("mic_123".into()),
            playback_device_id: Some("spk_456".into()),
            dark_theme: false,
            device_id: Some("dev-abc".into()),
            onboarding_step: 4,
            last_crew_id: None,
            ..Default::default()
        };
        let toml_str = toml::to_string(&s).unwrap();
        let decoded: Settings = toml::from_str(&toml_str).unwrap();
        assert_eq!(decoded.capture_device_id.as_deref(), Some("mic_123"));
        assert_eq!(decoded.playback_device_id.as_deref(), Some("spk_456"));
        assert!(!decoded.dark_theme);
    }

    #[test]
    fn settings_missing_fields_use_defaults() {
        let partial = r#"dark_theme = false"#;
        let decoded: Settings = toml::from_str(partial).unwrap();
        assert!(decoded.capture_device_id.is_none());
        assert!(decoded.playback_device_id.is_none());
        assert!(!decoded.dark_theme);
    }

    #[test]
    fn settings_clear_stale_device_roundtrip() {
        let mut s = Settings {
            capture_device_id: Some("{0.0.1.00000000}.{stale-guid}".into()),
            playback_device_id: Some("{0.0.0.00000000}.{stale-guid}".into()),
            ..Default::default()
        };
        assert!(s.capture_device_id.is_some());
        assert!(s.playback_device_id.is_some());

        // Simulate what the AudioDeviceFallback handler does
        s.capture_device_id = None;
        s.playback_device_id = None;

        let toml_str = toml::to_string(&s).unwrap();
        let decoded: Settings = toml::from_str(&toml_str).unwrap();
        assert!(
            decoded.capture_device_id.is_none(),
            "cleared capture device should not persist"
        );
        assert!(
            decoded.playback_device_id.is_none(),
            "cleared playback device should not persist"
        );
    }
}
