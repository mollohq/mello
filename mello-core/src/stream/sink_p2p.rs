use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::RwLock;

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::error::StreamError;
use super::pacer::{EgressPacer, PacingTelemetry};
use super::packet::StreamPacket;
use super::sink::PacketSink;

const MAX_P2P_VIEWERS: usize = 5;
const DEFAULT_SINK_PACING_KBPS: u32 = 6_000;

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
    viewers: RwLock<HashMap<String, ViewerPeer>>,
    msg_seq: AtomicU16,
    pacer: Mutex<EgressPacer>,
}

impl Default for P2PFanoutSink {
    fn default() -> Self {
        Self::new()
    }
}

impl P2PFanoutSink {
    pub fn new() -> Self {
        Self {
            viewers: RwLock::new(HashMap::new()),
            msg_seq: AtomicU16::new(0),
            pacer: Mutex::new(EgressPacer::new(DEFAULT_SINK_PACING_KBPS)),
        }
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

    /// Send data, chunking if it exceeds CHUNK_MAX_PAYLOAD.
    /// Small messages (<= CHUNK_MAX_PAYLOAD) are sent as a single chunk.
    async fn broadcast(&self, data: &[u8]) {
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

            let connected_count = {
                let viewers = self.viewers.read().unwrap();
                viewers
                    .values()
                    .filter(|vp| unsafe { mello_sys::mello_peer_is_connected(vp.peer) })
                    .count()
            };
            if connected_count == 0 {
                return;
            }

            self.pacer
                .lock()
                .await
                .pace(msg.len() * connected_count)
                .await;

            {
                let viewers = self.viewers.read().unwrap();
                for vp in viewers.values() {
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
    }
}

#[async_trait]
impl PacketSink for P2PFanoutSink {
    async fn send_video(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let bytes = packet.serialize();
        self.broadcast(&bytes).await;
        Ok(())
    }

    async fn send_audio(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let bytes = packet.serialize();
        self.broadcast(&bytes).await;
        Ok(())
    }

    async fn send_control(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let bytes = packet.serialize();
        self.broadcast(&bytes).await;
        Ok(())
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
