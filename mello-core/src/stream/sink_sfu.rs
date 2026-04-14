use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::error::StreamError;
use super::pacer::{EgressPacer, PacingTelemetry};
use super::packet::StreamPacket;
use super::sink::PacketSink;
use super::sink_p2p::{CHUNK_HEADER_SIZE, CHUNK_MAX_PAYLOAD};
use crate::transport::SfuConnection;

const DEFAULT_SINK_PACING_KBPS: u32 = 6_000;

pub struct SfuSink {
    connection: Arc<SfuConnection>,
    msg_seq: AtomicU16,
    pacer: Mutex<EgressPacer>,
}

impl SfuSink {
    pub fn new(connection: Arc<SfuConnection>) -> Self {
        Self {
            connection,
            msg_seq: AtomicU16::new(0),
            pacer: Mutex::new(EgressPacer::new(DEFAULT_SINK_PACING_KBPS)),
        }
    }

    pub fn connection(&self) -> &Arc<SfuConnection> {
        &self.connection
    }
}

#[async_trait]
impl PacketSink for SfuSink {
    async fn send_video(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let data = packet.serialize();
        self.send_chunked_media(&data).await
    }

    async fn send_audio(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let data = packet.serialize();
        self.send_chunked_media(&data).await
    }

    async fn send_control(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let data = packet.serialize();
        self.connection.send_control(&data)
    }

    async fn set_pacing_kbps(&self, target_kbps: u32) {
        self.pacer.lock().await.set_target_kbps(target_kbps);
    }

    async fn pacing_telemetry(&self) -> Option<PacingTelemetry> {
        Some(self.pacer.lock().await.telemetry())
    }

    async fn on_viewer_joined(&self, viewer_id: &str) {
        log::debug!("SFU sink: viewer joined {}", viewer_id);
    }

    async fn on_viewer_left(&self, viewer_id: &str) {
        log::debug!("SFU sink: viewer left {}", viewer_id);
    }
}

impl SfuSink {
    async fn send_chunked_media(&self, data: &[u8]) -> Result<(), StreamError> {
        let chunk_count = data.len().div_ceil(CHUNK_MAX_PAYLOAD).max(1) as u16;
        let msg_id = self.msg_seq.fetch_add(1, Ordering::Relaxed);

        for chunk_idx in 0..chunk_count {
            let start = chunk_idx as usize * CHUNK_MAX_PAYLOAD;
            let end = (start + CHUNK_MAX_PAYLOAD).min(data.len());
            let payload = &data[start..end];

            let mut msg = Vec::with_capacity(CHUNK_HEADER_SIZE + payload.len());
            msg.extend_from_slice(&msg_id.to_le_bytes());
            msg.extend_from_slice(&chunk_idx.to_le_bytes());
            msg.extend_from_slice(&chunk_count.to_le_bytes());
            msg.extend_from_slice(payload);

            self.pacer.lock().await.pace(msg.len()).await;
            self.connection.send_media(&msg)?;
        }
        Ok(())
    }
}
