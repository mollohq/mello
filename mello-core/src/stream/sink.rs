use async_trait::async_trait;

use super::error::StreamError;
use super::packet::StreamPacket;

/// Topology-agnostic packet sink. The stream manager sends encoded packets
/// through this trait — it doesn't know whether they go to P2P peers or an SFU.
#[async_trait]
pub trait PacketSink: Send + Sync {
    async fn send_video(&self, packet: &StreamPacket) -> Result<(), StreamError>;
    async fn send_audio(&self, packet: &StreamPacket) -> Result<(), StreamError>;
    async fn send_control(&self, packet: &StreamPacket) -> Result<(), StreamError>;

    /// Called when a new viewer joins mid-session (triggers keyframe request).
    async fn on_viewer_joined(&self, viewer_id: &str);

    /// Called when a viewer leaves.
    async fn on_viewer_left(&self, viewer_id: &str);
}
