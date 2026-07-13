use std::time::{Duration, Instant};

use mello_sys::MelloRtpVideoStats;

use super::config::Codec;

const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
const REMB_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const GOOD_SAMPLES_FOR_INCREASE: u32 = 10;
const LOSS_STEP_DOWN_THRESHOLD: f32 = 0.05;
const LOSS_MILD_THRESHOLD: f32 = 0.02;
const LOSS_GOOD_THRESHOLD: f32 = 0.01;
const JITTER_MILD_MS: f32 = 20.0;
const JITTER_GOOD_MS: f32 = 10.0;
const STEP_DOWN_FACTOR: f32 = 0.75;
const STEP_MILD_FACTOR: f32 = 0.85;
const MIN_INCREASE_BPS: u32 = 100_000;
const INCREASE_FRACTION: f32 = 0.05;
const RTP_CLOCK_HZ: f32 = 90_000.0;

/// Subset of native RTP stats used for receiver-side congestion sampling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CongestionSample {
    pub accepted_packets: u64,
    pub accepted_bytes: u64,
    pub missing_sequences: u64,
    pub repaired_packets: u64,
    pub incomplete_access_units: u64,
    pub emitted_access_units: u64,
    pub gate_entries: u64,
    pub gate_exits: u64,
    pub gate_dropped_access_units: u64,
    pub interarrival_jitter: u32,
}

impl CongestionSample {
    pub fn from_native(stats: &MelloRtpVideoStats) -> Self {
        Self {
            accepted_packets: stats.rx_core_accepted_packets,
            accepted_bytes: stats.rx_core_accepted_bytes,
            missing_sequences: stats.rx_core_missing_sequences_detected,
            repaired_packets: stats.rx_core_repaired_packets,
            incomplete_access_units: stats.rx_core_incomplete_access_units,
            emitted_access_units: stats.rx_core_emitted_access_units,
            gate_entries: stats.rx_core_gate_entries,
            gate_exits: stats.rx_core_gate_exits,
            gate_dropped_access_units: stats.rx_core_gate_dropped_access_units,
            interarrival_jitter: stats.rx_core_interarrival_jitter,
        }
    }

    pub fn delta_since(&self, previous: &Self) -> Self {
        Self {
            accepted_packets: self
                .accepted_packets
                .saturating_sub(previous.accepted_packets),
            accepted_bytes: self.accepted_bytes.saturating_sub(previous.accepted_bytes),
            missing_sequences: self
                .missing_sequences
                .saturating_sub(previous.missing_sequences),
            repaired_packets: self
                .repaired_packets
                .saturating_sub(previous.repaired_packets),
            incomplete_access_units: self
                .incomplete_access_units
                .saturating_sub(previous.incomplete_access_units),
            emitted_access_units: self
                .emitted_access_units
                .saturating_sub(previous.emitted_access_units),
            gate_entries: self.gate_entries.saturating_sub(previous.gate_entries),
            gate_exits: self.gate_exits.saturating_sub(previous.gate_exits),
            gate_dropped_access_units: self
                .gate_dropped_access_units
                .saturating_sub(previous.gate_dropped_access_units),
            interarrival_jitter: self.interarrival_jitter,
        }
    }

    pub fn loss_ratio(&self) -> f32 {
        let denom = self.accepted_packets.saturating_add(self.missing_sequences);
        if denom == 0 {
            return 0.0;
        }
        self.missing_sequences as f32 / denom as f32
    }

