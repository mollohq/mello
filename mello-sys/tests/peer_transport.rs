//! Integration test: two local peers exchange SDP, connect, and transfer
//! data packets through unreliable data channels — no audio hardware needed.

use std::ffi::{CStr, CString};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

struct IceRelay {
    candidates: Vec<(String, String, i32)>,
}

static RELAY_A_TO_B: std::sync::OnceLock<Arc<Mutex<IceRelay>>> = std::sync::OnceLock::new();
static RELAY_B_TO_A: std::sync::OnceLock<Arc<Mutex<IceRelay>>> = std::sync::OnceLock::new();

unsafe extern "C" fn ice_cb_a(_ud: *mut std::ffi::c_void, candidate: *const mello_sys::MelloIceCandidate) {
    if candidate.is_null() { return; }
    let c = &*candidate;
    let cand = CStr::from_ptr(c.candidate).to_string_lossy().into_owned();
    let mid = CStr::from_ptr(c.sdp_mid).to_string_lossy().into_owned();
    let idx = c.sdp_mline_index;
    if let Some(relay) = RELAY_A_TO_B.get() {
        relay.lock().unwrap().candidates.push((cand, mid, idx));
    }
}

unsafe extern "C" fn ice_cb_b(_ud: *mut std::ffi::c_void, candidate: *const mello_sys::MelloIceCandidate) {
    if candidate.is_null() { return; }
    let c = &*candidate;
    let cand = CStr::from_ptr(c.candidate).to_string_lossy().into_owned();
    let mid = CStr::from_ptr(c.sdp_mid).to_string_lossy().into_owned();
    let idx = c.sdp_mline_index;
    if let Some(relay) = RELAY_B_TO_A.get() {
        relay.lock().unwrap().candidates.push((cand, mid, idx));
    }
}

fn drain_candidates(
    relay: &Arc<Mutex<IceRelay>>,
    target_peer: *mut mello_sys::MelloPeerConnection,
) {
    let candidates: Vec<_> = {
        let mut r = relay.lock().unwrap();
        r.candidates.drain(..).collect()
    };
    for (cand, mid, idx) in candidates {
        let mc = mello_sys::MelloIceCandidate {
            candidate: CString::new(cand).unwrap().into_raw(),
            sdp_mid: CString::new(mid).unwrap().into_raw(),
            sdp_mline_index: idx,
        };
        unsafe { mello_sys::mello_peer_add_ice_candidate(target_peer, &mc); }
        // Reclaim the CStrings
        unsafe {
            let _ = CString::from_raw(mc.candidate as *mut _);
            let _ = CString::from_raw(mc.sdp_mid as *mut _);
        }
    }
}

