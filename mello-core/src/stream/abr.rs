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
/// Hold-off after a bitrate step-down before any step-up is allowed.
const STEP_UP_HOLDOFF_AFTER_DOWN_SECS: u64 = 6;
/// Bitrate reduction factor on step-down.
const STEP_DOWN_FACTOR: f32 = 0.75; // reduce by 25%
/// Bitrate increase factor on step-up.
const STEP_UP_FACTOR: f32 = 1.10; // increase by 10%
/// Minimum interval between two bitrate step-downs.
const STEP_DOWN_COOLDOWN_MS: u64 = 750;
/// Ignore viewers whose loss reports are too old.
const REPORT_STALE_SECS: u64 = 3;
/// EWMA alpha for smoothing per-viewer loss (0..1).
const LOSS_EWMA_ALPHA: f32 = 0.35;
/// EWMA alpha for smoothing per-viewer observed receive throughput (kbps).
const RX_KBPS_EWMA_ALPHA: f32 = 0.40;
/// Ignore tiny throughput samples (noise / idle windows).
const RX_KBPS_MIN_VALID: u16 = 250;
/// Safety margin for estimated viewer receive throughput.
const RX_KBPS_HEADROOM_FACTOR: f32 = 0.85;
/// Require meaningful delta before forcing a bandwidth clamp step-down.
const RX_KBPS_CLAMP_MIN_DELTA: u32 = 250;

/// Per-viewer loss tracking state.
struct ViewerLossState {
    last_report: Instant,
    smoothed_loss: f32,
    smoothed_rx_kbps: Option<f32>,
    /// Timestamp since which the viewer has been continuously healthy.
    healthy_since: Option<Instant>,
}

/// FEC group sizes for different loss conditions.
const FEC_N_HEALTHY: usize = 0; // no FEC when loss < 1%
const FEC_N_MODERATE: usize = 10; // 10% overhead when 1-5% loss
const FEC_N_HIGH: usize = 5; // 20% overhead when > 5% loss
const FEC_MODERATE_ENTER_THRESHOLD: f32 = 0.015; // enter moderate >= 1.5%
const FEC_MODERATE_EXIT_THRESHOLD: f32 = 0.008; // leave moderate <= 0.8%
const FEC_HIGH_ENTER_THRESHOLD: f32 = 0.060; // enter high >= 6%
const FEC_HIGH_EXIT_THRESHOLD: f32 = 0.035; // leave high <= 3.5%

#[derive(Debug, Clone)]
pub struct AbrChange {
    pub new_bitrate_kbps: Option<u32>,
    pub new_fec_n: Option<usize>,
    pub reason: BitrateChangeReason,
}

#[derive(Debug, Clone)]
pub enum BitrateChangeReason {
    StepDown {
        viewer_id: String,
        loss_pct: f32,
    },
    BandwidthClamp {
        viewer_id: String,
        estimated_rx_kbps: u32,
        target_kbps: u32,
    },
    StepUp,
    FecOnly,
}

/// Adaptive Bitrate controller. Host-driven, monitors per-viewer loss reports.
pub struct AbrController {
    current_bitrate_kbps: u32,
    min_bitrate_kbps: u32,
    max_bitrate_kbps: u32,
    current_fec_n: usize,
    viewers: HashMap<String, ViewerLossState>,
    last_step_down: Option<Instant>,
}

impl AbrController {
    pub fn new(config: &StreamConfig) -> Self {
        Self {
            current_bitrate_kbps: config.bitrate_kbps,
            min_bitrate_kbps: StreamConfig::min_bitrate_kbps(config.codec),
            max_bitrate_kbps: config.bitrate_kbps,
            current_fec_n: 0,
            viewers: HashMap::new(),
            last_step_down: None,
        }
    }

    pub fn current_bitrate_kbps(&self) -> u32 {
        self.current_bitrate_kbps
    }

    pub fn current_fec_n(&self) -> usize {
        self.current_fec_n
    }

    pub fn on_viewer_joined(&mut self, viewer_id: &str) {
        self.viewers.insert(
            viewer_id.to_string(),
            ViewerLossState {
                last_report: Instant::now(),
                smoothed_loss: 0.0,
                smoothed_rx_kbps: None,
                healthy_since: Some(Instant::now()),
            },
        );
    }

    pub fn on_viewer_left(&mut self, viewer_id: &str) {
        self.viewers.remove(viewer_id);
    }

