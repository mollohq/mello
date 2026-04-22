use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use super::error::StreamError;
use super::pacer::{EgressPacer, PacingTelemetry};
use super::packet::StreamPacket;
use super::sink::PacketSink;
use super::sink_p2p::CHUNK_HEADER_SIZE;
use crate::transport::SfuConnection;

const DEFAULT_SINK_PACING_KBPS: u32 = 6_000;
const SFU_CHUNK_MAX_PAYLOAD: usize = 40_000;

/// Egress channel capacity — enough to absorb encoder bursts without blocking
/// the manager, but small enough to exert back-pressure before memory bloats.
const EGRESS_QUEUE_CAPACITY: usize = 128;

pub struct SfuSink {
    connection: Arc<SfuConnection>,
    msg_seq: AtomicU16,
    egress_tx: mpsc::Sender<Vec<u8>>,
    egress_spawned: OnceLock<()>,
    egress_rx: std::sync::Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
    pacer: Arc<Mutex<EgressPacer>>,
}

impl SfuSink {
    pub fn new(connection: Arc<SfuConnection>) -> Self {
        let pacer = Arc::new(Mutex::new(EgressPacer::new(DEFAULT_SINK_PACING_KBPS)));
        let (egress_tx, egress_rx) = mpsc::channel(EGRESS_QUEUE_CAPACITY);

        Self {
            connection,
            msg_seq: AtomicU16::new(0),
            egress_tx,
            egress_spawned: OnceLock::new(),
            egress_rx: std::sync::Mutex::new(Some(egress_rx)),
            pacer,
        }
    }

    /// Lazily spawn the egress task on first use — avoids requiring a tokio
    /// runtime context at construction time.
    fn ensure_egress_task(&self) {
        self.egress_spawned.get_or_init(|| {
            let conn = self.connection.clone();
            let pacer = self.pacer.clone();
            let mut rx = self
                .egress_rx
                .lock()
                .unwrap()
                .take()
                .expect("egress_rx taken twice");
            tokio::spawn(async move {
                while let Some(msg) = rx.recv().await {
                    pacer.lock().await.pace(msg.len()).await;
                    if let Err(e) = conn.send_media(&msg) {
                        log::warn!("SFU egress task send failed: bytes={} err={}", msg.len(), e);
                    }
                }
                log::info!("SFU egress task exited");
            });
        });
    }

    pub fn connection(&self) -> &Arc<SfuConnection> {
        &self.connection
    }
}

#[async_trait]
impl PacketSink for SfuSink {
    async fn send_video(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let data = packet.serialize();
        self.enqueue_chunked_media(&data)
    }

    async fn send_audio(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let data = packet.serialize();
        self.enqueue_chunked_media(&data)
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
    fn enqueue_chunked_media(&self, data: &[u8]) -> Result<(), StreamError> {
        self.ensure_egress_task();
        if !self.connection.is_media_channel_open() {
            return Ok(());
        }
        let chunk_count = data.len().div_ceil(SFU_CHUNK_MAX_PAYLOAD).max(1) as u16;
        let msg_id = self.msg_seq.fetch_add(1, Ordering::Relaxed);

        for chunk_idx in 0..chunk_count {
            let start = chunk_idx as usize * SFU_CHUNK_MAX_PAYLOAD;
            let end = (start + SFU_CHUNK_MAX_PAYLOAD).min(data.len());
            let payload = &data[start..end];

            let mut msg = Vec::with_capacity(CHUNK_HEADER_SIZE + payload.len());
            msg.extend_from_slice(&msg_id.to_le_bytes());
            msg.extend_from_slice(&chunk_idx.to_le_bytes());
            msg.extend_from_slice(&chunk_count.to_le_bytes());
            msg.extend_from_slice(payload);

            if self.egress_tx.try_send(msg).is_err() {
                log::warn!(
                    "SFU sink egress queue full: msg_id={} chunk={}/{} — dropping",
                    msg_id,
                    chunk_idx + 1,
                    chunk_count,
                );
                return Err(StreamError::SendFailed("egress queue full".to_string()));
            }
        }
        Ok(())
    }
}
