use std::collections::HashMap;
use std::time::Instant;

use super::config::StreamConfig;
use super::packet::LossReport;

/// Threshold above which we step down bitrate.
const LOSS_STEP_DOWN_THRESHOLD: f32 = 0.05; // 5%
/// Threshold below which we consider the link healthy.
const LOSS_STEP_UP_THRESHOLD: f32 = 0.01; // 1%
/// How long all viewers must report low loss before stepping up.
const STEP_UP_DURATION_SECS: u64 = 10;
/// Bitrate reduction factor on step-down.
const STEP_DOWN_FACTOR: f32 = 0.75; // reduce by 25%
/// Bitrate increase factor on step-up.
const STEP_UP_FACTOR: f32 = 1.10; // increase by 10%

/// Per-viewer loss tracking state.
struct ViewerLossState {
    last_report: Instant,
    /// Timestamp since which the viewer has been continuously healthy.
    healthy_since: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct BitrateChange {
    pub new_bitrate_kbps: u32,
    pub reason: BitrateChangeReason,
}

#[derive(Debug, Clone)]
pub enum BitrateChangeReason {
    StepDown { viewer_id: String, loss_pct: f32 },
    StepUp,
}

/// Adaptive Bitrate controller. Host-driven, monitors per-viewer loss reports.
pub struct AbrController {
    current_bitrate_kbps: u32,
    min_bitrate_kbps: u32,
    max_bitrate_kbps: u32,
    viewers: HashMap<String, ViewerLossState>,
}

impl AbrController {
    pub fn new(config: &StreamConfig) -> Self {
        Self {
            current_bitrate_kbps: config.bitrate_kbps,
            min_bitrate_kbps: StreamConfig::min_bitrate_kbps(config.codec),
            max_bitrate_kbps: config.bitrate_kbps,
            viewers: HashMap::new(),
        }
    }

    pub fn current_bitrate_kbps(&self) -> u32 {
        self.current_bitrate_kbps
    }

    pub fn on_viewer_joined(&mut self, viewer_id: &str) {
        self.viewers.insert(
            viewer_id.to_string(),
            ViewerLossState {
                last_report: Instant::now(),
                healthy_since: Some(Instant::now()),
            },
        );
    }

    pub fn on_viewer_left(&mut self, viewer_id: &str) {
        self.viewers.remove(viewer_id);
    }

    /// Process a loss report from a viewer. Returns a bitrate change if
    /// the ABR rules trigger an adjustment.
    pub fn process_loss_report(
        &mut self,
        viewer_id: &str,
        report: &LossReport,
    ) -> Option<BitrateChange> {
        let now = Instant::now();
        let loss = report.loss_ratio();

        let state = self.viewers.entry(viewer_id.to_string()).or_insert(ViewerLossState {
            last_report: now,
            healthy_since: Some(now),
        });
        state.last_report = now;

        if loss > LOSS_STEP_DOWN_THRESHOLD {
            state.healthy_since = None;
            return self.step_down(viewer_id, loss);
        }

        if loss < LOSS_STEP_UP_THRESHOLD {
            if state.healthy_since.is_none() {
                state.healthy_since = Some(now);
            }
        } else {
            state.healthy_since = None;
        }

        self.try_step_up(now)
    }

    fn step_down(&mut self, viewer_id: &str, loss_pct: f32) -> Option<BitrateChange> {
        let new_bitrate = (self.current_bitrate_kbps as f32 * STEP_DOWN_FACTOR) as u32;
        let new_bitrate = new_bitrate.max(self.min_bitrate_kbps);

        if new_bitrate >= self.current_bitrate_kbps {
            return None; // already at floor
        }

        log::warn!(
            "ABR step-down: viewer {} loss {:.1}%, bitrate {} -> {} kbps",
            viewer_id,
            loss_pct * 100.0,
            self.current_bitrate_kbps,
            new_bitrate
        );

        self.current_bitrate_kbps = new_bitrate;
        Some(BitrateChange {
            new_bitrate_kbps: new_bitrate,
            reason: BitrateChangeReason::StepDown {
                viewer_id: viewer_id.to_string(),
                loss_pct,
            },
        })
    }

    fn try_step_up(&mut self, now: Instant) -> Option<BitrateChange> {
        if self.current_bitrate_kbps >= self.max_bitrate_kbps {
            return None;
        }

        let all_healthy = self.viewers.values().all(|v| {
            v.healthy_since
                .map(|since| now.duration_since(since).as_secs() >= STEP_UP_DURATION_SECS)
                .unwrap_or(false)
        });

        if !all_healthy || self.viewers.is_empty() {
            return None;
        }

        let new_bitrate = (self.current_bitrate_kbps as f32 * STEP_UP_FACTOR) as u32;
        let new_bitrate = new_bitrate.min(self.max_bitrate_kbps);

        if new_bitrate <= self.current_bitrate_kbps {
            return None;
        }

        log::info!(
            "ABR step-up: bitrate {} -> {} kbps",
            self.current_bitrate_kbps,
            new_bitrate
        );

        self.current_bitrate_kbps = new_bitrate;

        // Reset healthy_since to require another full window before next step-up
        for state in self.viewers.values_mut() {
            state.healthy_since = Some(now);
        }

        Some(BitrateChange {
            new_bitrate_kbps: new_bitrate,
            reason: BitrateChangeReason::StepUp,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::config::{Codec, QualityPreset};

    fn default_config() -> StreamConfig {
        StreamConfig::from_preset(QualityPreset::High, Codec::H264)
    }

    #[test]
    fn step_down_on_high_loss() {
        let mut abr = AbrController::new(&default_config());
        abr.on_viewer_joined("v1");

        let report = LossReport {
            packets_received: 90,
            packets_lost: 10,
        };
        let change = abr.process_loss_report("v1", &report);
        assert!(change.is_some());
        let c = change.unwrap();
        assert!(c.new_bitrate_kbps < 12_000);
        assert_eq!(c.new_bitrate_kbps, 9_000); // 12000 * 0.75
    }

    #[test]
    fn no_step_down_below_floor() {
        let mut abr = AbrController::new(&default_config());
        abr.current_bitrate_kbps = 2_000; // already at potato floor
        abr.on_viewer_joined("v1");

        let report = LossReport {
            packets_received: 80,
            packets_lost: 20,
        };
        let change = abr.process_loss_report("v1", &report);
        assert!(change.is_none());
    }

    #[test]
    fn no_step_up_without_duration() {
        let mut abr = AbrController::new(&default_config());
        abr.current_bitrate_kbps = 8_000; // below max
        abr.on_viewer_joined("v1");

        let report = LossReport {
            packets_received: 1000,
            packets_lost: 0,
        };
        // Immediately after joining — not enough healthy duration
        let change = abr.process_loss_report("v1", &report);
        assert!(change.is_none());
    }
}