#[test]
fn two_peers_exchange_packets() {
    let relay_a_to_b = RELAY_A_TO_B.get_or_init(|| Arc::new(Mutex::new(IceRelay { candidates: vec![] })));
    let relay_b_to_a = RELAY_B_TO_A.get_or_init(|| Arc::new(Mutex::new(IceRelay { candidates: vec![] })));

    let ctx_a = unsafe { mello_sys::mello_init() };
    let ctx_b = unsafe { mello_sys::mello_init() };
    assert!(!ctx_a.is_null(), "context A init failed");
    assert!(!ctx_b.is_null(), "context B init failed");

    let id_a = CString::new("peer_a").unwrap();
    let id_b = CString::new("peer_b").unwrap();

    let peer_a = unsafe { mello_sys::mello_peer_create(ctx_a, id_a.as_ptr()) };
    let peer_b = unsafe { mello_sys::mello_peer_create(ctx_b, id_b.as_ptr()) };
    assert!(!peer_a.is_null(), "peer A create failed");
    assert!(!peer_b.is_null(), "peer B create failed");

    // Set ICE callbacks to relay candidates between peers
    unsafe {
        mello_sys::mello_peer_set_ice_callback(peer_a, Some(ice_cb_a), std::ptr::null_mut());
        mello_sys::mello_peer_set_ice_callback(peer_b, Some(ice_cb_b), std::ptr::null_mut());
    }

    // A creates offer
    let offer_ptr = unsafe { mello_sys::mello_peer_create_offer(peer_a) };
    assert!(!offer_ptr.is_null(), "offer creation failed");
    let offer_sdp = unsafe { CStr::from_ptr(offer_ptr) }
        .to_str()
        .expect("invalid offer SDP");
    assert!(offer_sdp.contains("v=0"), "offer should contain SDP");

    // B creates answer from A's offer
    let offer_c = CString::new(offer_sdp).unwrap();
    let answer_ptr = unsafe { mello_sys::mello_peer_create_answer(peer_b, offer_c.as_ptr()) };
    assert!(!answer_ptr.is_null(), "answer creation failed");
    let answer_sdp = unsafe { CStr::from_ptr(answer_ptr) }
        .to_str()
        .expect("invalid answer SDP");
    assert!(answer_sdp.contains("v=0"), "answer should contain SDP");

    // A sets B's answer as remote description
    let answer_c = CString::new(answer_sdp).unwrap();
    let result = unsafe {
        mello_sys::mello_peer_set_remote_description(peer_a, answer_c.as_ptr(), false)
    };
    assert_eq!(result, mello_sys::MelloResult_MELLO_OK, "set remote desc failed");

    // Exchange ICE candidates and wait for connection
    let mut connected = false;
    for _ in 0..100 {
        // Relay A's candidates to B, and B's candidates to A
        drain_candidates(relay_a_to_b, peer_b);
        drain_candidates(relay_b_to_a, peer_a);

        let a_conn = unsafe { mello_sys::mello_peer_is_connected(peer_a) };
        let b_conn = unsafe { mello_sys::mello_peer_is_connected(peer_b) };
        if a_conn && b_conn {
            connected = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(connected, "peers did not connect within 10 seconds");

    // A sends a packet to B via unreliable channel
    let test_data: Vec<u8> = (0..100).map(|i| (i % 256) as u8).collect();
    let send_result = unsafe {
        mello_sys::mello_peer_send_unreliable(peer_a, test_data.as_ptr(), test_data.len() as i32)
    };
    assert_eq!(send_result, mello_sys::MelloResult_MELLO_OK, "send failed");

    thread::sleep(Duration::from_millis(500));

    // B polls for the received packet
    let mut recv_buf = [0u8; 4000];
    let recv_size = unsafe {
        mello_sys::mello_peer_recv(peer_b, recv_buf.as_mut_ptr(), recv_buf.len() as i32)
    };
    assert!(recv_size > 0, "peer B received no data (size={})", recv_size);
    assert_eq!(recv_size as usize, test_data.len(), "received size mismatch");
    assert_eq!(&recv_buf[..recv_size as usize], &test_data[..], "received data mismatch");

    // B sends back to A
    let reply: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let send_result = unsafe {
        mello_sys::mello_peer_send_unreliable(peer_b, reply.as_ptr(), reply.len() as i32)
    };
    assert_eq!(send_result, mello_sys::MelloResult_MELLO_OK, "reply send failed");

    thread::sleep(Duration::from_millis(500));

    let recv_size = unsafe {
        mello_sys::mello_peer_recv(peer_a, recv_buf.as_mut_ptr(), recv_buf.len() as i32)
    };
    assert_eq!(recv_size as usize, reply.len(), "reply size mismatch");
    assert_eq!(&recv_buf[..recv_size as usize], &reply[..], "reply data mismatch");

    // Cleanup
    unsafe {
        mello_sys::mello_peer_destroy(peer_a);
        mello_sys::mello_peer_destroy(peer_b);
        mello_sys::mello_destroy(ctx_a);
        mello_sys::mello_destroy(ctx_b);
    }
}

#[test]
fn context_init_destroy() {
    let ctx = unsafe { mello_sys::mello_init() };
    assert!(!ctx.is_null());
    unsafe { mello_sys::mello_destroy(ctx); }
}

#[test]
fn peer_null_safety() {
    assert!(unsafe { mello_sys::mello_peer_create_offer(std::ptr::null_mut()) }.is_null());
    assert!(unsafe {
        mello_sys::mello_peer_create_answer(std::ptr::null_mut(), std::ptr::null())
    }.is_null());
    assert!(!unsafe { mello_sys::mello_peer_is_connected(std::ptr::null_mut()) });

    let mut buf = [0u8; 100];
    assert_eq!(
        unsafe { mello_sys::mello_peer_recv(std::ptr::null_mut(), buf.as_mut_ptr(), 100) },
        0
    );
}

#[test]
fn peer_create_destroy_many() {
    let ctx = unsafe { mello_sys::mello_init() };
    assert!(!ctx.is_null());

    let mut peers = Vec::new();
    for i in 0..5 {
        let id = CString::new(format!("peer_{}", i)).unwrap();
        let peer = unsafe { mello_sys::mello_peer_create(ctx, id.as_ptr()) };
        assert!(!peer.is_null(), "peer {} create failed", i);
        peers.push(peer);
    }

    for peer in peers {
        unsafe { mello_sys::mello_peer_destroy(peer); }
    }
    unsafe { mello_sys::mello_destroy(ctx); }
}

#[test]
fn voice_null_context_safety() {
    let null_ctx: *mut mello_sys::MelloContext = std::ptr::null_mut();
    unsafe {
        let _ = mello_sys::mello_voice_start_capture(null_ctx);
        let _ = mello_sys::mello_voice_stop_capture(null_ctx);
        mello_sys::mello_voice_set_mute(null_ctx, true);
        mello_sys::mello_voice_set_deafen(null_ctx, true);
        assert!(!mello_sys::mello_voice_is_speaking(null_ctx));
        assert_eq!(mello_sys::mello_voice_get_input_level(null_ctx), 0.0);

        let mut buf = [0u8; 100];
        assert_eq!(mello_sys::mello_voice_get_packet(null_ctx, buf.as_mut_ptr(), 100), 0);

        let peer_id = CString::new("test").unwrap();
        let _ = mello_sys::mello_voice_feed_packet(null_ctx, peer_id.as_ptr(), buf.as_ptr(), 10);
    }
}
