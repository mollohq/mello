use async_trait::async_trait;

use super::error::StreamError;
use super::pacer::PacingTelemetry;

/// Host-side viewer feedback from native RTP (PLI, REMB, local IDR needed,
/// GCC send-side estimate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkVideoFeedbackKind {
    Pli,
    Remb { bitrate_bps: u32 },
    LocalIdrNeeded,
    GccTarget { bitrate_bps: u32 },
}

/// One polled feedback event with viewer identity preserved by the sink topology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkVideoFeedback {
    pub viewer_id: String,
    pub kind: SinkVideoFeedbackKind,
}

/// Aggregated native RTP egress stats across sink peers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeRtpTelemetry {
    pub target_kbps: u32,
    pub tx_access_units_sent: u64,
    pub tx_access_units_dropped: u64,
    pub tx_bytes_sent: u64,
}

impl NativeRtpTelemetry {
    /// Map into the legacy pacing telemetry shape still consumed by streaming.rs.
    pub fn to_pacing_telemetry(self) -> PacingTelemetry {
        PacingTelemetry {
            target_kbps: self.target_kbps,
            paced_bytes: self.tx_bytes_sent,
            sleep_count: 0,
            sleep_ms_total: 0,
        }
    }

    pub(crate) fn aggregate(self, other: Self) -> Self {
        Self {
            target_kbps: self.target_kbps.max(other.target_kbps),
            tx_access_units_sent: self
                .tx_access_units_sent
                .saturating_add(other.tx_access_units_sent),
            tx_access_units_dropped: self
                .tx_access_units_dropped
                .saturating_add(other.tx_access_units_dropped),
            tx_bytes_sent: self.tx_bytes_sent.saturating_add(other.tx_bytes_sent),
        }
    }
}

/// Topology-agnostic packet sink. The stream manager sends encoded Annex-B access
/// units through this trait — it doesn't know whether they go to P2P peers or an SFU.
#[async_trait]
pub trait PacketSink: Send + Sync {
    /// Send one complete encoded Annex-B access unit with its capture timestamp.
    async fn send_video(
        &self,
        annex_b: &[u8],
        capture_timestamp_us: u64,
        is_keyframe: bool,
    ) -> Result<(), StreamError>;

    /// Stream game-audio transport is stubbed — account only, no wire send.
    async fn send_audio_stub(&self, byte_len: usize);

    /// Propagate the pacing target to each native RTP sender (bits/sec internally).
    async fn set_pacing_kbps(&self, target_kbps: u32);

    /// Aggregated native RTP egress stats for telemetry.
    async fn native_rtp_telemetry(&self) -> Option<NativeRtpTelemetry>;

    /// Legacy pacing telemetry adapter for existing host debug events.
    async fn pacing_telemetry(&self) -> Option<PacingTelemetry> {
        self.native_rtp_telemetry()
            .await
            .map(NativeRtpTelemetry::to_pacing_telemetry)
    }

    /// Poll one queued native video feedback event, if any.
    async fn poll_video_feedback(&self) -> Option<SinkVideoFeedback>;

    /// Drain one viewer-join notification queued by the sink (e.g. P2P add_viewer).
    async fn poll_viewer_joined(&self) -> Option<String>;

    /// Drain one viewer-leave notification queued by the sink (e.g. P2P remove_viewer).
    async fn poll_viewer_left(&self) -> Option<String>;

    /// Called when a new viewer joins mid-session.
    async fn on_viewer_joined(&self, viewer_id: &str);

    /// Called when a viewer leaves; drops per-viewer feedback state.
    async fn on_viewer_left(&self, viewer_id: &str);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_rtp_telemetry_maps_to_pacing_telemetry() {
        let native = NativeRtpTelemetry {
            target_kbps: 6_000,
            tx_access_units_sent: 120,
            tx_access_units_dropped: 2,
            tx_bytes_sent: 900_000,
        };
        let pacing = native.to_pacing_telemetry();
        assert_eq!(pacing.target_kbps, 6_000);
        assert_eq!(pacing.paced_bytes, 900_000);
        assert_eq!(pacing.sleep_count, 0);
        assert_eq!(pacing.sleep_ms_total, 0);
    }

    #[test]
    fn native_rtp_telemetry_aggregate_keeps_max_target_and_sums_traffic() {
        let a = NativeRtpTelemetry {
            target_kbps: 2_000,
            tx_access_units_sent: 10,
            tx_access_units_dropped: 1,
            tx_bytes_sent: 50_000,
        };
        let b = NativeRtpTelemetry {
            target_kbps: 3_000,
            tx_access_units_sent: 20,
            tx_access_units_dropped: 2,
            tx_bytes_sent: 70_000,
        };
        let agg = a.aggregate(b);
        assert_eq!(agg.target_kbps, 3_000);
        assert_eq!(agg.tx_access_units_sent, 30);
        assert_eq!(agg.tx_access_units_dropped, 3);
        assert_eq!(agg.tx_bytes_sent, 120_000);
    }
}