    /// Process a loss report from a viewer. Returns changes to apply if the
    /// ABR rules trigger a bitrate or FEC adjustment.
    pub fn process_loss_report(
        &mut self,
        viewer_id: &str,
        report: &LossReport,
    ) -> Option<AbrChange> {
        let now = Instant::now();
        let loss = report.loss_ratio();

        // Update per-viewer state
        let smoothed_loss = {
            let state = self
                .viewers
                .entry(viewer_id.to_string())
                .or_insert(ViewerLossState {
                    last_report: now,
                    smoothed_loss: loss,
                    smoothed_rx_kbps: None,
                    healthy_since: Some(now),
                });
            state.last_report = now;
            if state.smoothed_loss == 0.0 {
                state.smoothed_loss = loss;
            } else {
                state.smoothed_loss =
                    LOSS_EWMA_ALPHA * loss + (1.0 - LOSS_EWMA_ALPHA) * state.smoothed_loss;
            }

            if state.smoothed_loss > LOSS_STEP_DOWN_THRESHOLD {
                state.healthy_since = None;
            } else if state.smoothed_loss < LOSS_STEP_UP_THRESHOLD {
                if state.healthy_since.is_none() {
                    state.healthy_since = Some(now);
                }
            } else {
                state.healthy_since = None;
            }

            if let Some(rx_kbps) = report.observed_rx_kbps {
                if rx_kbps >= RX_KBPS_MIN_VALID {
                    let rx_kbps = rx_kbps as f32;
                    state.smoothed_rx_kbps = Some(match state.smoothed_rx_kbps {
                        Some(prev) => {
                            RX_KBPS_EWMA_ALPHA * rx_kbps + (1.0 - RX_KBPS_EWMA_ALPHA) * prev
                        }
                        None => rx_kbps,
                    });
                }
            }

            state.smoothed_loss
        };

        // Adaptive FEC: drive parity from worst recent viewer leg (smoothed).
        let worst_recent_loss = self.worst_recent_loss(now).unwrap_or(smoothed_loss);
        let fec_change = self.update_fec(worst_recent_loss);

        if smoothed_loss > LOSS_STEP_DOWN_THRESHOLD {
            let mut change = self.step_down(viewer_id, smoothed_loss, now);
            if change.is_none() {
                change = self.step_down_bandwidth_clamp(now);
            }
            if let Some(ref mut c) = change {
                if fec_change.is_some() {
                    c.new_fec_n = fec_change;
                }
            } else if let Some(new_fec) = fec_change {
                return Some(AbrChange {
                    new_bitrate_kbps: None,
                    new_fec_n: Some(new_fec),
                    reason: BitrateChangeReason::FecOnly,
                });
            }
            return change;
        }

        let bitrate_change = self
            .step_down_bandwidth_clamp(now)
            .or_else(|| self.try_step_up(now));
        if bitrate_change.is_some() || fec_change.is_some() {
            let mut change = bitrate_change.unwrap_or(AbrChange {
                new_bitrate_kbps: None,
                new_fec_n: None,
                reason: BitrateChangeReason::FecOnly,
            });
            if fec_change.is_some() {
                change.new_fec_n = fec_change;
            }
            Some(change)
        } else {
            None
        }
    }

    fn worst_recent_loss(&self, now: Instant) -> Option<f32> {
        self.viewers
            .values()
            .filter(|v| now.duration_since(v.last_report).as_secs() <= REPORT_STALE_SECS)
            .map(|v| v.smoothed_loss)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }

    fn most_constrained_recent_rx_kbps(&self, now: Instant) -> Option<(String, f32)> {
        self.viewers
            .iter()
            .filter(|(_, v)| now.duration_since(v.last_report).as_secs() <= REPORT_STALE_SECS)
            .filter_map(|(viewer_id, v)| {
                v.smoothed_rx_kbps
                    .filter(|kbps| *kbps >= RX_KBPS_MIN_VALID as f32)
                    .map(|kbps| (viewer_id.clone(), kbps))
            })
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    }

    fn target_from_rx_kbps(&self, rx_kbps: f32) -> u32 {
        let target = (rx_kbps * RX_KBPS_HEADROOM_FACTOR).round() as u32;
        target.max(self.min_bitrate_kbps).min(self.max_bitrate_kbps)
    }

    fn step_down_bandwidth_clamp(&mut self, now: Instant) -> Option<AbrChange> {
        let (viewer_id, constrained_rx_kbps) = self.most_constrained_recent_rx_kbps(now)?;
        let target = self.target_from_rx_kbps(constrained_rx_kbps);

        if self.current_bitrate_kbps <= target + RX_KBPS_CLAMP_MIN_DELTA {
            return None;
        }
        if let Some(last) = self.last_step_down {
            if now.duration_since(last).as_millis() < STEP_DOWN_COOLDOWN_MS as u128 {
                return None;
            }
        }

        log::warn!(
            "ABR bandwidth clamp: viewer {} rx_est={:.0} kbps, bitrate {} -> {} kbps",
            viewer_id,
            constrained_rx_kbps,
            self.current_bitrate_kbps,
            target
        );

        self.current_bitrate_kbps = target;
        self.last_step_down = Some(now);
        Some(AbrChange {
            new_bitrate_kbps: Some(target),
            new_fec_n: None,
            reason: BitrateChangeReason::BandwidthClamp {
                viewer_id,
                estimated_rx_kbps: constrained_rx_kbps.round() as u32,
                target_kbps: target,
            },
        })
    }

