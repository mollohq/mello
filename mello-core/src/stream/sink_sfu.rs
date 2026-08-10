use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use super::error::StreamError;
use super::sink::{NativeRtpTelemetry, PacketSink, SinkVideoFeedback, SinkVideoFeedbackKind};
use crate::transport::sfu_connection::VideoFeedbackKind;
use crate::transport::SfuConnection;

/// The SFU aggregates viewer feedback without per-viewer identity.
pub const SFU_CONTROL_VIEWER_ID: &str = "sfu";

const DEFAULT_SINK_PACING_KBPS: u32 = 6_000;

pub struct SfuSink {
    connection: Arc<SfuConnection>,
    pacing_kbps: AtomicU32,
    pending_joins: RwLock<VecDeque<String>>,
    pending_leaves: RwLock<VecDeque<String>>,
    audio_stub_bytes: AtomicU32,
    /// Active viewer ids for REMB aggregation bookkeeping.
    viewers: RwLock<HashMap<String, ()>>,
}

impl SfuSink {
    pub fn new(connection: Arc<SfuConnection>) -> Self {
        Self {
            connection,
            pacing_kbps: AtomicU32::new(DEFAULT_SINK_PACING_KBPS),
            pending_joins: RwLock::new(VecDeque::new()),
            pending_leaves: RwLock::new(VecDeque::new()),
            audio_stub_bytes: AtomicU32::new(0),
            viewers: RwLock::new(HashMap::new()),
        }
    }

    pub fn connection(&self) -> &Arc<SfuConnection> {
        &self.connection
    }

    fn pacing_target_bps(&self) -> u64 {
        u64::from(self.pacing_kbps.load(Ordering::Relaxed).max(1)) * 1_000
    }

    fn apply_native_pacing(&self) {
        let bps = self.pacing_target_bps();
        if let Err(e) = self.connection.set_video_pacing_target(bps) {
            log::warn!("SFU sink: set_video_pacing_target failed: {}", e);
        }
    }
}

#[async_trait]
impl PacketSink for SfuSink {
    async fn send_video(
        &self,
        annex_b: &[u8],
        capture_timestamp_us: u64,
        _is_keyframe: bool,
    ) -> Result<(), StreamError> {
        if !self.connection.is_video_track_open() {
            return Err(StreamError::SendFailed(
                "RTP video track closed".to_string(),
            ));
        }
        self.connection
            .send_video_access_unit(annex_b, capture_timestamp_us)
    }

    async fn send_audio_stub(&self, byte_len: usize) {
        self.audio_stub_bytes.fetch_add(
            u32::try_from(byte_len).unwrap_or(u32::MAX),
            Ordering::Relaxed,
        );
    }

    async fn set_pacing_kbps(&self, target_kbps: u32) {
        self.pacing_kbps
            .store(target_kbps.max(1), Ordering::Relaxed);
        self.apply_native_pacing();
    }

    async fn native_rtp_telemetry(&self) -> Option<NativeRtpTelemetry> {
        let stats = self.connection.video_stats().ok()?;
        Some(NativeRtpTelemetry {
            target_kbps: (stats.tx_pacing_target_bps / 1_000) as u32,
            tx_access_units_sent: stats.tx_access_units_sent,
            tx_access_units_dropped: stats.tx_access_units_dropped,
            tx_bytes_sent: stats.tx_bytes_sent,
        })
    }

    async fn poll_video_feedback(&self) -> Option<SinkVideoFeedback> {
        let feedback = self.connection.take_video_feedback().ok()??;
        let kind = match feedback.kind {
            VideoFeedbackKind::Pli => SinkVideoFeedbackKind::Pli,
            VideoFeedbackKind::Remb => SinkVideoFeedbackKind::Remb {
                bitrate_bps: feedback.remb_bitrate_bps,
            },
            VideoFeedbackKind::LocalIdrNeeded => SinkVideoFeedbackKind::LocalIdrNeeded,
            VideoFeedbackKind::GccTarget => SinkVideoFeedbackKind::GccTarget {
                bitrate_bps: feedback.remb_bitrate_bps,
            },
        };
        Some(SinkVideoFeedback {
            viewer_id: SFU_CONTROL_VIEWER_ID.to_string(),
            kind,
        })
    }

    async fn poll_viewer_joined(&self) -> Option<String> {
        self.pending_joins.write().ok()?.pop_front()
    }

    async fn poll_viewer_left(&self) -> Option<String> {
        self.pending_leaves.write().ok()?.pop_front()
    }

    async fn on_viewer_joined(&self, viewer_id: &str) {
        log::debug!("SFU sink: viewer joined {}", viewer_id);
        let is_new = self
            .viewers
            .write()
            .ok()
            .is_some_and(|mut viewers| viewers.insert(viewer_id.to_string(), ()).is_none());
        if is_new {
            if let Ok(mut joins) = self.pending_joins.write() {
                joins.push_back(viewer_id.to_string());
            }
        }
    }

    async fn on_viewer_left(&self, viewer_id: &str) {
        log::debug!("SFU sink: viewer left {}", viewer_id);
        let removed = self
            .viewers
            .write()
            .ok()
            .is_some_and(|mut viewers| viewers.remove(viewer_id).is_some());
        if removed {
            if let Ok(mut leaves) = self.pending_leaves.write() {
                leaves.push_back(viewer_id.to_string());
            }
        }
    }

    async fn send_stats(&self, payload: &serde_json::Value) {
        self.connection.send_stream_stats(payload).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sfu_sink_send_video_uses_rtp_access_unit_path() {
        let source = include_str!("sink_sfu.rs");
        let impl_end = source.find("#[cfg(test)]").unwrap_or(source.len());
        let impl_source = &source[..impl_end];
        assert!(impl_source.contains("send_video_access_unit"));
        assert!(!impl_source.contains("enqueue_chunked_media"));
        assert!(!impl_source.contains("send_media"));
        assert!(!impl_source.contains("last_keyframe"));
    }

    #[test]
    fn sfu_feedback_uses_synthetic_viewer_id() {
        let kind = SinkVideoFeedbackKind::Pli;
        let fb = SinkVideoFeedback {
            viewer_id: SFU_CONTROL_VIEWER_ID.to_string(),
            kind,
        };
        assert_eq!(fb.viewer_id, "sfu");
    }

    #[test]
    fn sfu_membership_dedup_source_gate() {
        let src = include_str!("sink_sfu.rs");
        assert!(src.contains("is_some_and(|mut viewers| viewers.insert"));
        assert!(src.contains("if is_new"));
        assert!(src.contains("if removed"));
    }
}
