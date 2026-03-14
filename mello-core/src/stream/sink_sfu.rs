use async_trait::async_trait;

use super::error::StreamError;
use super::packet::StreamPacket;
use super::sink::PacketSink;

/// SFU sink stub — v0.2. Wire protocol is specified in a future 13-SFU.md.
/// Construction always fails with `SfuNotImplemented`.
pub struct SfuSink {
    #[allow(dead_code)]
    endpoint: String,
    #[allow(dead_code)]
    token: String,
}

impl SfuSink {
    pub async fn new(endpoint: &str, token: &str) -> Result<Self, StreamError> {
        log::warn!(
            "SFU sink requested (endpoint={}) but not implemented yet",
            endpoint
        );
        let _ = (endpoint, token);
        Err(StreamError::SfuNotImplemented)
    }
}

#[async_trait]
impl PacketSink for SfuSink {
    async fn send_video(&self, _packet: &StreamPacket) -> Result<(), StreamError> {
        Err(StreamError::SfuNotImplemented)
    }

    async fn send_audio(&self, _packet: &StreamPacket) -> Result<(), StreamError> {
        Err(StreamError::SfuNotImplemented)
    }

    async fn send_control(&self, _packet: &StreamPacket) -> Result<(), StreamError> {
        Err(StreamError::SfuNotImplemented)
    }

    async fn on_viewer_joined(&self, _viewer_id: &str) {}
    async fn on_viewer_left(&self, _viewer_id: &str) {}
}
