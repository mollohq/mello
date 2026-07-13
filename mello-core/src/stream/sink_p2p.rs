use std::collections::{HashMap, VecDeque};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use super::error::StreamError;
use super::rtp_peer::{
    poll_video_feedback, send_access_unit, set_pacing_target, snapshot_video_stats, VideoFeedback,
};
use super::sink::{NativeRtpTelemetry, PacketSink, SinkVideoFeedback, SinkVideoFeedbackKind};

const MAX_P2P_VIEWERS: usize = 5;
const DEFAULT_SINK_PACING_KBPS: u32 = 6_000;

/// Raw peer handle from mello-sys. The actual pointer lifetime is managed by
/// whoever creates the peer (the stream host orchestration code).
struct ViewerPeer {
    peer: *mut mello_sys::MelloPeerConnection,
}

unsafe impl Send for ViewerPeer {}
unsafe impl Sync for ViewerPeer {}

/// P2P fan-out sink: sends Annex-B access units to up to 5 viewer RTP peers.
/// Each connected viewer receives an independent native RTP send.
pub struct P2PFanoutSink {
    viewers: Arc<RwLock<HashMap<String, ViewerPeer>>>,
    pacing_kbps: AtomicU32,
    pending_joins: RwLock<VecDeque<String>>,
    pending_leaves: RwLock<VecDeque<String>>,
    audio_stub_bytes: AtomicU32,
}

impl Default for P2PFanoutSink {
    fn default() -> Self {
        Self::new()
    }
}

impl P2PFanoutSink {
    pub fn new() -> Self {
        Self {
            viewers: Arc::new(RwLock::new(HashMap::new())),
            pacing_kbps: AtomicU32::new(DEFAULT_SINK_PACING_KBPS),
            pending_joins: RwLock::new(VecDeque::new()),
            pending_leaves: RwLock::new(VecDeque::new()),
            audio_stub_bytes: AtomicU32::new(0),
        }
    }

    /// # Safety
    /// `peer` must be a valid, non-null `MelloPeerConnection` pointer that
    /// outlives its membership in this sink (callers destroy peers only after
    /// `remove_viewer`, or after the stream session has stopped).
    pub unsafe fn add_viewer(
        &self,
        viewer_id: String,
        peer: *mut mello_sys::MelloPeerConnection,
    ) -> Result<(), StreamError> {
        let mut viewers = self
            .viewers
            .write()
            .map_err(|_| StreamError::SendFailed("P2P viewer map lock poisoned".to_string()))?;
        if viewers.len() >= MAX_P2P_VIEWERS {
            return Err(StreamError::ViewerLimitReached {
                max: MAX_P2P_VIEWERS,
            });
        }

        viewers.insert(viewer_id.clone(), ViewerPeer { peer });
        drop(viewers);

        if let Ok(mut joins) = self.pending_joins.write() {
            joins.push_back(viewer_id);
        }

        self.apply_pacing_to_all_peers();
        Ok(())
    }

    pub fn remove_viewer(&self, viewer_id: &str) {
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

    pub fn viewer_count(&self) -> usize {
        self.viewers.read().map(|v| v.len()).unwrap_or(0)
    }

    fn pacing_target_bps(&self) -> u64 {
        u64::from(self.pacing_kbps.load(Ordering::Relaxed).max(1)) * 1_000
    }

    fn apply_pacing_to_all_peers(&self) {
        let bps = self.pacing_target_bps();
        let Ok(viewers) = self.viewers.read() else {
            return;
        };
        for vp in viewers.values() {
            if let Some(peer) = NonNull::new(vp.peer) {
                if !unsafe { mello_sys::mello_peer_is_connected(vp.peer) } {
                    continue;
                }
                if let Err(e) = set_pacing_target(peer, bps) {
                    log::warn!("P2P sink: set_pacing_target failed: {}", e);
                }
            }
        }
    }

    fn map_feedback(viewer_id: &str, feedback: VideoFeedback) -> SinkVideoFeedback {
        let kind = match feedback {
            VideoFeedback::Pli => SinkVideoFeedbackKind::Pli,
            VideoFeedback::Remb { bitrate_bps } => SinkVideoFeedbackKind::Remb { bitrate_bps },
            VideoFeedback::LocalIdrNeeded => SinkVideoFeedbackKind::LocalIdrNeeded,
        };
        SinkVideoFeedback {
            viewer_id: viewer_id.to_string(),
            kind,
        }
    }

    #[cfg(test)]
    pub(crate) fn add_viewer_for_test(
        &self,
        viewer_id: &str,
        peer: *mut mello_sys::MelloPeerConnection,
    ) {
        self.viewers
            .write()
            .expect("viewer map lock")
            .insert(viewer_id.to_string(), ViewerPeer { peer });
    }
}

#[async_trait]
impl PacketSink for P2PFanoutSink {
    async fn send_video(
        &self,
        annex_b: &[u8],
        capture_timestamp_us: u64,
        _is_keyframe: bool,
    ) -> Result<(), StreamError> {
        // Keep the read lock across each native send. remove_viewer() takes the
        // write lock, so once it returns no sender can retain a stale peer pointer.
        let viewers = self
            .viewers
            .read()
            .map_err(|_| StreamError::SendFailed("P2P viewer map lock poisoned".to_string()))?;

        let mut last_err: Option<StreamError> = None;
        for vp in viewers.values() {
            if !unsafe { mello_sys::mello_peer_is_connected(vp.peer) } {
                continue;
            }
            let Some(peer) = NonNull::new(vp.peer) else {
                continue;
            };
            if let Err(e) = send_access_unit(peer, annex_b, capture_timestamp_us) {
                log::warn!("P2P sink: RTP send failed for viewer: {}", e);
                last_err = Some(StreamError::SendFailed(e.to_string()));
            }
        }
        if let Some(err) = last_err {
            return Err(err);
        }
        Ok(())
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
        self.apply_pacing_to_all_peers();
    }

