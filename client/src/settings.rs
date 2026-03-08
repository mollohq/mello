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