    pub fn jitter_ms(&self) -> f32 {
        self.interarrival_jitter as f32 * 1_000.0 / RTP_CLOCK_HZ
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CongestionAction {
    StepDownSevere,
    StepDownMild,
    Hold,
}

/// Viewer-side receive-target controller driven by native RTP stats deltas.
pub struct ViewerCongestionController {
    floor_bps: u32,
    ceiling_bps: u32,
    current_target_bps: u32,
    last_emitted_bps: u32,
    consecutive_good_samples: u32,
    last_stats: Option<CongestionSample>,
    last_sample_at: Option<Instant>,
    last_remb_emit_at: Option<Instant>,
}

impl ViewerCongestionController {
    pub fn new(ceiling_kbps: u32, codec: Codec) -> Self {
        let ceiling_bps = ceiling_kbps.saturating_mul(1_000);
        let floor_bps = StreamConfigFloor::bps(codec);
        let initial = ceiling_bps.max(floor_bps).min(ceiling_bps);
        Self {
            floor_bps,
            ceiling_bps,
            current_target_bps: initial,
            last_emitted_bps: 0,
            consecutive_good_samples: 0,
            last_stats: None,
            last_sample_at: None,
            last_remb_emit_at: None,
        }
    }

    pub fn current_target_bps(&self) -> u32 {
        self.current_target_bps
    }

    pub fn set_ceiling_kbps(&mut self, ceiling_kbps: u32) {
        self.ceiling_bps = ceiling_kbps.saturating_mul(1_000);
        self.current_target_bps = self.clamp_bps(self.current_target_bps);
    }

    /// Sample stats at most every 500 ms. Returns a receive target when REMB should be sent.
    pub fn tick(&mut self, stats: CongestionSample, now: Instant) -> Option<u32> {
        if self
            .last_sample_at
            .is_some_and(|last| now.duration_since(last) < SAMPLE_INTERVAL)
        {
            return self.maybe_emit_heartbeat(now);
        }

        let Some(previous) = self.last_stats else {
            self.last_stats = Some(stats);
            self.last_sample_at = Some(now);
            self.current_target_bps = self.clamp_bps(self.ceiling_bps);
            return self.emit_if_needed(now, true);
        };

        let delta = stats.delta_since(&previous);
        self.last_stats = Some(stats);
        self.last_sample_at = Some(now);

        let action = self.classify_sample(&delta);
        match action {
            CongestionAction::StepDownSevere => {
                self.consecutive_good_samples = 0;
                self.current_target_bps =
                    self.clamp_bps((self.current_target_bps as f32 * STEP_DOWN_FACTOR) as u32);
            }
            CongestionAction::StepDownMild => {
                self.consecutive_good_samples = 0;
                self.current_target_bps =
                    self.clamp_bps((self.current_target_bps as f32 * STEP_MILD_FACTOR) as u32);
            }
            CongestionAction::Hold => {
                if self.is_good_sample(&delta) {
                    self.consecutive_good_samples = self.consecutive_good_samples.saturating_add(1);
                    if self.consecutive_good_samples >= GOOD_SAMPLES_FOR_INCREASE {
                        self.apply_increase();
                    }
                } else {
                    self.consecutive_good_samples = 0;
                }
            }
        }

        self.emit_if_needed(now, false)
    }

    fn apply_increase(&mut self) {
        let step = MIN_INCREASE_BPS
            .max(((self.current_target_bps as f32) * INCREASE_FRACTION).round() as u32);
        self.current_target_bps = self.clamp_bps(self.current_target_bps.saturating_add(step));
        self.consecutive_good_samples = 0;
    }

    fn classify_sample(&self, delta: &CongestionSample) -> CongestionAction {
        let loss = delta.loss_ratio();
        if delta.incomplete_access_units > 0
            || delta.gate_entries > 0
            || loss > LOSS_STEP_DOWN_THRESHOLD
        {
            return CongestionAction::StepDownSevere;
        }
        if loss >= LOSS_MILD_THRESHOLD || delta.jitter_ms() > JITTER_MILD_MS {
            return CongestionAction::StepDownMild;
        }
        CongestionAction::Hold
    }

    fn is_good_sample(&self, delta: &CongestionSample) -> bool {
        delta.loss_ratio() < LOSS_GOOD_THRESHOLD
            && delta.incomplete_access_units == 0
            && delta.gate_entries == 0
            && delta.jitter_ms() < JITTER_GOOD_MS
    }

    fn clamp_bps(&self, bps: u32) -> u32 {
        bps.max(self.floor_bps).min(self.ceiling_bps)
    }

    fn emit_if_needed(&mut self, now: Instant, force: bool) -> Option<u32> {
        let heartbeat_due = self
            .last_remb_emit_at
            .map(|last| now.duration_since(last) >= REMB_HEARTBEAT_INTERVAL)
            .unwrap_or(true);
        let change_bps = self.current_target_bps.abs_diff(self.last_emitted_bps);
        let significant_change = self.last_emitted_bps == 0
            || change_bps.saturating_mul(100) >= self.last_emitted_bps.saturating_mul(5);

        if force || significant_change || heartbeat_due {
            self.last_emitted_bps = self.current_target_bps;
            self.last_remb_emit_at = Some(now);
            Some(self.current_target_bps)
        } else {
            None
        }
    }

    fn maybe_emit_heartbeat(&mut self, now: Instant) -> Option<u32> {
        self.emit_if_needed(now, false)
    }
}

struct StreamConfigFloor;

impl StreamConfigFloor {
    fn bps(codec: Codec) -> u32 {
        super::config::StreamConfig::min_bitrate_kbps(codec).saturating_mul(1_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller_with_ceiling(ceiling_kbps: u32) -> ViewerCongestionController {
        ViewerCongestionController::new(ceiling_kbps, Codec::H264)
    }

    fn advance(
        controller: &mut ViewerCongestionController,
        sample: CongestionSample,
        base: Instant,
        ms: u64,
    ) -> Option<u32> {
        controller.tick(sample, base + Duration::from_millis(ms))
    }

    #[test]
    fn initial_tick_emits_ceiling_target() {
        let mut controller = controller_with_ceiling(4_500);
        let base = Instant::now();
        let emit = advance(&mut controller, CongestionSample::default(), base, 0);
        assert_eq!(emit, Some(4_500_000));
        assert_eq!(controller.current_target_bps(), 4_500_000);
    }

    #[test]
    fn severe_loss_steps_down_immediately() {
        let mut controller = controller_with_ceiling(4_000);
        let base = Instant::now();
        let _ = advance(&mut controller, CongestionSample::default(), base, 0);
        let stats = CongestionSample {
            accepted_packets: 90,
            missing_sequences: 10,
            ..Default::default()
        };
        let emit = advance(&mut controller, stats, base, 500);
        assert_eq!(emit, Some(3_000_000));
    }

    #[test]
    fn gate_entry_triggers_severe_step_down() {
        let mut controller = controller_with_ceiling(4_000);
        let base = Instant::now();
        let _ = advance(&mut controller, CongestionSample::default(), base, 0);
        let stats = CongestionSample {
            gate_entries: 1,
            ..Default::default()
        };
        let emit = advance(&mut controller, stats, base, 500);
        assert_eq!(emit, Some(3_000_000));
    }

    #[test]
    fn mild_loss_steps_down_by_fifteen_percent() {
        let mut controller = controller_with_ceiling(4_000);
        let base = Instant::now();
        let _ = advance(&mut controller, CongestionSample::default(), base, 0);
        let stats = CongestionSample {
            accepted_packets: 96,
            missing_sequences: 4,
            ..Default::default()
        };
        let emit = advance(&mut controller, stats, base, 500);
        assert_eq!(emit, Some(3_400_000));
    }

    #[test]
    fn high_jitter_triggers_mild_step_down() {
        let mut controller = controller_with_ceiling(4_000);
        let base = Instant::now();
        let _ = advance(&mut controller, CongestionSample::default(), base, 0);
        let stats = CongestionSample {
            interarrival_jitter: 2_000, // ~22.2 ms
            ..Default::default()
        };
        let emit = advance(&mut controller, stats, base, 500);
        assert_eq!(emit, Some(3_400_000));
    }

    #[test]
    fn ten_good_samples_increase_by_at_least_one_hundred_kbps() {
        let mut controller = controller_with_ceiling(5_000);
        let base = Instant::now();
        let _ = advance(&mut controller, CongestionSample::default(), base, 0);
        let mild_loss = CongestionSample {
            accepted_packets: 96,
            missing_sequences: 4,
            ..Default::default()
        };
        let _ = advance(&mut controller, mild_loss, base, 500);
        assert_eq!(controller.current_target_bps(), 4_250_000);

        let mut emit = None;
        for i in 1..=10 {
            let stats = CongestionSample {
                accepted_packets: 100 + i * 100,
                ..Default::default()
            };
            emit = advance(&mut controller, stats, base, 500 + i * 500);
        }
        assert_eq!(controller.current_target_bps(), 4_462_500);
        assert_eq!(emit, Some(4_462_500));
    }

    #[test]
    fn target_is_clamped_to_floor() {
        let mut controller = controller_with_ceiling(2_000);
        let base = Instant::now();
        let _ = advance(&mut controller, CongestionSample::default(), base, 0);
        for i in 0..8 {
            let stats = CongestionSample {
                incomplete_access_units: 1,
                accepted_packets: i * 10,
                ..Default::default()
            };
            let _ = advance(&mut controller, stats, base, 500 + i * 500);
        }
        assert_eq!(controller.current_target_bps(), 1_500_000);
    }

    #[test]
    fn small_change_is_not_emitted_before_heartbeat() {
        let mut controller = controller_with_ceiling(4_000);
        let base = Instant::now();
        let _ = advance(&mut controller, CongestionSample::default(), base, 0);
        let stats = CongestionSample {
            accepted_packets: 100,
            ..Default::default()
        };
        let _ = advance(&mut controller, stats, base, 500);
        let emit = advance(&mut controller, stats, base, 900);
        assert!(emit.is_none());
    }

    #[test]
    fn heartbeat_re_emits_after_two_seconds() {
        let mut controller = controller_with_ceiling(4_000);
        let base = Instant::now();
        let _ = advance(&mut controller, CongestionSample::default(), base, 0);
        let emit = advance(&mut controller, CongestionSample::default(), base, 2_000);
        assert_eq!(emit, Some(4_000_000));
    }

    #[test]
    fn table_driven_policy_matrix() {
        struct Case {
            name: &'static str,
            delta: CongestionSample,
            start_kbps: u32,
            expected_kbps: u32,
        }

        let cases = [
            Case {
                name: "loss_above_five_percent",
                delta: CongestionSample {
                    accepted_packets: 94,
                    missing_sequences: 6,
                    ..Default::default()
                },
                start_kbps: 4_000,
                expected_kbps: 3_000,
            },
            Case {
                name: "incomplete_au",
                delta: CongestionSample {
                    incomplete_access_units: 1,
                    ..Default::default()
                },
                start_kbps: 4_000,
                expected_kbps: 3_000,
            },
            Case {
                name: "loss_between_two_and_five_percent",
                delta: CongestionSample {
                    accepted_packets: 96,
                    missing_sequences: 4,
                    ..Default::default()
                },
                start_kbps: 4_000,
                expected_kbps: 3_400,
            },
            Case {
                name: "jitter_above_twenty_ms",
                delta: CongestionSample {
                    interarrival_jitter: 1_900,
                    ..Default::default()
                },
                start_kbps: 4_000,
                expected_kbps: 3_400,
            },
        ];

        for case in cases {
            let mut controller = controller_with_ceiling(case.start_kbps);
            let base = Instant::now();
            let _ = advance(&mut controller, CongestionSample::default(), base, 0);
            let emit = advance(&mut controller, case.delta, base, 500);
            assert_eq!(
                emit,
                Some(case.expected_kbps * 1_000),
                "case {} failed",
                case.name
            );
        }
    }
}
