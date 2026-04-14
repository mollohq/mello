use std::sync::Arc;

use async_trait::async_trait;

use super::error::StreamError;
use super::pacer::PacingTelemetry;
use super::packet::StreamPacket;

/// Topology-agnostic packet sink. The stream manager sends encoded packets
/// through this trait — it doesn't know whether they go to P2P peers or an SFU.
#[async_trait]
pub trait PacketSink: Send + Sync {
    async fn send_video(&self, packet: &StreamPacket) -> Result<(), StreamError>;
    async fn send_audio(&self, packet: &StreamPacket) -> Result<(), StreamError>;
    async fn send_control(&self, packet: &StreamPacket) -> Result<(), StreamError>;
    async fn set_pacing_kbps(&self, target_kbps: u32);
    async fn pacing_telemetry(&self) -> Option<PacingTelemetry>;

    /// Called when a new viewer joins mid-session (triggers keyframe request).
    async fn on_viewer_joined(&self, viewer_id: &str);

    /// Called when a viewer leaves.
    async fn on_viewer_left(&self, viewer_id: &str);
}

/// Sends packets through both an SFU relay and direct P2P fanout.
/// The SFU path is fire-and-forget; P2P errors are also ignored so one
/// failing path doesn't stall the other.
pub struct DualSink {
    pub primary: Arc<dyn PacketSink>,
    pub secondary: Arc<dyn PacketSink>,
}

#[async_trait]
impl PacketSink for DualSink {
    async fn send_video(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let _ = self.primary.send_video(packet).await;
        let _ = self.secondary.send_video(packet).await;
        Ok(())
    }

    async fn send_audio(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let _ = self.primary.send_audio(packet).await;
        let _ = self.secondary.send_audio(packet).await;
        Ok(())
    }

    async fn send_control(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let _ = self.primary.send_control(packet).await;
        let _ = self.secondary.send_control(packet).await;
        Ok(())
    }

    async fn set_pacing_kbps(&self, target_kbps: u32) {
        self.primary.set_pacing_kbps(target_kbps).await;
        self.secondary.set_pacing_kbps(target_kbps).await;
    }

    async fn pacing_telemetry(&self) -> Option<PacingTelemetry> {
        let p = self.primary.pacing_telemetry().await;
        let s = self.secondary.pacing_telemetry().await;
        match (p, s) {
            (Some(a), Some(b)) => Some(a.aggregate(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    async fn on_viewer_joined(&self, viewer_id: &str) {
        self.primary.on_viewer_joined(viewer_id).await;
        self.secondary.on_viewer_joined(viewer_id).await;
    }

    async fn on_viewer_left(&self, viewer_id: &str) {
        self.primary.on_viewer_left(viewer_id).await;
        self.secondary.on_viewer_left(viewer_id).await;
    }
}
