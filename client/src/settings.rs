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
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        match confy::load::<Settings>(APP_NAME, None) {
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
        if let Err(e) = confy::store(APP_NAME, None, self) {
            log::warn!("Failed to save settings: {}", e);
        }
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
}
