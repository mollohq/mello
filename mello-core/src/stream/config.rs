use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    #[default]
    H264,
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityPreset {
    Ultra,
    High,
    Medium,
    Low,
    Potato,
}

impl QualityPreset {
    pub fn params(&self, codec: Codec) -> PresetParams {
        match self {
            Self::Ultra => PresetParams {
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_kbps: match codec {
                    Codec::H264 => 8_000,
                    Codec::Av1 => 5_000,
                },
                fec_n: 5,
            },
            Self::High => PresetParams {
                width: 1920,
                height: 1080,
                fps: 30,
                bitrate_kbps: match codec {
                    Codec::H264 => 4_500,
                    Codec::Av1 => 3_000,
                },
                fec_n: 5,
            },
            Self::Medium => PresetParams {
                width: 1280,
                height: 720,
                fps: 60,
                bitrate_kbps: match codec {
                    Codec::H264 => 4_000,
                    Codec::Av1 => 2_500,
                },
                fec_n: 4,
            },
            Self::Low => PresetParams {
                width: 1280,
                height: 720,
                fps: 30,
                bitrate_kbps: match codec {
                    Codec::H264 => 2_500,
                    Codec::Av1 => 1_500,
                },
                fec_n: 3,
            },
            Self::Potato => PresetParams {
                width: 854,
                height: 480,
                fps: 30,
                bitrate_kbps: 1_500,
                fec_n: 3,
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PresetParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub fec_n: usize,
}

#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: Codec,
    pub preset: QualityPreset,
    pub fec_n: usize,
}

impl StreamConfig {
    pub fn from_preset(preset: QualityPreset, codec: Codec) -> Self {
        let p = preset.params(codec);
        Self {
            width: p.width,
            height: p.height,
            fps: p.fps,
            bitrate_kbps: p.bitrate_kbps,
            codec,
            preset,
            fec_n: p.fec_n,
        }
    }

    /// Minimum bitrate floor (Potato preset).
    pub fn min_bitrate_kbps(codec: Codec) -> u32 {
        QualityPreset::Potato.params(codec).bitrate_kbps
    }
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self::from_preset(QualityPreset::Medium, Codec::H264)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_params_match_spec() {
        let ultra = QualityPreset::Ultra.params(Codec::H264);
        assert_eq!(ultra.bitrate_kbps, 8_000);
        assert_eq!(ultra.fps, 60);
        assert_eq!(ultra.fec_n, 5);

        let potato = QualityPreset::Potato.params(Codec::H264);
        assert_eq!(potato.bitrate_kbps, 1_500);
        assert_eq!(potato.fec_n, 3);
        assert_eq!(potato.width, 854);
    }

    #[test]
    fn av1_lower_bitrate() {
        let h264 = QualityPreset::High.params(Codec::H264);
        let av1 = QualityPreset::High.params(Codec::Av1);
        assert!(av1.bitrate_kbps < h264.bitrate_kbps);
    }

    #[test]
    fn default_config_is_medium_h264() {
        let cfg = StreamConfig::default();
        assert_eq!(cfg.preset, QualityPreset::Medium);
        assert_eq!(cfg.codec, Codec::H264);
    }
}
