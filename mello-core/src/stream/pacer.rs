use std::time::{Duration, Instant};

const PACER_MIN_TARGET_KBPS: u32 = 250;
const PACER_BURST_MS: u32 = 40;
const PACER_MAX_SLEEP_MS: u64 = 40;

#[derive(Debug, Clone, Copy, Default)]
pub struct PacingTelemetry {
    pub target_kbps: u32,
    pub paced_bytes: u64,
    pub sleep_count: u64,
    pub sleep_ms_total: u64,
}

impl PacingTelemetry {
    pub(crate) fn aggregate(self, other: Self) -> Self {
        Self {
            target_kbps: self.target_kbps.saturating_add(other.target_kbps),
            paced_bytes: self.paced_bytes.saturating_add(other.paced_bytes),
            sleep_count: self.sleep_count.saturating_add(other.sleep_count),
            sleep_ms_total: self.sleep_ms_total.saturating_add(other.sleep_ms_total),
        }
    }
}

/// Token-bucket pacer for smoothing stream egress bursts.
pub(crate) struct EgressPacer {
    target_kbps: u32,
    tokens_bytes: f64,
    burst_bytes: f64,
    last_refill: Instant,
    paced_bytes: u64,
    sleep_count: u64,
    sleep_ms_total: u64,
    oversize_pace_count: u64,
}

impl EgressPacer {
    pub(crate) fn new(target_kbps: u32) -> Self {
        let target_kbps = target_kbps.max(PACER_MIN_TARGET_KBPS);
        let burst_bytes = Self::calc_burst_bytes(target_kbps);
        Self {
            target_kbps,
            tokens_bytes: burst_bytes,
            burst_bytes,
            last_refill: Instant::now(),
            paced_bytes: 0,
            sleep_count: 0,
            sleep_ms_total: 0,
            oversize_pace_count: 0,
        }
    }

    pub(crate) fn set_target_kbps(&mut self, target_kbps: u32) {
        self.refill();
        self.target_kbps = target_kbps.max(PACER_MIN_TARGET_KBPS);
        self.burst_bytes = Self::calc_burst_bytes(self.target_kbps);
        if self.tokens_bytes > self.burst_bytes {
            self.tokens_bytes = self.burst_bytes;
        }
    }

    pub(crate) async fn pace(&mut self, bytes: usize) {
        let bytes = bytes as f64;
        // Packets can be larger than current bucket burst capacity (for example
        // a large keyframe chunk). If we require tokens >= bytes while tokens
        // are capped at burst_bytes, we can deadlock forever.
        let required_tokens = if bytes > self.burst_bytes {
            self.oversize_pace_count = self.oversize_pace_count.saturating_add(1);
            if self.oversize_pace_count <= 3 || self.oversize_pace_count.is_multiple_of(200) {
                log::warn!(
                    "Pacer oversized payload: bytes={} burst_bytes={:.0} target_kbps={}",
                    bytes as u64,
                    self.burst_bytes,
                    self.target_kbps
                );
            }
            self.burst_bytes
        } else {
            bytes
        };
        loop {
            self.refill();
            if self.tokens_bytes >= required_tokens {
                self.tokens_bytes -= required_tokens;
                self.paced_bytes = self.paced_bytes.saturating_add(bytes as u64);
                return;
            }

            let needed = (required_tokens - self.tokens_bytes).max(1.0);
            let wait_secs = needed / self.bytes_per_sec();
            let wait = Duration::from_secs_f64(wait_secs)
                .min(Duration::from_millis(PACER_MAX_SLEEP_MS))
                .max(Duration::from_millis(1));
            self.sleep_count = self.sleep_count.saturating_add(1);
            self.sleep_ms_total = self.sleep_ms_total.saturating_add(wait.as_millis() as u64);
            tokio::time::sleep(wait).await;
        }
    }

    pub(crate) fn telemetry(&self) -> PacingTelemetry {
        PacingTelemetry {
            target_kbps: self.target_kbps,
            paced_bytes: self.paced_bytes,
            sleep_count: self.sleep_count,
            sleep_ms_total: self.sleep_ms_total,
        }
    }

    fn calc_burst_bytes(target_kbps: u32) -> f64 {
        let bytes_per_sec = target_kbps as f64 * 1000.0 / 8.0;
        let burst = bytes_per_sec * (PACER_BURST_MS as f64 / 1000.0);
        burst.max(1_500.0)
    }

    fn bytes_per_sec(&self) -> f64 {
        self.target_kbps as f64 * 1000.0 / 8.0
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        if elapsed <= 0.0 {
            return;
        }
        self.tokens_bytes =
            (self.tokens_bytes + elapsed * self.bytes_per_sec()).min(self.burst_bytes);
    }
}

pub(crate) fn calc_stream_pacing_target_kbps(
    video_bitrate_kbps: u32,
    fec_n: usize,
    audio_budget_kbps: u32,
) -> u32 {
    let fec_factor = if fec_n >= 2 {
        1.0 + (1.0 / fec_n as f64)
    } else {
        1.0
    };
    let paced_video = (video_bitrate_kbps as f64 * fec_factor).round() as u32;
    paced_video
        .saturating_add(audio_budget_kbps)
        .max(PACER_MIN_TARGET_KBPS)
}

#[cfg(test)]
mod tests {
    use super::{calc_stream_pacing_target_kbps, EgressPacer, PacingTelemetry};
    use tokio::time::{timeout, Duration};

    #[test]
    fn pacing_target_accounts_for_fec_and_audio() {
        let base = calc_stream_pacing_target_kbps(4_000, 0, 160);
        let fec10 = calc_stream_pacing_target_kbps(4_000, 10, 160);
        let fec5 = calc_stream_pacing_target_kbps(4_000, 5, 160);

        assert!(base >= 4_000);
        assert!(fec10 > base);
        assert!(fec5 > fec10);
    }

    #[test]
    fn pacing_telemetry_aggregation_sums_fields() {
        let a = PacingTelemetry {
            target_kbps: 2_000,
            paced_bytes: 10_000,
            sleep_count: 3,
            sleep_ms_total: 15,
        };
        let b = PacingTelemetry {
            target_kbps: 3_000,
            paced_bytes: 20_000,
            sleep_count: 4,
            sleep_ms_total: 20,
        };
        let c = a.aggregate(b);
        assert_eq!(c.target_kbps, 5_000);
        assert_eq!(c.paced_bytes, 30_000);
        assert_eq!(c.sleep_count, 7);
        assert_eq!(c.sleep_ms_total, 35);
    }

    #[tokio::test]
    async fn oversized_payload_does_not_deadlock() {
        let mut pacer = EgressPacer::new(4_160);
        // > burst at this bitrate (roughly ~20KB with current constants)
        let oversized = 60_000usize;
        let res = timeout(Duration::from_millis(200), pacer.pace(oversized)).await;
        assert!(
            res.is_ok(),
            "pacer should not deadlock on oversized payloads"
        );
    }
}
