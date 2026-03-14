use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;

use super::error::StreamError;
use super::packet::StreamPacket;
use super::sink::PacketSink;

const MAX_P2P_VIEWERS: usize = 5;

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
}

impl P2PFanoutSink {
    pub fn new() -> Self {
        Self {
            viewers: RwLock::new(HashMap::new()),
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

    fn broadcast(&self, data: &[u8]) {
        let viewers = self.viewers.read().unwrap();
        for vp in viewers.values() {
            let connected = unsafe { mello_sys::mello_peer_is_connected(vp.peer) };
            if connected {
                unsafe {
                    mello_sys::mello_peer_send_unreliable(
                        vp.peer,
                        data.as_ptr(),
                        data.len() as i32,
                    );
                }
            }
        }
    }
}

#[async_trait]
impl PacketSink for P2PFanoutSink {
    async fn send_video(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let bytes = packet.serialize();
        self.broadcast(&bytes);
        Ok(())
    }

    async fn send_audio(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let bytes = packet.serialize();
        self.broadcast(&bytes);
        Ok(())
    }

    async fn send_control(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let bytes = packet.serialize();
        self.broadcast(&bytes);
        Ok(())
    }

    async fn on_viewer_joined(&self, viewer_id: &str) {
        log::info!("P2P viewer joined stream: {}", viewer_id);
    }

    async fn on_viewer_left(&self, viewer_id: &str) {
        log::info!("P2P viewer left stream: {}", viewer_id);
        self.remove_viewer(viewer_id);
    }
}
