use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use super::error::StreamError;
use super::pacer::{EgressPacer, PacingTelemetry};
use super::packet::StreamPacket;
use super::sink::PacketSink;

const MAX_P2P_VIEWERS: usize = 5;
const DEFAULT_SINK_PACING_KBPS: u32 = 6_000;
const EGRESS_QUEUE_CAPACITY: usize = 128;

/// Max payload per DataChannel message. SCTP fragments anything larger into
/// many chunks; losing a single fragment kills the entire message in unreliable
/// mode. Matches the proven chunk size from stream-host tool.
pub const CHUNK_MAX_PAYLOAD: usize = 60_000;

/// Chunk header: [msg_id:2][chunk_idx:2][chunk_count:2] = 6 bytes.
/// Prepended to every DataChannel message so the viewer can reassemble.
pub const CHUNK_HEADER_SIZE: usize = 6;

/// Raw peer handle from mello-sys. The actual pointer lifetime is managed by
/// whoever creates the peer (the stream host orchestration code).
struct ViewerPeer {
    peer: *mut mello_sys::MelloPeerConnection,
}

unsafe impl Send for ViewerPeer {}
unsafe impl Sync for ViewerPeer {}

/// P2P fan-out sink: sends packets to up to 5 viewer DataChannel connections.
/// Fan-out is fire-and-forget — a slow or disconnected viewer does not stall
/// the pipeline for other viewers.
pub struct P2PFanoutSink {
    viewers: Arc<RwLock<HashMap<String, ViewerPeer>>>,
    msg_seq: AtomicU16,
    egress_tx: mpsc::Sender<Vec<u8>>,
    egress_spawned: OnceLock<()>,
    egress_rx: std::sync::Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
    pacer: Arc<Mutex<EgressPacer>>,
}

impl Default for P2PFanoutSink {
    fn default() -> Self {
        Self::new()
    }
}

impl P2PFanoutSink {
    pub fn new() -> Self {
        let viewers = Arc::new(RwLock::new(HashMap::new()));
        let pacer = Arc::new(Mutex::new(EgressPacer::new(DEFAULT_SINK_PACING_KBPS)));
        let (egress_tx, egress_rx) = mpsc::channel(EGRESS_QUEUE_CAPACITY);

        Self {
            viewers,
            msg_seq: AtomicU16::new(0),
            egress_tx,
            egress_spawned: OnceLock::new(),
            egress_rx: std::sync::Mutex::new(Some(egress_rx)),
            pacer,
        }
    }

    fn ensure_egress_task(&self) {
        self.egress_spawned.get_or_init(|| {
            let viewers = self.viewers.clone();
            let pacer = self.pacer.clone();
            let mut rx = self
                .egress_rx
                .lock()
                .unwrap()
                .take()
                .expect("egress_rx taken twice");
            tokio::spawn(async move {
                while let Some(msg) = rx.recv().await {
                    let connected_count = {
                        let vw = viewers.read().unwrap();
                        vw.values()
                            .filter(|vp| unsafe { mello_sys::mello_peer_is_connected(vp.peer) })
                            .count()
                    };
                    if connected_count == 0 {
                        continue;
                    }

                    pacer.lock().await.pace(msg.len() * connected_count).await;

                    {
                        let vw = viewers.read().unwrap();
                        for vp in vw.values() {
                            let connected = unsafe { mello_sys::mello_peer_is_connected(vp.peer) };
                            if !connected {
                                continue;
                            }
                            unsafe {
                                mello_sys::mello_peer_send_unreliable(
                                    vp.peer,
                                    msg.as_ptr(),
                                    msg.len() as i32,
                                );
                            }
                        }
                    }
                }
                log::info!("P2P egress task exited");
            });
        });
    }

    pub fn add_viewer(
        &self,
        viewer_id: String,
        peer: *mut mello_sys::MelloPeerConnection,
    ) -> Result<(), StreamError> {
        let mut viewers = self.viewers.write().unwrap();
        if viewers.len() >= MAX_P2P_VIEWERS {
            return Err(StreamError::ViewerLimitReached {
                max: MAX_P2P_VIEWERS,
            });
        }
        viewers.insert(viewer_id, ViewerPeer { peer });
        Ok(())
    }

    pub fn remove_viewer(&self, viewer_id: &str) {
        let mut viewers = self.viewers.write().unwrap();
        viewers.remove(viewer_id);
    }

    pub fn viewer_count(&self) -> usize {
        self.viewers.read().unwrap().len()
    }

    fn enqueue_chunked(&self, data: &[u8]) -> Result<(), StreamError> {
        self.ensure_egress_task();
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

            if self.egress_tx.try_send(msg).is_err() {
                log::warn!(
                    "P2P sink egress queue full: msg_id={} chunk={}/{} — dropping",
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

#[async_trait]
impl PacketSink for P2PFanoutSink {
    async fn send_video(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let bytes = packet.serialize();
        self.enqueue_chunked(&bytes)
    }

    async fn send_audio(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let bytes = packet.serialize();
        self.enqueue_chunked(&bytes)
    }

    async fn send_control(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let bytes = packet.serialize();
        self.enqueue_chunked(&bytes)
    }

    async fn set_pacing_kbps(&self, target_kbps: u32) {
        self.pacer.lock().await.set_target_kbps(target_kbps);
    }

    async fn pacing_telemetry(&self) -> Option<PacingTelemetry> {
        Some(self.pacer.lock().await.telemetry())
    }

    async fn on_viewer_joined(&self, viewer_id: &str) {
        log::info!("P2P viewer joined stream: {}", viewer_id);
    }

    async fn on_viewer_left(&self, viewer_id: &str) {
        log::info!("P2P viewer left stream: {}", viewer_id);
        self.remove_viewer(viewer_id);
    }
}
