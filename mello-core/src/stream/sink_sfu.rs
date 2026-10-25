use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use async_trait::async_trait;

use super::error::StreamError;
use super::sink::{NativeRtpTelemetry, PacketSink, SinkVideoFeedback, SinkVideoFeedbackKind};
use crate::transport::sfu_connection::VideoFeedbackKind;
use crate::transport::SfuConnection;

/// The SFU aggregates viewer feedback without per-viewer identity.
pub const SFU_CONTROL_VIEWER_ID: &str = "sfu";

const DEFAULT_SINK_PACING_KBPS: u32 = 6_000;

/// The sink of a stream host: one SFU connection.
///
/// The connection can come later than the sink, and it can change
/// (streaming-quality stage 6). The manager drains the encoder from the
/// first access unit while the SFU join runs, and a reconnect replaces the
/// connection in the same session. Without a connection, video and audio
/// drop as backpressure: that costs no recovery and no key frame. Each
/// attach applies the pacing and goog_cc settings to the new peer again and
/// asks the manager for a key frame, so the new track starts with an IDR.
pub struct SfuSink {
    connection: RwLock<Option<Arc<SfuConnection>>>,
    pacing_kbps: AtomicU32,
    /// goog_cc limits (min, start, max), kept for the next attach.
    bitrate_limits: Mutex<Option<(u32, u32, u32)>>,
    /// The encoder allocation, kept for the next attach. Zero is unset.
    max_allocated_kbps: AtomicU32,
    /// A key frame request for the manager, set by `attach`.
    idr_on_attach: AtomicBool,
    /// When the current connection was attached, and when its first video
    /// access unit went out (the stream start timing).
    attached_at: Mutex<Option<Instant>>,
    first_video_sent_at: Mutex<Option<Instant>>,
    pending_joins: RwLock<VecDeque<String>>,
    pending_leaves: RwLock<VecDeque<String>>,
    /// Active viewer ids for REMB aggregation bookkeeping.
    viewers: RwLock<HashMap<String, ()>>,
}

impl SfuSink {
    /// A sink on `connection`, attached from the start.
    pub fn new(connection: Arc<SfuConnection>) -> Self {
        let sink = Self::detached();
        if let Ok(mut slot) = sink.connection.write() {
            *slot = Some(connection);
        }
        if let Ok(mut at) = sink.attached_at.lock() {
            *at = Some(Instant::now());
        }
        sink
    }

    /// A sink with no connection yet. Video and audio drop until `attach`.
    pub fn detached() -> Self {
        Self {
            connection: RwLock::new(None),
            pacing_kbps: AtomicU32::new(DEFAULT_SINK_PACING_KBPS),
            bitrate_limits: Mutex::new(None),
            max_allocated_kbps: AtomicU32::new(0),
            idr_on_attach: AtomicBool::new(false),
            attached_at: Mutex::new(None),
            first_video_sent_at: Mutex::new(None),
            pending_joins: RwLock::new(VecDeque::new()),
            pending_leaves: RwLock::new(VecDeque::new()),
            viewers: RwLock::new(HashMap::new()),
        }
    }

    /// Send on `connection` from now on. Applies the kept pacing target,
    /// goog_cc limits and allocation to its peer, and asks the manager for
    /// a key frame: the next access unit on the new track is an IDR.
    /// Returns the connection it replaces.
    pub fn attach(&self, connection: Arc<SfuConnection>) -> Option<Arc<SfuConnection>> {
        self.apply_native_pacing(&connection);
        if let Ok(limits) = self.bitrate_limits.lock() {
            if let Some((min, start, max)) = *limits {
                apply_bitrate_limits(&connection, min, start, max);
            }
        }
        let allocated = self.max_allocated_kbps.load(Ordering::Relaxed);
        if allocated > 0 {
            apply_max_allocated(&connection, allocated);
        }
        let old = match self.connection.write() {
            Ok(mut slot) => slot.replace(connection),
            Err(_) => return None,
        };
        if let Ok(mut at) = self.attached_at.lock() {
            *at = Some(Instant::now());
        }
        if let Ok(mut first) = self.first_video_sent_at.lock() {
            *first = None;
        }
        self.idr_on_attach.store(true, Ordering::Release);
        log::info!(
            "SFU sink: connection attached (replaces one: {})",
            old.is_some()
        );
        old
    }

    /// Stop sending. Returns the connection it held.
    pub fn detach(&self) -> Option<Arc<SfuConnection>> {
        let old = self
            .connection
            .write()
            .ok()
            .and_then(|mut slot| slot.take());
        if old.is_some() {
            log::info!("SFU sink: connection detached; media drops until the next attach");
        }
        old
    }