    fn update_fec(&mut self, loss: f32) -> Option<usize> {
        let target = match self.current_fec_n {
            FEC_N_HEALTHY => {
                if loss >= FEC_HIGH_ENTER_THRESHOLD {
                    FEC_N_HIGH
                } else if loss >= FEC_MODERATE_ENTER_THRESHOLD {
                    FEC_N_MODERATE
                } else {
                    FEC_N_HEALTHY
                }
            }
            FEC_N_MODERATE => {
                if loss >= FEC_HIGH_ENTER_THRESHOLD {
                    FEC_N_HIGH
                } else if loss <= FEC_MODERATE_EXIT_THRESHOLD {
                    FEC_N_HEALTHY
                } else {
                    FEC_N_MODERATE
                }
            }
            FEC_N_HIGH => {
                if loss <= FEC_MODERATE_EXIT_THRESHOLD {
                    FEC_N_HEALTHY
                } else if loss <= FEC_HIGH_EXIT_THRESHOLD {
                    FEC_N_MODERATE
                } else {
                    FEC_N_HIGH
                }
            }
            _ => {
                if loss >= FEC_HIGH_ENTER_THRESHOLD {
                    FEC_N_HIGH
                } else if loss >= FEC_MODERATE_ENTER_THRESHOLD {
                    FEC_N_MODERATE
                } else {
                    FEC_N_HEALTHY
                }
            }
        };

        if target != self.current_fec_n {
            log::info!(
                "ABR FEC: fec_n {} -> {} (worst_loss={:.1}%)",
                self.current_fec_n,
                target,
                loss * 100.0
            );
            self.current_fec_n = target;
            Some(target)
        } else {
            None
        }
    }

    fn step_down(&mut self, viewer_id: &str, loss_pct: f32, now: Instant) -> Option<AbrChange> {
        if let Some(last) = self.last_step_down {
            if now.duration_since(last).as_millis() < STEP_DOWN_COOLDOWN_MS as u128 {
                return None;
            }
        }

        let new_bitrate = (self.current_bitrate_kbps as f32 * STEP_DOWN_FACTOR) as u32;
        let new_bitrate = new_bitrate.max(self.min_bitrate_kbps);

        if new_bitrate >= self.current_bitrate_kbps {
            return None;
        }

        log::warn!(
            "ABR step-down: viewer {} loss {:.1}%, bitrate {} -> {} kbps",
            viewer_id,
            loss_pct * 100.0,
            self.current_bitrate_kbps,
            new_bitrate
        );

        self.current_bitrate_kbps = new_bitrate;
        self.last_step_down = Some(now);
        Some(AbrChange {
            new_bitrate_kbps: Some(new_bitrate),
            new_fec_n: None,
            reason: BitrateChangeReason::StepDown {
                viewer_id: viewer_id.to_string(),
                loss_pct,
            },
        })
    }

