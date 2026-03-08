use serde::{Deserialize, Serialize};

const APP_NAME: &str = "mello";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub capture_device_id: Option<String>,
    pub playback_device_id: Option<String>,
    pub dark_theme: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            capture_device_id: None,
            playback_device_id: None,
            dark_theme: true,
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
