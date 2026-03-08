use std::collections::HashMap;
use std::ffi::CString;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SignalMessage {
    Offer { sdp: String },
    Answer { sdp: String },
    IceCandidate { candidate: String, sdp_mid: String, sdp_mline_index: i32 },
}

struct PeerState {
    peer: *mut mello_sys::MelloPeerConnection,
    _peer_id_c: CString,
}

unsafe impl Send for PeerState {}
unsafe impl Sync for PeerState {}

pub struct VoiceMesh {
    local_id: String,
    peers: HashMap<String, PeerState>,
    outgoing_signals: Vec<(String, SignalMessage)>,
}

impl VoiceMesh {
    pub fn new() -> Self {
        Self {
            local_id: String::new(),
            peers: HashMap::new(),
            outgoing_signals: Vec::new(),
        }
    }

    pub fn init(&mut self, local_id: &str, _member_ids: &[String]) {
        self.local_id = local_id.to_string();
        self.destroy_all_peers();
    }

    /// Create a peer connection. The lower ID creates the offer (deterministic).
    pub fn create_peer(&mut self, ctx: *mut mello_sys::MelloContext, local_id: &str, member_id: &str) {
        if self.peers.contains_key(member_id) { return; }
        if local_id == member_id { return; }

        let peer_id_c = CString::new(member_id).unwrap();
        let peer = unsafe {
            mello_sys::mello_peer_create(ctx, peer_id_c.as_ptr())
        };
        if peer.is_null() {
            log::error!("Failed to create peer connection for {}", member_id);
            return;
        }

        self.peers.insert(member_id.to_string(), PeerState { peer, _peer_id_c: peer_id_c });

        let should_offer = local_id < member_id;
        if should_offer {
            let sdp_ptr = unsafe { mello_sys::mello_peer_create_offer(peer) };
            if !sdp_ptr.is_null() {
                let sdp = unsafe { std::ffi::CStr::from_ptr(sdp_ptr) }
                    .to_string_lossy()
                    .into_owned();
                log::info!("Created offer for peer {}", member_id);
                self.outgoing_signals.push((
                    member_id.to_string(),
                    SignalMessage::Offer { sdp },
                ));
            }
        } else {
            log::info!("Waiting for offer from peer {}", member_id);
        }
    }

    pub fn handle_signal(&mut self, ctx: *mut mello_sys::MelloContext, from: &str, signal: SignalMessage) {
        match signal {
            SignalMessage::Offer { sdp } => {
                // If we don't have a peer for this sender yet, create one
                if !self.peers.contains_key(from) {
                    let peer_id_c = CString::new(from).unwrap();
                    let peer = unsafe { mello_sys::mello_peer_create(ctx, peer_id_c.as_ptr()) };
                    if peer.is_null() {
                        log::error!("Failed to create peer for incoming offer from {}", from);
                        return;
                    }
                    self.peers.insert(from.to_string(), PeerState { peer, _peer_id_c: peer_id_c });
                }

                let state = self.peers.get(from).unwrap();
                let sdp_c = CString::new(sdp).unwrap();
                let answer_ptr = unsafe {
                    mello_sys::mello_peer_create_answer(state.peer, sdp_c.as_ptr())
                };
                if !answer_ptr.is_null() {
                    let answer = unsafe { std::ffi::CStr::from_ptr(answer_ptr) }
                        .to_string_lossy()
                        .into_owned();
                    log::info!("Created answer for peer {}", from);
                    self.outgoing_signals.push((
                        from.to_string(),
                        SignalMessage::Answer { sdp: answer },
                    ));
                }
            }
            SignalMessage::Answer { sdp } => {
                if let Some(state) = self.peers.get(from) {
                    let sdp_c = CString::new(sdp).unwrap();
                    unsafe {
                        mello_sys::mello_peer_set_remote_description(state.peer, sdp_c.as_ptr(), false);
                    }
                    log::info!("Set remote answer from peer {}", from);
                }
            }
            SignalMessage::IceCandidate { candidate, sdp_mid, sdp_mline_index } => {
                if let Some(state) = self.peers.get(from) {
                    let cand_c = CString::new(candidate).unwrap();
                    let mid_c = CString::new(sdp_mid).unwrap();
                    let ice = mello_sys::MelloIceCandidate {
                        candidate: cand_c.as_ptr(),
                        sdp_mid: mid_c.as_ptr(),
                        sdp_mline_index,
                    };
                    unsafe {
                        mello_sys::mello_peer_add_ice_candidate(state.peer, &ice);
                    }
                }
            }
        }
    }

    pub fn drain_signals(&mut self) -> Vec<(String, SignalMessage)> {
        std::mem::take(&mut self.outgoing_signals)
    }

    /// Send audio data to all connected peers via unreliable channel
    pub fn broadcast_audio(&self, data: &[u8]) {
        for (_, state) in &self.peers {
            let connected = unsafe { mello_sys::mello_peer_is_connected(state.peer) };
            if connected {
                unsafe {
                    mello_sys::mello_peer_send_unreliable(
                        state.peer,
                        data.as_ptr(),
                        data.len() as i32,
                    );
                }
            }
        }
    }

    /// Poll received audio from all peers and feed to the audio pipeline
    pub fn poll_incoming(&self, ctx: *mut mello_sys::MelloContext) {
        let mut buf = [0u8; 4000];
        for (peer_id, state) in &self.peers {
            loop {
                let size = unsafe {
                    mello_sys::mello_peer_recv(state.peer, buf.as_mut_ptr(), buf.len() as i32)
                };
                if size <= 0 { break; }
                let peer_id_c = std::ffi::CString::new(peer_id.as_str()).unwrap();
                unsafe {
                    mello_sys::mello_voice_feed_packet(
                        ctx,
                        peer_id_c.as_ptr(),
                        buf.as_ptr(),
                        size,
                    );
                }
            }
        }
    }

    pub fn destroy_peer(&mut self, member_id: &str) {
        if let Some(state) = self.peers.remove(member_id) {
            unsafe { mello_sys::mello_peer_destroy(state.peer); }
            log::info!("Destroyed peer connection for {}", member_id);
        }
    }

    pub fn destroy_all_peers(&mut self) {
        for (id, state) in self.peers.drain() {
            unsafe { mello_sys::mello_peer_destroy(state.peer); }
            log::info!("Destroyed peer connection for {}", id);
        }
    }
}
