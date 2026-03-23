use std::sync::Arc;

use async_trait::async_trait;

use super::error::StreamError;
use super::packet::StreamPacket;
use super::sink::PacketSink;
use crate::transport::SfuConnection;

pub struct SfuSink {
    connection: Arc<SfuConnection>,
}

impl SfuSink {
    pub fn new(connection: Arc<SfuConnection>) -> Self {
        Self { connection }
    }

    pub fn connection(&self) -> &Arc<SfuConnection> {
        &self.connection
    }
}

#[async_trait]
impl PacketSink for SfuSink {
    async fn send_video(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let data = packet.serialize();
        self.connection.send_media(&data)
    }

    async fn send_audio(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let data = packet.serialize();
        self.connection.send_media(&data)
    }

    async fn send_control(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let data = packet.serialize();
        self.connection.send_control(&data)
    }

    async fn on_viewer_joined(&self, viewer_id: &str) {
        log::debug!("SFU sink: viewer joined {}", viewer_id);
    }

    async fn on_viewer_left(&self, viewer_id: &str) {
        log::debug!("SFU sink: viewer left {}", viewer_id);
    }
}
