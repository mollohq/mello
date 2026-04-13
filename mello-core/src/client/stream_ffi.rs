use std::collections::HashMap;
use std::ffi::CStr;
use std::sync::Arc;

use crate::stream::viewer::StreamViewer;
use crate::voice::{SignalEnvelope, SignalMessage, SignalPurpose};

use super::FrameSlot;

pub(super) struct FrameCallbackData {
    pub frame_slot: FrameSlot,
    pub frame_consumed: Arc<std::sync::atomic::AtomicBool>,
}

/// Reassembles chunked DataChannel messages back into full StreamPackets.
pub(super) struct ChunkAssembler {
    pending: HashMap<u16, ChunkAssembly>,
}

struct ChunkAssembly {
    chunk_count: u16,
    chunks_received: u16,
    chunks: Vec<Option<Vec<u8>>>,
}

impl ChunkAssembler {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }

    /// Feed a raw DataChannel message. Returns the reassembled payload if complete.
    pub fn feed(&mut self, raw: &[u8]) -> Option<Vec<u8>> {
        use crate::stream::sink_p2p::{CHUNK_HEADER_SIZE, CHUNK_MAX_PAYLOAD};
        const MAX_CHUNKS_PER_MESSAGE: u16 = 64;
        if raw.len() < CHUNK_HEADER_SIZE {
            return None;
        }

        let msg_id = u16::from_le_bytes([raw[0], raw[1]]);
        let chunk_idx = u16::from_le_bytes([raw[2], raw[3]]);
        let chunk_count = u16::from_le_bytes([raw[4], raw[5]]);
        let payload = &raw[CHUNK_HEADER_SIZE..];

        if chunk_count == 0
            || chunk_count > MAX_CHUNKS_PER_MESSAGE
            || chunk_idx >= chunk_count
            || payload.len() > CHUNK_MAX_PAYLOAD
        {
            return None;
        }

        // Evict stale assemblies (keep only messages within a recent window)
        self.pending.retain(|&id, _| msg_id.wrapping_sub(id) < 64);

        let entry = self.pending.entry(msg_id).or_insert_with(|| ChunkAssembly {
            chunk_count,
            chunks_received: 0,
            chunks: (0..chunk_count).map(|_| None).collect(),
        });

        let idx = chunk_idx as usize;
        if idx < entry.chunks.len() && entry.chunks[idx].is_none() {
            entry.chunks[idx] = Some(payload.to_vec());
            entry.chunks_received += 1;
        }

        if entry.chunks_received == entry.chunk_count {
            let assembly = self.pending.remove(&msg_id).unwrap();
            let total: usize = assembly
                .chunks
                .iter()
                .map(|c| c.as_ref().map_or(0, |v| v.len()))
                .sum();
            let mut result = Vec::with_capacity(total);
            for data in assembly.chunks.into_iter().flatten() {
                result.extend_from_slice(&data);
            }
            Some(result)
        } else {
            None
        }
    }
}

/// State for the viewer-side streaming pipeline.
pub(super) struct ViewerState {
    /// The C++ viewer pipeline handle. None until the host's Answer arrives
    /// with the actual encode resolution so we can initialize the decoder correctly.
    pub viewer: Option<*mut mello_sys::MelloStreamView>,
    /// P2P peer to host (only in P2P mode).
    pub peer: *mut mello_sys::MelloPeerConnection,
    /// SFU connection (only in SFU mode).
    pub sfu_connection: Option<Arc<crate::transport::SfuConnection>>,
    /// "sfu" or "p2p"
    pub mode: String,
    pub host_id: String,
    pub _frame_cb_data: *mut FrameCallbackData,
    pub _ice_cb_data: *mut StreamIceCallbackData,
    pub got_keyframe: bool,
    pub frames_presented: u64,
    pub transport_packets: u64,
    pub transport_bytes: u64,
    pub transport_truncations: u64,
    pub recv_buf: Vec<u8>,
    pub stream_viewer: StreamViewer,
    pub chunk_assembler: ChunkAssembler,
}

unsafe impl Send for ViewerState {}
unsafe impl Sync for ViewerState {}

impl Drop for ViewerState {
    fn drop(&mut self) {
        unsafe {
            if let Some(v) = self.viewer {
                if !v.is_null() {
                    mello_sys::mello_stream_stop_viewer(v);
                }
            }
            if !self.peer.is_null() {
                mello_sys::mello_peer_destroy(self.peer);
            }
            if !self._frame_cb_data.is_null() {
                drop(Box::from_raw(self._frame_cb_data));
            }
            if !self._ice_cb_data.is_null() {
                drop(Box::from_raw(self._ice_cb_data));
            }
        }
        // SfuConnection is Arc-dropped automatically; leave() is called in handle_stop_watching
    }
}

pub(super) struct StreamIceCallbackData {
    pub peer_id: String,
    pub send_queue: std::sync::Arc<std::sync::Mutex<Vec<(String, SignalEnvelope)>>>,
    /// ICE candidates gathered before the offer/answer is queued.
    /// Once `flushed` is true, new candidates go straight to `send_queue`.
    pub pending: std::sync::Mutex<Vec<SignalEnvelope>>,
    pub flushed: std::sync::atomic::AtomicBool,
}

pub(super) struct StreamHostPeer {
    pub peer: *mut mello_sys::MelloPeerConnection,
    pub ice_cb_data: *mut StreamIceCallbackData,
}

unsafe impl Send for StreamHostPeer {}
unsafe impl Sync for StreamHostPeer {}

