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
    last_keyframe: Mutex<Option<Vec<u8>>>,
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
            last_keyframe: Mutex::new(None),
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
        if packet.is_keyframe() {
            *self.last_keyframe.lock().await = Some(data.clone());
        }
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
        let cached = self.last_keyframe.lock().await.clone();
        if let Some(frame) = cached {
            if let Err(e) = self.enqueue_chunked_media(&frame) {
                log::warn!(
                    "SFU sink: failed to replay cached keyframe for viewer {}: {}",
                    viewer_id,
                    e
                );
            } else {
                log::debug!(
                    "SFU sink: replayed cached keyframe to newly joined viewer {}",
                    viewer_id
                );
            }
        }
    }

    async fn on_viewer_left(&self, viewer_id: &str) {
        log::debug!("SFU sink: viewer left {}", viewer_id);
    }
}

impl SfuSink {
    fn enqueue_chunked_media(&self, data: &[u8]) -> Result<(), StreamError> {
        self.ensure_egress_task();
        if !self.connection.is_media_channel_open() {
            return Err(StreamError::SendFailed("media channel closed".to_string()));
        }
        let chunk_count = data.len().div_ceil(SFU_CHUNK_MAX_PAYLOAD).max(1) as u16;

        // Pre-flight: check that enough queue capacity exists for all chunks.
        // This avoids sending partial messages that the viewer can never reassemble.
        let available = self.egress_tx.capacity();
        if (chunk_count as usize) > available {
            let msg_id = self.msg_seq.load(Ordering::Relaxed);
            log::warn!(
                "SFU sink egress: dropping whole frame msg_id={} ({} chunks, {} slots free)",
                msg_id,
                chunk_count,
                available,
            );
            return Err(StreamError::SendFailed(
                "egress queue full — whole frame dropped".to_string(),
            ));
        }

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
                    "SFU sink egress queue full mid-frame: msg_id={} chunk={}/{} — dropping remainder",
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
