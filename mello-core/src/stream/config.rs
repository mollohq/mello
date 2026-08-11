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
                    Codec::H264 => 5_000,
                    Codec::Av1 => 2_500,
                },
                fec_n: 4,
            },
            Self::Low => PresetParams {
                width: 1280,
                height: 720,
                fps: 30,
                bitrate_kbps: match codec {
                    Codec::H264 => 3_000,
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
    /// What is actually being captured, e.g. `process pid=1234`.
    ///
    /// Carried so host telemetry can state it. A stream that captures the wrong
    /// thing looks identical to a stream that captures nothing, and without this
    /// the difference is unrecoverable after the fact.
    pub capture_desc: String,
}

/// A validated capture target.
///
/// Exists so an unrecognised mode, or a mode missing its identifier, cannot be
/// silently reinterpreted as something else. The previous dispatch fell through
/// to monitor capture of display 0 for *any* unmatched input, which turns a
/// selection bug into a black stream with no error anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTarget {
    Monitor { index: u32 },
    Window { hwnd: u64 },
    Process { pid: u32 },
}

impl CaptureTarget {
    /// Resolve the UI's `(mode, ids)` tuple, rejecting anything ambiguous.
    pub fn resolve(
        mode: &str,
        monitor_index: Option<u32>,
        hwnd: Option<u64>,
        pid: Option<u32>,
    ) -> Result<Self, String> {
        match mode {
            // "game" is what the picker labels a game entry with; both spellings
            // reach here from different call paths and mean the same thing.
            "process" | "game" => match pid {
                Some(pid) if pid != 0 => Ok(Self::Process { pid }),
                _ => Err(format!(
                    "capture mode '{mode}' selected without a process id"
                )),
            },
            "window" => match hwnd {
                Some(hwnd) if hwnd != 0 => Ok(Self::Window { hwnd }),
                _ => Err("capture mode 'window' selected without a window handle".to_string()),
            },
            "monitor" => Ok(Self::Monitor {
                index: monitor_index.unwrap_or(0),
            }),
            other => Err(format!("unknown capture mode '{other}'")),
        }
    }

    /// Short human-readable form for logs and telemetry.
    pub fn describe(&self) -> String {
        match self {
            Self::Monitor { index } => format!("monitor index={index}"),
            Self::Window { hwnd } => format!("window hwnd=0x{hwnd:x}"),
            Self::Process { pid } => format!("process pid={pid}"),
        }
    }

    /// Stable token for telemetry grouping.
    pub fn mode_label(&self) -> &'static str {
        match self {
            Self::Monitor { .. } => "monitor",
            Self::Window { .. } => "window",
            Self::Process { .. } => "process",
        }
    }
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
            capture_desc: String::new(),
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

    /// The regression this type exists for. A 2026-08-11 field test streamed a
    /// black screen for three minutes: the host was on monitor capture of
    /// display 0 while the streamer believed they had picked a game. Any
    /// unmatched mode used to fall through to exactly that, silently.
    #[test]
    fn unknown_mode_is_rejected_rather_than_becoming_display_zero() {
        for mode in ["", "screen", "Process", "display", "game-1"] {
            let resolved = CaptureTarget::resolve(mode, Some(0), Some(42), Some(99));
            assert!(
                resolved.is_err(),
                "mode {mode:?} resolved to {resolved:?} instead of erroring"
            );
        }
    }

    /// Both spellings reach this code from different call paths and mean the
    /// same thing; treating one as unknown would send it to monitor capture.
    #[test]
    fn game_and_process_are_the_same_target() {
        let a = CaptureTarget::resolve("process", None, None, Some(1234));
        let b = CaptureTarget::resolve("game", None, None, Some(1234));
        assert_eq!(a, Ok(CaptureTarget::Process { pid: 1234 }));
        assert_eq!(b, Ok(CaptureTarget::Process { pid: 1234 }));
    }

    /// A missing identifier is as wrong as a missing mode: `pid.unwrap_or(0)`
    /// used to hand libmello process id 0 and let it fail downstream.
    #[test]
    fn a_target_without_its_identifier_is_rejected() {
        assert!(CaptureTarget::resolve("process", None, None, None).is_err());
        assert!(CaptureTarget::resolve("process", None, None, Some(0)).is_err());
        assert!(CaptureTarget::resolve("window", None, None, None).is_err());
        assert!(CaptureTarget::resolve("window", None, Some(0), None).is_err());
    }

    /// Monitor index is the one identifier with a meaningful default: display 0
    /// is the primary, and the picker always supplies it anyway.
    #[test]
    fn monitor_defaults_to_the_primary_display() {
        assert_eq!(
            CaptureTarget::resolve("monitor", None, None, None),
            Ok(CaptureTarget::Monitor { index: 0 })
        );
        assert_eq!(
            CaptureTarget::resolve("monitor", Some(2), None, None),
            Ok(CaptureTarget::Monitor { index: 2 })
        );
    }

    #[test]
    fn descriptions_identify_the_target_in_logs() {
        assert_eq!(
            CaptureTarget::Process { pid: 1234 }.describe(),
            "process pid=1234"
        );
        assert_eq!(
            CaptureTarget::Monitor { index: 1 }.describe(),
            "monitor index=1"
        );
        assert_eq!(
            CaptureTarget::Window { hwnd: 0x90aea }.describe(),
            "window hwnd=0x90aea"
        );
    }

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