/// Send-safe wrapper for MelloStreamHost pointer, used to pass across async boundaries.
pub(super) struct StreamHostHandle(pub *mut mello_sys::MelloStreamHost);
unsafe impl Send for StreamHostHandle {}

pub(super) unsafe extern "C" fn on_viewer_frame(
    user_data: *mut std::ffi::c_void,
    rgba: *const u8,
    w: u32,
    h: u32,
    _ts: u64,
) {
    if user_data.is_null() || rgba.is_null() || w == 0 || h == 0 {
        return;
    }
    let data = &*(user_data as *const FrameCallbackData);
    let expected_len = (w * h) as usize * 4;
    let src = std::slice::from_raw_parts(rgba, expected_len);
    if let Ok(mut slot) = data.frame_slot.lock() {
        match slot.as_mut() {
            Some((ow, oh, buf)) if buf.len() == expected_len => {
                buf.copy_from_slice(src);
                *ow = w;
                *oh = h;
            }
            _ => {
                *slot = Some((w, h, src.to_vec()));
            }
        }
        data.frame_consumed
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

pub(super) unsafe extern "C" fn stream_ice_callback(
    user_data: *mut std::ffi::c_void,
    candidate: *const mello_sys::MelloIceCandidate,
) {
    if user_data.is_null() || candidate.is_null() {
        return;
    }
    let data = &*(user_data as *const StreamIceCallbackData);
    let c = &*candidate;
    let cand = CStr::from_ptr(c.candidate).to_string_lossy().into_owned();
    let mid = CStr::from_ptr(c.sdp_mid).to_string_lossy().into_owned();
    let idx = c.sdp_mline_index;
    log::debug!(
        "Stream ICE candidate gathered for peer {}: {}",
        data.peer_id,
        cand
    );

    let envelope = SignalEnvelope {
        purpose: SignalPurpose::Stream,
        stream_width: None,
        stream_height: None,
        message: SignalMessage::IceCandidate {
            candidate: cand,
            sdp_mid: mid,
            sdp_mline_index: idx,
        },
    };

    if data.flushed.load(std::sync::atomic::Ordering::Acquire) {
        // Offer/answer already queued — send directly
        if let Ok(mut q) = data.send_queue.lock() {
            q.push((data.peer_id.clone(), envelope));
        }
    } else {
        // Buffer until offer/answer is queued
        if let Ok(mut buf) = data.pending.lock() {
            buf.push(envelope);
        }
    }
}

pub(super) unsafe extern "C" fn stream_state_callback(
    user_data: *mut std::ffi::c_void,
    state: i32,
) {
    if user_data.is_null() {
        return;
    }
    let data = &*(user_data as *const StreamIceCallbackData);
    let label = match state {
        0 => "New",
        1 => "Connecting",
        2 => "Connected",
        3 => "Disconnected",
        4 => "Failed",
        5 => "Closed",
        _ => "Unknown",
    };
    if state == 4 {
        log::error!(
            "Stream peer {} ICE state: {} — NAT traversal failed",
            data.peer_id,
            label
        );
    } else if state == 2 {
        log::info!("Stream peer {} ICE state: {}", data.peer_id, label);
    } else {
        log::debug!("Stream peer {} ICE state: {}", data.peer_id, label);
    }
}

/// Flush buffered ICE candidates from a `StreamIceCallbackData` into the main
/// send queue. Must be called *after* the offer/answer has been pushed to `send_queue`.
/// Sets `flushed = true` so subsequent candidates go directly to the send queue.
pub(super) fn flush_ice_buffer(cb_data: &StreamIceCallbackData) {
    let buffered: Vec<SignalEnvelope> = cb_data
        .pending
        .lock()
        .map(|mut buf| std::mem::take(&mut *buf))
        .unwrap_or_default();
    if !buffered.is_empty() {
        if let Ok(mut q) = cb_data.send_queue.lock() {
            for envelope in buffered {
                q.push((cb_data.peer_id.clone(), envelope));
            }
        }
    }
    cb_data
        .flushed
        .store(true, std::sync::atomic::Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::ChunkAssembler;
    use crate::stream::sink_p2p::{CHUNK_HEADER_SIZE, CHUNK_MAX_PAYLOAD};

    fn make_chunk(msg_id: u16, idx: u16, count: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(CHUNK_HEADER_SIZE + payload.len());
        out.extend_from_slice(&msg_id.to_le_bytes());
        out.extend_from_slice(&idx.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn chunk_assembler_reassembles_complete_message() {
        let mut asm = ChunkAssembler::new();
        let c0 = make_chunk(7, 0, 2, b"hello ");
        let c1 = make_chunk(7, 1, 2, b"world");

        assert!(asm.feed(&c0).is_none());
        let msg = asm.feed(&c1).expect("message should reassemble");
        assert_eq!(msg, b"hello world");
    }

    #[test]
    fn chunk_assembler_rejects_invalid_chunk_count() {
        let mut asm = ChunkAssembler::new();
        let bad = make_chunk(8, 0, 65, b"x");
        assert!(asm.feed(&bad).is_none());
    }

    #[test]
    fn chunk_assembler_rejects_oversized_chunk_payload() {
        let mut asm = ChunkAssembler::new();
        let oversized = vec![0u8; CHUNK_MAX_PAYLOAD + 1];
        let bad = make_chunk(9, 0, 1, &oversized);
        assert!(asm.feed(&bad).is_none());
    }
}