    /// The viewers the sink knows (joined and not left).
    pub fn viewer_ids(&self) -> Vec<String> {
        self.viewers
            .read()
            .map(|v| v.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The connection the sink sends on now.
    pub fn connection(&self) -> Option<Arc<SfuConnection>> {
        self.connection.read().ok().and_then(|slot| slot.clone())
    }

    /// When the current connection was attached, and when its first video
    /// access unit went out. `None` until that access unit went out.
    pub fn first_video_sent(&self) -> Option<(Instant, Instant)> {
        let first = (*self.first_video_sent_at.lock().ok()?)?;
        let attached = (*self.attached_at.lock().ok()?)?;
        Some((attached, first))
    }

    fn pacing_target_bps(&self) -> u64 {
        u64::from(self.pacing_kbps.load(Ordering::Relaxed).max(1)) * 1_000
    }

    fn apply_native_pacing(&self, connection: &SfuConnection) {
        let bps = self.pacing_target_bps();
        if let Err(e) = connection.set_video_pacing_target(bps) {
            log::warn!("SFU sink: set_video_pacing_target failed: {}", e);
        }
    }

    fn not_connected() -> StreamError {
        StreamError::SfuSendBackpressure("no SFU connection".to_string())
    }
}

fn apply_bitrate_limits(connection: &SfuConnection, min_kbps: u32, start_kbps: u32, max_kbps: u32) {
    let kbps = |v: u32| u64::from(v) * 1_000;
    if let Err(e) =
        connection.set_video_bitrate_limits(kbps(min_kbps), kbps(start_kbps), kbps(max_kbps))
    {
        log::warn!("SFU sink: set_video_bitrate_limits failed: {}", e);
    }
}

fn apply_max_allocated(connection: &SfuConnection, kbps: u32) {
    if let Err(e) = connection.set_video_max_allocated(u64::from(kbps) * 1_000) {
        log::warn!("SFU sink: set_video_max_allocated failed: {}", e);
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
        let Some(connection) = self.connection() else {
            return Err(Self::not_connected());
        };
        if !connection.is_video_track_open() {
            return Err(StreamError::SendFailed(
                "RTP video track closed".to_string(),
            ));
        }
        connection.send_video_access_unit(annex_b, capture_timestamp_us)?;
        if let Ok(mut first) = self.first_video_sent_at.lock() {
            if first.is_none() {
                *first = Some(Instant::now());
            }
        }
        Ok(())
    }

    async fn send_audio(&self, opus: &[u8], timestamp: u32) -> Result<(), StreamError> {
        if opus.is_empty() {
            return Ok(());
        }
        let Some(connection) = self.connection() else {
            return Err(Self::not_connected());
        };
        connection.send_audio(opus, timestamp)
    }

    async fn set_pacing_kbps(&self, target_kbps: u32) {
        self.pacing_kbps
            .store(target_kbps.max(1), Ordering::Relaxed);
        if let Some(connection) = self.connection() {
            self.apply_native_pacing(&connection);
        }
    }

    async fn send_control(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        // The relay fans host control out to viewers (same channel the
        // cursor packet was designed for). Best-effort by trait contract.
        let Some(connection) = self.connection() else {
            return;
        };
        if let Err(e) = connection.send_control(data) {
            log::debug!("SFU sink: control send failed: {}", e);
        }
    }

    async fn native_rtp_telemetry(&self) -> Option<NativeRtpTelemetry> {
        let stats = self.connection()?.video_stats().ok()?;
        Some(NativeRtpTelemetry {
            target_kbps: (stats.tx_pacing_target_bps / 1_000) as u32,
            tx_access_units_sent: stats.tx_access_units_sent,
            tx_access_units_dropped: stats.tx_access_units_dropped,
            tx_bytes_sent: stats.tx_bytes_sent,
        })
    }

    async fn poll_video_feedback(&self) -> Option<SinkVideoFeedback> {
        // A new connection starts with an IDR: its track has none yet.
        if self.idr_on_attach.swap(false, Ordering::AcqRel) {
            return Some(SinkVideoFeedback {
                viewer_id: SFU_CONTROL_VIEWER_ID.to_string(),
                kind: SinkVideoFeedbackKind::LocalIdrNeeded,
            });
        }
        let feedback = self.connection()?.take_video_feedback().ok()??;
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
        if let Some(connection) = self.connection() {
            connection.send_stream_stats(payload).await;
        }
    }

    async fn set_bitrate_limits(&self, min_kbps: u32, start_kbps: u32, max_kbps: u32) {
        if let Ok(mut limits) = self.bitrate_limits.lock() {
            *limits = Some((min_kbps, start_kbps, max_kbps));
        }
        if let Some(connection) = self.connection() {
            apply_bitrate_limits(&connection, min_kbps, start_kbps, max_kbps);
        }
    }

    async fn set_max_allocated_kbps(&self, kbps: u32) {
        self.max_allocated_kbps.store(kbps, Ordering::Relaxed);
        if let Some(connection) = self.connection() {
            apply_max_allocated(&connection, kbps);
        }
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

    /// Stage 6: before the SFU join ends, the manager drains the encoder
    /// into a sink with no connection. That costs backpressure only, not a
    /// send failure, which would gate the deltas and ask for key frames.
    #[tokio::test]
    async fn a_detached_sink_drops_media_as_backpressure() {
        let sink = SfuSink::detached();
        assert!(matches!(
            sink.send_video(&[0, 0, 0, 1, 0x65], 1, true).await,
            Err(StreamError::SfuSendBackpressure(_))
        ));
        assert!(matches!(
            sink.send_audio(&[1, 2, 3], 960).await,
            Err(StreamError::SfuSendBackpressure(_))
        ));
        assert!(sink.native_rtp_telemetry().await.is_none());
        assert!(sink.poll_video_feedback().await.is_none());
        assert!(sink.connection().is_none());
        assert!(sink.first_video_sent().is_none());
    }

    /// Settings that arrive before the attach are kept for it.
    #[tokio::test]
    async fn a_detached_sink_keeps_the_settings_for_the_attach() {
        let sink = SfuSink::detached();
        sink.set_pacing_kbps(7_000).await;
        sink.set_bitrate_limits(300, 4_800, 8_000).await;
        sink.set_max_allocated_kbps(8_000).await;
        assert_eq!(sink.pacing_kbps.load(Ordering::Relaxed), 7_000);
        assert_eq!(
            *sink.bitrate_limits.lock().expect("limits"),
            Some((300, 4_800, 8_000))
        );
        assert_eq!(sink.max_allocated_kbps.load(Ordering::Relaxed), 8_000);
    }
}
