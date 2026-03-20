use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalPurpose {
    #[default]
    Voice,
    Stream,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SignalMessage {
    Offer {
        sdp: String,
    },
    Answer {
        sdp: String,
    },
    IceCandidate {
        candidate: String,
        sdp_mid: String,
        sdp_mline_index: i32,
    },
}

/// Wire-format envelope that wraps SignalMessage with a purpose discriminator.
/// Old clients that omit `purpose` will deserialize as Voice (backward compat).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalEnvelope {
    #[serde(default)]
    pub purpose: SignalPurpose,
    #[serde(flatten)]
    pub message: SignalMessage,
    /// Host encode resolution, included in Stream Answer so the viewer
    /// can initialize the decoder at the correct size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_height: Option<u32>,
}

struct IceCallbackData {
    peer_id: String,
    queue: Arc<Mutex<Vec<(String, SignalMessage)>>>,
}

struct PeerState {
    peer: *mut mello_sys::MelloPeerConnection,
    _peer_id_c: CString,
    ice_cb_data: *mut IceCallbackData,
}

unsafe impl Send for PeerState {}
unsafe impl Sync for PeerState {}

pub struct VoiceMesh {
    local_id: String,
    peers: HashMap<String, PeerState>,
    outgoing_signals: Vec<(String, SignalMessage)>,
    ice_signal_queue: Arc<Mutex<Vec<(String, SignalMessage)>>>,
    ice_servers: Vec<CString>,
}

impl Default for VoiceMesh {
    fn default() -> Self {
        Self::new()
    }
}

impl VoiceMesh {
    pub fn new() -> Self {
        Self {
            local_id: String::new(),
            peers: HashMap::new(),
            outgoing_signals: Vec::new(),
            ice_signal_queue: Arc::new(Mutex::new(Vec::new())),
            ice_servers: Vec::new(),
        }
    }

    pub fn set_ice_servers(&mut self, urls: Vec<String>) {
        self.ice_servers = urls
            .into_iter()
            .filter_map(|u| CString::new(u).ok())
            .collect();
        log::info!("ICE servers configured: {} entries", self.ice_servers.len());
    }

    pub fn init(&mut self, local_id: &str, _member_ids: &[String]) {
        self.local_id = local_id.to_string();
        self.destroy_all_peers();
    }

    fn make_peer(&self, ctx: *mut mello_sys::MelloContext, member_id: &str) -> Option<PeerState> {
        let peer_id_c = CString::new(member_id).unwrap();
        let peer = unsafe { mello_sys::mello_peer_create(ctx, peer_id_c.as_ptr()) };
        if peer.is_null() {
            log::error!("Failed to create peer connection for {}", member_id);
            return None;
        }

        if !self.ice_servers.is_empty() {
            let ptrs: Vec<*const std::os::raw::c_char> =
                self.ice_servers.iter().map(|s| s.as_ptr()).collect();
            unsafe {
                mello_sys::mello_peer_set_ice_servers(
                    peer,
                    ptrs.as_ptr() as *mut *const std::os::raw::c_char,
                    ptrs.len() as std::os::raw::c_int,
                );
            }
        }

        let cb_data = Box::into_raw(Box::new(IceCallbackData {
            peer_id: member_id.to_string(),
            queue: Arc::clone(&self.ice_signal_queue),
        }));

        unsafe extern "C" fn ice_callback(
            user_data: *mut std::ffi::c_void,
            candidate: *const mello_sys::MelloIceCandidate,
        ) {
            if user_data.is_null() || candidate.is_null() {
                return;
            }
            let data = &*(user_data as *const IceCallbackData);
            let c = &*candidate;
            let cand = CStr::from_ptr(c.candidate).to_string_lossy().into_owned();
            let mid = CStr::from_ptr(c.sdp_mid).to_string_lossy().into_owned();
            let idx = c.sdp_mline_index;
            log::debug!("ICE candidate gathered for peer {}: {}", data.peer_id, cand);
            if let Ok(mut queue) = data.queue.lock() {
                queue.push((
                    data.peer_id.clone(),
                    SignalMessage::IceCandidate {
                        candidate: cand,
                        sdp_mid: mid,
                        sdp_mline_index: idx,
                    },
                ));
            }
        }

        unsafe {
            mello_sys::mello_peer_set_ice_callback(
                peer,
                Some(ice_callback),
                cb_data as *mut std::ffi::c_void,
            );
        }

        Some(PeerState {
            peer,
            _peer_id_c: peer_id_c,
            ice_cb_data: cb_data,
        })
    }

    /// Create a peer connection. The lower ID creates the offer (deterministic).
    pub fn create_peer(
        &mut self,
        ctx: *mut mello_sys::MelloContext,
        local_id: &str,
        member_id: &str,
    ) {
        if self.peers.contains_key(member_id) {
            return;
        }
        if local_id == member_id {
            return;
        }

        let state = match self.make_peer(ctx, member_id) {
            Some(s) => s,
            None => return,
        };
        let peer = state.peer;
        self.peers.insert(member_id.to_string(), state);

        let should_offer = local_id < member_id;
        if should_offer {
            let sdp_ptr = unsafe { mello_sys::mello_peer_create_offer(peer) };
            if !sdp_ptr.is_null() {
                let sdp = unsafe { CStr::from_ptr(sdp_ptr) }
                    .to_string_lossy()
                    .into_owned();
                log::info!("Created offer for peer {}", member_id);
                self.outgoing_signals
                    .push((member_id.to_string(), SignalMessage::Offer { sdp }));
            }
        } else {
            log::info!("Waiting for offer from peer {}", member_id);
        }
    }

