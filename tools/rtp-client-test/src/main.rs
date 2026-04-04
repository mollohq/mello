// rtp-client-test: minimal libdatachannel client that connects to rtp-sfu-mini
// to verify RTP track reception during renegotiation.
//
// Connects to the Go mini-SFU via TCP, exchanges SDP, sends audio,
// handles renegotiation, and reports whether it receives audio on the new track.

use std::ffi::{c_void, CStr, CString};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(serde::Serialize, serde::Deserialize)]
struct SigMsg {
    r#type: String,
    sdp: String,
}

struct CallbackState {
    audio_packets: AtomicU64,
    audio_bytes: AtomicU64,
}

unsafe extern "C" fn on_state(user_data: *mut c_void, state: i32) {
    let _ = user_data;
    let label = match state {
        0 => "new",
        1 => "connecting",
        2 => "connected",
        3 => "disconnected",
        4 => "failed",
        5 => "closed",
        _ => "unknown",
    };
    eprintln!("[client] Connection state: {} ({})", label, state);
}

unsafe extern "C" fn on_audio_track(
    user_data: *mut c_void,
    sender_id: *const i8,
    _data: *const u8,
    size: i32,
) {
    let state = &*(user_data as *const CallbackState);
    let sender = if sender_id.is_null() {
        "<null>"
    } else {
        CStr::from_ptr(sender_id).to_str().unwrap_or("<invalid>")
    };

    let count = state.audio_packets.fetch_add(1, Ordering::Relaxed);
    state.audio_bytes.fetch_add(size as u64, Ordering::Relaxed);

    if count < 5 || count.is_multiple_of(50) {
        eprintln!(
            "[client] Audio track data: sender={} size={} total_pkts={}",
            sender,
            size,
            count + 1
        );
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9999".to_string());

    eprintln!("=== RTP Client Test (libdatachannel) ===\n");

    // Init mello
    let ctx = unsafe { mello_sys::mello_init() };
    if ctx.is_null() {
        eprintln!("ERROR: mello_init() failed");
        std::process::exit(1);
    }

    let peer_id = CString::new("test-peer").unwrap();
    let peer = unsafe { mello_sys::mello_peer_create(ctx, peer_id.as_ptr()) };
    if peer.is_null() {
        eprintln!("ERROR: mello_peer_create() failed");
        std::process::exit(1);
    }

    // Set up callbacks
    let cb_state = Arc::new(CallbackState {
        audio_packets: AtomicU64::new(0),
        audio_bytes: AtomicU64::new(0),
    });

    unsafe {
        mello_sys::mello_peer_set_state_callback(peer, Some(on_state), std::ptr::null_mut());
        mello_sys::mello_peer_set_audio_track_callback(
            peer,
            Some(on_audio_track),
            Arc::as_ptr(&cb_state) as *mut c_void,
        );
    }

    // Connect to SFU
    eprintln!("[step 1] Connecting to SFU at {}...", addr);
    let stream = TcpStream::connect(&addr).expect("failed to connect to SFU");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;

    let send = |w: &mut TcpStream, msg: &SigMsg| {
        let data = serde_json::to_string(msg).unwrap();
        w.write_all(data.as_bytes()).unwrap();
        w.write_all(b"\n").unwrap();
        w.flush().unwrap();
    };

    let recv = |r: &mut BufReader<TcpStream>| -> SigMsg {
        let mut line = String::new();
        r.read_line(&mut line).expect("failed to read from SFU");
        serde_json::from_str(&line).expect("failed to parse SFU message")
    };

    // Create offer
    eprintln!("[step 1] Creating offer...");
    let offer_sdp = unsafe {
        let ptr = mello_sys::mello_peer_create_offer(peer);
        if ptr.is_null() {
            eprintln!("ERROR: mello_peer_create_offer() returned null");
            std::process::exit(1);
        }
        CStr::from_ptr(ptr).to_string_lossy().to_string()
    };
    eprintln!("[step 1] Offer created ({} bytes)", offer_sdp.len());

    send(
        &mut writer,
        &SigMsg {
            r#type: "offer".into(),
            sdp: offer_sdp,
        },
    );

    // Receive answer
    let answer_msg = recv(&mut reader);
    assert_eq!(answer_msg.r#type, "answer");
    eprintln!(
        "[step 1] Got answer ({} bytes), applying...",
        answer_msg.sdp.len()
    );

    let answer_cstr = CString::new(answer_msg.sdp).unwrap();
    let result =
        unsafe { mello_sys::mello_peer_set_remote_description(peer, answer_cstr.as_ptr(), false) };
    eprintln!("[step 1] set_remote_description result: {}", result);

    // Wait for connection + send some test audio
    eprintln!("\n[step 2] Waiting for connection, sending test audio...");
    let audio_data = [0u8; 80]; // silence opus frame
    for i in 0..50 {
        unsafe {
            mello_sys::mello_peer_send_audio(peer, audio_data.as_ptr(), audio_data.len() as i32);
        }
        if (i + 1) % 25 == 0 {
            eprintln!("[client] Sent {} audio packets", i + 1);
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Wait for renegotiation offer from SFU
    eprintln!("\n[step 3] Waiting for renegotiation offer from SFU...");
    let reneg_msg = recv(&mut reader);
    assert_eq!(reneg_msg.r#type, "offer");
    eprintln!(
        "[step 3] Got renegotiation offer ({} bytes)",
        reneg_msg.sdp.len()
    );

    let offer_cstr = CString::new(reneg_msg.sdp).unwrap();
    let answer_ptr =
        unsafe { mello_sys::mello_peer_handle_remote_offer(peer, offer_cstr.as_ptr()) };
    if answer_ptr.is_null() {
        eprintln!("ERROR: mello_peer_handle_remote_offer() returned null");
        std::process::exit(1);
    }
    let answer_sdp = unsafe { CStr::from_ptr(answer_ptr).to_string_lossy().to_string() };
    eprintln!(
        "[step 3] Renegotiation answer created ({} bytes)",
        answer_sdp.len()
    );

    send(
        &mut writer,
        &SigMsg {
            r#type: "answer".into(),
            sdp: answer_sdp,
        },
    );

    // Wait and count received audio packets
    eprintln!("\n[step 4] Waiting for RTP audio from SFU (6 seconds)...");
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let pkts = cb_state.audio_packets.load(Ordering::Relaxed);
        let bytes = cb_state.audio_bytes.load(Ordering::Relaxed);
        let elapsed = start.elapsed().as_secs();
        eprintln!(
            "[client] t={}s  audio_pkts={}  audio_bytes={}",
            elapsed, pkts, bytes
        );
        if elapsed >= 6 {
            break;
        }
    }

    let total_pkts = cb_state.audio_packets.load(Ordering::Relaxed);
    let total_bytes = cb_state.audio_bytes.load(Ordering::Relaxed);

    eprintln!("\n=== Results ===");
    eprintln!("Audio packets received: {}", total_pkts);
    eprintln!("Audio bytes received:   {}", total_bytes);

    if total_pkts > 0 {
        eprintln!("\n✓ RTP audio received through libdatachannel!");
        eprintln!("  The renegotiation + track reception works.");
    } else {
        eprintln!("\n✗ No RTP audio received.");
        eprintln!("  libdatachannel did not deliver audio on the renegotiated track.");
    }

    // Cleanup
    unsafe {
        mello_sys::mello_peer_destroy(peer);
        mello_sys::mello_destroy(ctx);
    }
}