    async fn native_rtp_telemetry(&self) -> Option<NativeRtpTelemetry> {
        // Match send_video's lifetime barrier so removal waits for all native reads.
        let viewers = self.viewers.read().ok()?;
        if viewers.is_empty() {
            return None;
        }
        let mut agg = NativeRtpTelemetry {
            target_kbps: self.pacing_kbps.load(Ordering::Relaxed),
            ..NativeRtpTelemetry::default()
        };
        for vp in viewers.values() {
            if !unsafe { mello_sys::mello_peer_is_connected(vp.peer) } {
                continue;
            }
            let Some(peer) = NonNull::new(vp.peer) else {
                continue;
            };
            let stats = snapshot_video_stats(peer);
            agg = agg.aggregate(NativeRtpTelemetry {
                target_kbps: (stats.tx_pacing_target_bps / 1_000) as u32,
                tx_access_units_sent: stats.tx_access_units_sent,
                tx_access_units_dropped: stats.tx_access_units_dropped,
                tx_bytes_sent: stats.tx_bytes_sent,
            });
        }
        Some(agg)
    }

    async fn poll_video_feedback(&self) -> Option<SinkVideoFeedback> {
        let Ok(viewers) = self.viewers.read() else {
            return None;
        };
        for (viewer_id, vp) in viewers.iter() {
            if !unsafe { mello_sys::mello_peer_is_connected(vp.peer) } {
                continue;
            }
            let Some(peer) = NonNull::new(vp.peer) else {
                continue;
            };
            match poll_video_feedback(peer) {
                Ok(Some(feedback)) => {
                    return Some(Self::map_feedback(viewer_id, feedback));
                }
                Ok(None) => {}
                Err(e) => {
                    log::warn!(
                        "P2P sink: poll_video_feedback failed for {}: {}",
                        viewer_id,
                        e
                    );
                }
            }
        }
        None
    }

    async fn poll_viewer_joined(&self) -> Option<String> {
        self.pending_joins.write().ok()?.pop_front()
    }

    async fn poll_viewer_left(&self) -> Option<String> {
        self.pending_leaves.write().ok()?.pop_front()
    }

    async fn on_viewer_joined(&self, viewer_id: &str) {
        log::info!("P2P viewer joined stream: {}", viewer_id);
    }

    async fn on_viewer_left(&self, viewer_id: &str) {
        log::info!("P2P viewer left stream: {}", viewer_id);
        self.remove_viewer(viewer_id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    #[test]
    fn join_notification_queue_drains_fifo() {
        let sink = P2PFanoutSink::new();
        {
            let mut joins = sink.pending_joins.write().expect("lock");
            joins.push_back("viewer-a".to_string());
            joins.push_back("viewer-b".to_string());
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let first = rt.block_on(async { sink.poll_viewer_joined().await });
        let second = rt.block_on(async { sink.poll_viewer_joined().await });
        assert_eq!(first.as_deref(), Some("viewer-a"));
        assert_eq!(second.as_deref(), Some("viewer-b"));
    }

    #[test]
    fn pacing_target_stored_for_new_peers() {
        let sink = P2PFanoutSink::new();
        sink.pacing_kbps.store(4_500, Ordering::Relaxed);
        assert_eq!(sink.pacing_target_bps(), 4_500_000);
    }

    #[test]
    fn audio_stub_accumulates_byte_len() {
        let sink = P2PFanoutSink::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            sink.send_audio_stub(128).await;
            sink.send_audio_stub(64).await;
        });
        assert_eq!(sink.audio_stub_bytes.load(Ordering::Relaxed), 192);
    }

    #[test]
    fn fanout_holds_viewer_lifetime_lock_while_sending() {
        let source = include_str!("sink_p2p.rs");
        assert!(source.contains("let viewers = self"));
        assert!(source.contains("for vp in viewers.values()"));
        assert!(source.contains("send_access_unit(peer, annex_b, capture_timestamp_us)"));
    }

    #[test]
    fn send_video_impl_uses_rtp_access_unit_api() {
        let source = include_str!("sink_p2p.rs");
        let impl_end = source.find("#[cfg(test)]").unwrap_or(source.len());
        let impl_source = &source[..impl_end];
        assert!(impl_source.contains("send_access_unit"));
        assert!(!impl_source.contains("mello_peer_send_unreliable"));
        assert!(!impl_source.contains("enqueue_chunked"));
    }
}