    pub fn handle_signal(
        &mut self,
        ctx: *mut mello_sys::MelloContext,
        from: &str,
        signal: SignalMessage,
    ) {
        match signal {
            SignalMessage::Offer { sdp } => {
                if !self.peers.contains_key(from) {
                    let state = match self.make_peer(ctx, from) {
                        Some(s) => s,
                        None => return,
                    };
                    self.peers.insert(from.to_string(), state);
                }

                let state = self.peers.get(from).unwrap();
                let sdp_c = CString::new(sdp).unwrap();
                let answer_ptr =
                    unsafe { mello_sys::mello_peer_create_answer(state.peer, sdp_c.as_ptr()) };
                if !answer_ptr.is_null() {
                    let answer = unsafe { CStr::from_ptr(answer_ptr) }
                        .to_string_lossy()
                        .into_owned();
                    log::info!("Created answer for peer {}", from);
                    self.outgoing_signals
                        .push((from.to_string(), SignalMessage::Answer { sdp: answer }));
                }
            }
            SignalMessage::Answer { sdp } => {
                if let Some(state) = self.peers.get(from) {
                    let sdp_c = CString::new(sdp).unwrap();
                    unsafe {
                        mello_sys::mello_peer_set_remote_description(
                            state.peer,
                            sdp_c.as_ptr(),
                            false,
                        );
                    }
                    log::info!("Set remote answer from peer {}", from);
                }
            }
            SignalMessage::IceCandidate {
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => {
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
                    log::debug!("Added remote ICE candidate from peer {}", from);
                }
            }
        }
    }

    pub fn drain_signals(&mut self) -> Vec<(String, SignalMessage)> {
        let mut signals = std::mem::take(&mut self.outgoing_signals);
        if let Ok(mut ice_signals) = self.ice_signal_queue.lock() {
            signals.append(&mut ice_signals);
        }
        signals
    }

    /// Send audio data to all connected peers via unreliable channel
    pub fn broadcast_audio(&self, data: &[u8]) {
        for state in self.peers.values() {
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
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn poll_incoming(&self, ctx: *mut mello_sys::MelloContext) {
        let mut buf = [0u8; 4000];
        for (peer_id, state) in &self.peers {
            loop {
                let size = unsafe {
                    mello_sys::mello_peer_recv(state.peer, buf.as_mut_ptr(), buf.len() as i32)
                };
                if size <= 0 {
                    break;
                }
                let peer_id_c = std::ffi::CString::new(peer_id.as_str()).unwrap();
                unsafe {
                    mello_sys::mello_voice_feed_packet(ctx, peer_id_c.as_ptr(), buf.as_ptr(), size);
                }
            }
        }
    }

    pub fn destroy_peer(&mut self, member_id: &str) {
        if let Some(state) = self.peers.remove(member_id) {
            unsafe {
                mello_sys::mello_peer_destroy(state.peer);
            }
            if !state.ice_cb_data.is_null() {
                unsafe {
                    let _ = Box::from_raw(state.ice_cb_data);
                }
            }
            log::info!("Destroyed peer connection for {}", member_id);
        }
    }

    pub fn destroy_all_peers(&mut self) {
        for (id, state) in self.peers.drain() {
            unsafe {
                mello_sys::mello_peer_destroy(state.peer);
            }
            if !state.ice_cb_data.is_null() {
                unsafe {
                    let _ = Box::from_raw(state.ice_cb_data);
                }
            }
            log::info!("Destroyed peer connection for {}", id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SignalMessage;

    #[test]
    fn signal_offer_roundtrip() {
        let msg = SignalMessage::Offer {
            sdp: "v=0\r\no=- 123 456 IN IP4 0.0.0.0\r\n".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::Offer { sdp } => assert!(sdp.contains("v=0")),
            _ => panic!("expected Offer"),
        }
    }

    #[test]
    fn signal_answer_roundtrip() {
        let msg = SignalMessage::Answer {
            sdp: "v=0\r\nanswer_sdp\r\n".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::Answer { sdp } => assert!(sdp.contains("answer_sdp")),
            _ => panic!("expected Answer"),
        }
    }

    #[test]
    fn signal_ice_roundtrip() {
        let msg = SignalMessage::IceCandidate {
            candidate: "candidate:1 1 UDP 2122252543 192.168.1.1 12345 typ host".into(),
            sdp_mid: "0".into(),
            sdp_mline_index: 0,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::IceCandidate {
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => {
                assert!(candidate.contains("candidate:1"));
                assert_eq!(sdp_mid, "0");
                assert_eq!(sdp_mline_index, 0);
            }
            _ => panic!("expected IceCandidate"),
        }
    }

    #[test]
    fn signal_json_format() {
        let msg = SignalMessage::Offer {
            sdp: "test_sdp".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            json.contains("\"Offer\""),
            "variant tag should appear in JSON"
        );
        assert!(json.contains("\"sdp\""), "field name should appear in JSON");
        assert!(json.contains("test_sdp"), "sdp value should appear in JSON");
    }
}