    fn try_step_up(&mut self, now: Instant) -> Option<AbrChange> {
        let mut step_up_cap = self.max_bitrate_kbps;
        if let Some((_, constrained_rx_kbps)) = self.most_constrained_recent_rx_kbps(now) {
            step_up_cap = step_up_cap.min(self.target_from_rx_kbps(constrained_rx_kbps));
        }

        if self.current_bitrate_kbps >= step_up_cap {
            return None;
        }

        if let Some(last_down) = self.last_step_down {
            if now.duration_since(last_down).as_secs() < STEP_UP_HOLDOFF_AFTER_DOWN_SECS {
                return None;
            }
        }

        let all_healthy = self.viewers.values().all(|v| {
            now.duration_since(v.last_report).as_secs() <= REPORT_STALE_SECS
                && v.healthy_since
                    .map(|since| now.duration_since(since).as_secs() >= STEP_UP_DURATION_SECS)
                    .unwrap_or(false)
        });

        if !all_healthy || self.viewers.is_empty() {
            return None;
        }

        let new_bitrate = (self.current_bitrate_kbps as f32 * STEP_UP_FACTOR) as u32;
        let new_bitrate = new_bitrate.min(step_up_cap);

        if new_bitrate <= self.current_bitrate_kbps {
            return None;
        }

        log::info!(
            "ABR step-up: bitrate {} -> {} kbps",
            self.current_bitrate_kbps,
            new_bitrate
        );

        self.current_bitrate_kbps = new_bitrate;

        for state in self.viewers.values_mut() {
            state.healthy_since = Some(now);
        }

        Some(AbrChange {
            new_bitrate_kbps: Some(new_bitrate),
            new_fec_n: None,
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
        let before_bitrate = abr.current_bitrate_kbps();

        let report = LossReport {
            packets_received: 90,
            packets_lost: 10,
            observed_rx_kbps: None,
        };
        let change = abr.process_loss_report("v1", &report);
        assert!(change.is_some());
        let c = change.unwrap();
        let expected = (before_bitrate as f32 * STEP_DOWN_FACTOR) as u32;
        assert_eq!(c.new_bitrate_kbps, Some(expected));
        assert_eq!(c.new_fec_n, Some(FEC_N_HIGH));
    }

    #[test]
    fn no_step_down_below_floor() {
        let mut abr = AbrController::new(&default_config());
        abr.current_bitrate_kbps = 1_500; // already at potato floor
        abr.on_viewer_joined("v1");

        let report = LossReport {
            packets_received: 80,
            packets_lost: 20,
            observed_rx_kbps: None,
        };
        let change = abr.process_loss_report("v1", &report);
        // Bitrate can't go lower, but FEC should still activate
        assert!(change.is_some());
        assert!(change.as_ref().unwrap().new_bitrate_kbps.is_none());
        assert_eq!(change.unwrap().new_fec_n, Some(FEC_N_HIGH));
    }

    #[test]
    fn no_step_up_without_duration() {
        let mut abr = AbrController::new(&default_config());
        abr.current_bitrate_kbps = 4_000; // below max
        abr.on_viewer_joined("v1");

        let report = LossReport {
            packets_received: 1000,
            packets_lost: 0,
            observed_rx_kbps: None,
        };
        // Immediately after joining — not enough healthy duration
        let change = abr.process_loss_report("v1", &report);
        assert!(change.is_none());
    }

    #[test]
    fn fec_adapts_to_moderate_loss() {
        let mut abr = AbrController::new(&default_config());
        abr.on_viewer_joined("v1");

        // 3% loss — between step-up (1%) and step-down (5%) thresholds
        let report = LossReport {
            packets_received: 97,
            packets_lost: 3,
            observed_rx_kbps: None,
        };
        let change = abr.process_loss_report("v1", &report);
        assert!(change.is_some());
        assert_eq!(change.unwrap().new_fec_n, Some(FEC_N_MODERATE));
    }

    #[test]
    fn step_down_on_bandwidth_clamp_without_high_loss() {
        let mut abr = AbrController::new(&default_config());
        abr.on_viewer_joined("v1");

        // low loss but constrained throughput estimate
        let report = LossReport {
            packets_received: 995,
            packets_lost: 5,
            observed_rx_kbps: Some(3000),
        };

        let change = abr.process_loss_report("v1", &report).unwrap();
        match change.reason {
            BitrateChangeReason::BandwidthClamp { .. } => {}
            _ => panic!("expected bandwidth clamp"),
        }
        assert_eq!(
            change.new_bitrate_kbps,
            Some((3000.0_f32 * RX_KBPS_HEADROOM_FACTOR).round() as u32)
        );
    }

    #[test]
    fn step_up_is_capped_by_bandwidth_estimate() {
        let mut abr = AbrController::new(&default_config());
        abr.current_bitrate_kbps = 2_400;
        abr.max_bitrate_kbps = 8_000;
        abr.on_viewer_joined("v1");

        // Provide stable healthy loss reports and constrained throughput estimate.
        let report = LossReport {
            packets_received: 1000,
            packets_lost: 0,
            observed_rx_kbps: Some(3200),
        };

        let now = Instant::now();
        let state = abr.viewers.get_mut("v1").unwrap();
        state.last_report = now;
        state.healthy_since = Some(now - std::time::Duration::from_secs(STEP_UP_DURATION_SECS + 1));
        state.smoothed_rx_kbps = Some(3200.0);

        let change = abr.process_loss_report("v1", &report).unwrap();
        let cap = (3200.0_f32 * RX_KBPS_HEADROOM_FACTOR).round() as u32;
        let expected = ((2_400f32 * STEP_UP_FACTOR) as u32).min(cap.min(8_000));
        assert_eq!(change.new_bitrate_kbps, Some(expected));
    }

    #[test]
    fn tiny_throughput_sample_is_ignored() {
        let mut abr = AbrController::new(&default_config());
        abr.on_viewer_joined("v1");
        let before = abr.current_bitrate_kbps();

        // Below RX_KBPS_MIN_VALID (noise floor), so this must not trigger clamp.
        let report = LossReport {
            packets_received: 999,
            packets_lost: 1,
            observed_rx_kbps: Some(100),
        };

        let change = abr.process_loss_report("v1", &report);
        assert!(change.is_none());
        assert_eq!(abr.current_bitrate_kbps(), before);
    }
}
