//! SFU voice send path while the command loop is held (plans/voice-quality.md
//! stage 2, spec 10 section 4.5).
//!
//! A fake SFU answers the client's WebRTC offer with a native libmello peer
//! and records each voice RTP packet it receives, with its arrival time. The
//! client runs its real command loop with a libmello context on the
//! device-free audio backend. A test thread injects microphone frames as the
//! capture thread does, while `Command::TestHoldLoop` blocks the loop thread.
//! On this current-thread runtime the hold also stops every Tokio task, so
//! only native threads can carry a frame to the peer during it.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use super::loop_hold;
use super::Client;
use crate::command::Command;
use crate::config::Config;
use crate::crew_state::VoiceJoinResponse;
use crate::events::Event;
use crate::voice::send_sink::trace::{self, SinkCall};
use crate::voice::{VoiceManager, VoiceMode};

const LIMIT: Duration = Duration::from_secs(20);
const FRAME: usize = 960;
/// 10 ms capture chunks, as a device callback delivers them.
const CHUNK: usize = 480;

/// Voice RTP packets the fake SFU received: 16-bit RTP sequence and arrival.
type Arrivals = Arc<Mutex<Vec<(u16, Instant)>>>;
type Candidates = Arc<Mutex<Vec<(String, String, i32)>>>;

/// Native state of the fake SFU. The test destroys the peer at the end.
struct FakeSfuPeer {
    peer: usize,
    arrivals: *const Mutex<Vec<(u16, Instant)>>,
    candidates: *const Mutex<Vec<(String, String, i32)>>,
}

impl FakeSfuPeer {
    /// Destroy the native peer, then free its callback state.
    fn destroy(self) {
        // SAFETY: the peer came from mello_peer_create and is destroyed
        // once; its callbacks no longer run after destroy returns, so the
        // Arc counts handed to them can be released.
        unsafe {
            mello_sys::mello_peer_destroy(self.peer as *mut mello_sys::MelloPeerConnection);
            drop(Arc::from_raw(self.arrivals));
            drop(Arc::from_raw(self.candidates));
        }
    }
}

// SAFETY: the pointers are only used by `destroy`, after the task that
// created them has handed them over.
unsafe impl Send for FakeSfuPeer {}

struct MediaSfu {
    endpoint: String,
    arrivals: Arrivals,
    peer: tokio::sync::oneshot::Receiver<FakeSfuPeer>,
}

unsafe extern "C" fn on_sfu_audio(
    user_data: *mut std::ffi::c_void,
    _sender_id: *const std::ffi::c_char,
    data: *const u8,
    size: i32,
) {
    let at = Instant::now();
    if user_data.is_null() || data.is_null() || size < 4 {
        return;
    }
    let arrivals = &*(user_data as *const Mutex<Vec<(u16, Instant)>>);
    // libmello writes the 16-bit RTP sequence little endian into the header.
    let seq = u16::from_le_bytes([*data, *data.add(1)]);
    if let Ok(mut a) = arrivals.lock() {
        a.push((seq, at));
    }
}

unsafe extern "C" fn on_sfu_candidate(
    user_data: *mut std::ffi::c_void,
    candidate: *const mello_sys::MelloIceCandidate,
) {
    if user_data.is_null() || candidate.is_null() {
        return;
    }
    let queue = &*(user_data as *const Mutex<Vec<(String, String, i32)>>);
    let c = &*candidate;
    let cand = CStr::from_ptr(c.candidate).to_string_lossy().into_owned();
    let mid = CStr::from_ptr(c.sdp_mid).to_string_lossy().into_owned();
    if let Ok(mut q) = queue.lock() {
        q.push((cand, mid, c.sdp_mline_index));
    }
}

fn text(value: serde_json::Value) -> Message {
    Message::Text(value.to_string())
}

async fn next_text(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> serde_json::Value {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => {
                return serde_json::from_str(&t).expect("the client sends JSON");
            }
            Some(Ok(_)) => continue,
            other => panic!("signaling ended: {other:?}"),
        }
    }
}

/// An SFU that joins one voice client and receives its RTP audio on a native
/// libmello peer. The SFU relays ICE candidates both ways.
async fn media_sfu() -> MediaSfu {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local port");
    let endpoint = format!(
        "ws://{}/ws",
        listener.local_addr().expect("listener has an address")
    );
    let arrivals: Arrivals = Arc::new(Mutex::new(Vec::new()));
    let task_arrivals = Arc::clone(&arrivals);
    let (peer_tx, peer_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("the client connects");
        let mut ws = tokio_tungstenite::accept_async(socket)
            .await
            .expect("websocket handshake");
        ws.send(text(serde_json::json!({
            "type": "welcome",
            "data": { "server_id": "fake", "region": "test" }
        })))
        .await
        .expect("send welcome");

        let join = next_text(&mut ws).await;
        assert_eq!(join["type"], "join_voice", "{join}");
        ws.send(text(serde_json::json!({
            "type": "joined",
            "data": { "session_type": "voice", "session_id": "voice:crew-1:ch-1", "members": [] }
        })))
        .await
        .expect("send joined");

        let mut early_candidates = Vec::new();
        let offer = loop {
            let msg = next_text(&mut ws).await;
            match msg["type"].as_str() {
                Some("offer") => break msg["data"]["sdp"].as_str().expect("sdp").to_string(),
                Some("ice_candidate") => early_candidates.push(msg),
                _ => {}
            }
        };
        // The real SFU forwards a member's audio with the member's user id
        // as msid. libmello accepts an incoming voice track only with such an
        // id, so give the client's track one.
        let offer = offer.replace("msid:sfu ", "msid:client-uplink ");

        let candidates: Candidates = Arc::new(Mutex::new(Vec::new()));
        let id = CString::new("fake-sfu").expect("static id");
        let offer_c = CString::new(offer).expect("offer has no NUL");
        // SAFETY: libmello ignores the context argument; the callback state
        // stays alive until FakeSfuPeer::destroy, after the peer is gone.
        let (fake, answer) = unsafe {
            let arrivals_ptr = Arc::into_raw(Arc::clone(&task_arrivals));
            let candidates_ptr = Arc::into_raw(Arc::clone(&candidates));
            let peer = mello_sys::mello_peer_create(std::ptr::null_mut(), id.as_ptr());
            assert!(!peer.is_null(), "fake SFU peer");
            mello_sys::mello_peer_set_ice_callback(
                peer,
                Some(on_sfu_candidate),
                candidates_ptr as *mut std::ffi::c_void,
            );
            mello_sys::mello_peer_set_audio_track_callback(
                peer,
                Some(on_sfu_audio),
                arrivals_ptr as *mut std::ffi::c_void,
            );
            let answer = mello_sys::mello_peer_create_answer(peer, offer_c.as_ptr());
            assert!(!answer.is_null(), "fake SFU answer");
            let fake = FakeSfuPeer {
                peer: peer as usize,
                arrivals: arrivals_ptr,
                candidates: candidates_ptr,
            };
            (fake, CStr::from_ptr(answer).to_string_lossy().into_owned())
        };
        let peer_addr = fake.peer;
        let add_candidate = |msg: &serde_json::Value| {
            let cand = msg["data"]["candidate"].as_str().unwrap_or("");
            let mid = msg["data"]["sdp_mid"].as_str().unwrap_or("0");
            let cand = CString::new(cand).expect("candidate");
            let mid = CString::new(mid).expect("mid");
            let c = mello_sys::MelloIceCandidate {
                candidate: cand.as_ptr(),
                sdp_mid: mid.as_ptr(),
                sdp_mline_index: msg["data"]["sdp_mline_index"].as_i64().unwrap_or(0) as i32,
            };
            // SAFETY: the peer is live; the strings outlive the call.
            unsafe {
                mello_sys::mello_peer_add_ice_candidate(
                    peer_addr as *mut mello_sys::MelloPeerConnection,
                    &c,
                )
            };
        };
        for msg in &early_candidates {
            add_candidate(msg);
        }
        ws.send(text(
            serde_json::json!({ "type": "answer", "data": { "sdp": answer } }),
        ))
        .await
        .expect("send answer");
        let _ = peer_tx.send(fake);

        let mut relay = tokio::time::interval(Duration::from_millis(10));
        loop {
            tokio::select! {
                msg = ws.next() => match msg {
                    Some(Ok(Message::Text(t))) => {
                        let v: serde_json::Value = serde_json::from_str(&t).expect("JSON");
                        if v["type"] == "ice_candidate" {
                            add_candidate(&v);
                        }
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                },
                _ = relay.tick() => {
                    let pending: Vec<_> = candidates.lock().expect("candidates").drain(..).collect();
                    for (candidate, mid, idx) in pending {
                        let _ = ws.send(text(serde_json::json!({
                            "type": "ice_candidate",
                            "data": { "candidate": candidate, "sdp_mid": mid, "sdp_mline_index": idx }
                        }))).await;
                    }
                }
            }
        }
    });
    MediaSfu {
        endpoint,
        arrivals,
        peer: peer_rx,
    }
}

fn client_with_test_audio() -> (Client, mpsc::Receiver<Event>) {
    let (event_tx, events) = mpsc::channel();
    let voice = VoiceManager::with_test_audio_backend(event_tx.clone());
    let client = Client::with_voice(
        Config::default(),
        event_tx,
        voice,
        Arc::new(std::sync::Mutex::new(None)),
        Arc::new(std::sync::Mutex::new(None)),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(std::sync::atomic::AtomicU8::new(0)),
        false,
        false,
    );
    (client, events)
}

fn sfu_join_response(endpoint: &str) -> VoiceJoinResponse {
    serde_json::from_value(serde_json::json!({
        "channel_id": "ch-1",
        "voice_state": {
            "channel_id": "ch-1",
            "members": [{ "user_id": "me" }, { "user_id": "peer-1" }],
        },
        "mode": "sfu",
        "sfu_endpoint": endpoint,
        "sfu_token": "token",
    }))
    .expect("valid voice_join response")
}

async fn wait_for_event(
    events: &mpsc::Receiver<Event>,
    what: &str,
    pick: impl Fn(&Event) -> bool,
) -> Vec<Event> {
    let deadline = tokio::time::Instant::now() + LIMIT;
    let mut before = Vec::new();
    loop {
        while let Ok(ev) = events.try_recv() {
            if pick(&ev) {
                return before;
            }
            before.push(ev);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no {what} event; saw {before:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The microphone: injects 20 ms frames of a tone into the context, from
/// whatever thread calls it, as the capture thread delivers them.
#[derive(Clone, Copy)]
struct Mic {
    ctx: usize,
}

impl Mic {
    fn frame(&self, index: u64) {
        let mut chunk = [0i16; CHUNK];
        for half in 0..2u64 {
            for (i, s) in chunk.iter_mut().enumerate() {
                let n = index * FRAME as u64 + half * CHUNK as u64 + i as u64;
                let t = n as f64 / 48_000.0;
                *s = (6000.0 * (2.0 * std::f64::consts::PI * 300.0 * t).sin()) as i16;
            }
            // SAFETY: the context lives as long as the client; libmello
            // copies the samples.
            unsafe {
                mello_sys::mello_voice_inject_capture(
                    self.ctx as *mut mello_sys::MelloContext,
                    chunk.as_ptr(),
                    CHUNK as i32,
                )
            };
        }
    }
}

/// Wait on a native thread until `done` or the deadline.
fn wait_until(deadline: Instant, mut done: impl FnMut() -> bool) -> bool {
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    done()
}

/// One frame of the measurement: the sink call (libmello encode done, send
/// returned) and the arrival at the SFU peer.
#[derive(Clone, Copy)]
struct FrameTiming {
    call: SinkCall,
    arrived: Instant,
}

/// Result of one measurement window.
struct Window {
    frames: Vec<FrameTiming>,
    /// Arrivals that came while the loop was still held (hold windows only).
    arrived_while_held: Option<bool>,
}

/// Inject `frames` frames, `pace` apart, starting at frame index `first`,
/// then pair each sink call with its arrival at the peer. Runs on a native
/// thread. With `hold`, it starts when that hold starts and checks that all
/// frames arrived before the hold ended.
fn measure(
    mic: Mic,
    calls: trace::Calls,
    arrivals: Arrivals,
    first: u64,
    frames: usize,
    pace: Duration,
    hold: Option<u64>,
) -> Window {
    let start_wait = Instant::now() + LIMIT;
    if let Some(token) = hold {
        assert!(
            wait_until(start_wait, || loop_hold::window(token).is_some()),
            "the loop hold did not start"
        );
    }
    let calls_before = calls.lock().expect("calls").len();
    let arrivals_before = arrivals.lock().expect("arrivals").len();

    let mut next = Instant::now();
    for i in 0..frames as u64 {
        mic.frame(first + i);
        next += pace;
        if let Some(sleep) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(sleep);
        }
    }
    let all_arrived = wait_until(Instant::now() + Duration::from_secs(2), || {
        arrivals.lock().expect("arrivals").len() >= arrivals_before + frames
    });
    let arrived_while_held = hold
        .map(|token| all_arrived && loop_hold::window(token).is_some_and(|w| w.ended.is_none()));
    if arrived_while_held == Some(false) {
        panic!(
            "{} of {frames} frames reached the SFU peer while the loop was held",
            arrivals.lock().expect("arrivals").len() - arrivals_before
        );
    }

    let calls: Vec<SinkCall> = calls.lock().expect("calls")[calls_before..].to_vec();
    let got: Vec<(u16, Instant)> = arrivals.lock().expect("arrivals")[arrivals_before..].to_vec();
    assert_eq!(calls.len(), frames, "one sink call per injected frame");
    assert!(calls.iter().all(|c| c.sent), "the peer skipped a frame");
    assert!(
        calls
            .windows(2)
            .all(|w| w[1].sequence == w[0].sequence.wrapping_add(1)),
        "the encoder skipped a sequence"
    );
    assert_eq!(
        got.len(),
        frames,
        "frames that reached the SFU peer in the window"
    );
    // Frames leave in encode order on one thread, and loopback UDP keeps the
    // order: the k-th call is the k-th RTP sequence after the first.
    let by_seq: HashMap<u16, Instant> = got.iter().copied().collect();
    let seq0 = got[0].0;
    let frames = calls
        .iter()
        .enumerate()
        .map(|(k, call)| FrameTiming {
            call: *call,
            arrived: by_seq[&seq0.wrapping_add(k as u16)],
        })
        .collect();
    Window {
        frames,
        arrived_while_held,
    }
}

struct Percentiles {
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

fn percentiles(mut ms: Vec<f64>) -> Percentiles {
    ms.sort_by(f64::total_cmp);
    let at = |p: f64| ms[((ms.len() - 1) as f64 * p).round() as usize];
    Percentiles {
        p50: at(0.50),
        p95: at(0.95),
        p99: at(0.99),
        max: *ms.last().expect("samples"),
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn report(label: &str, w: &Window) -> (Percentiles, Percentiles) {
    let send = percentiles(
        w.frames
            .iter()
            .map(|f| ms(f.call.returned - f.call.entered))
            .collect(),
    );
    let wire = percentiles(
        w.frames
            .iter()
            .map(|f| ms(f.arrived.saturating_duration_since(f.call.entered)))
            .collect(),
    );
    for (what, p) in [("encode-to-send", &send), ("encode-to-wire", &wire)] {
        println!(
            "[send-path] {label:<12} {what:<15} frames={} p50={:.3} p95={:.3} p99={:.3} max={:.3} ms",
            w.frames.len(),
            p.p50,
            p.p95,
            p.p99,
            p.max
        );
    }
    (send, wire)
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Join the fake SFU through the real loop, then run `body` on a native
/// thread while the loop runs. `body` gets the command sender for holds.
async fn with_sfu_voice<T: Send + 'static>(
    body: impl FnOnce(Mic, trace::Calls, Arrivals, tokio::sync::mpsc::UnboundedSender<Command>) -> T
        + Send
        + 'static,
) -> T {
    let sfu = media_sfu().await;
    let (mut client, events) = client_with_test_audio();
    let mic = Mic {
        ctx: client.voice.mello_ctx() as usize,
    };
    let calls = trace::for_context(mic.ctx);
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();

    client
        .start_voice_media("crew-1", Some("me"), &sfu_join_response(&sfu.endpoint))
        .await;
    let arrivals = Arc::clone(&sfu.arrivals);
    let driver = async move {
        wait_for_event(&events, "VoiceStateChanged { in_call: true }", |ev| {
            matches!(ev, Event::VoiceStateChanged { in_call: true, .. })
        })
        .await;
        // Push-to-talk: every unmuted frame is encoded, so each injected
        // frame is one packet.
        cmd_tx
            .send(Command::SetPushToTalk { enabled: true })
            .expect("the loop listens");
        cmd_tx
            .send(Command::StartVoiceCaptureInject)
            .expect("the loop listens");
        cmd_tx
            .send(Command::ListAudioDevices)
            .expect("the loop listens");
        wait_for_event(&events, "AudioDevicesListed", |ev| {
            matches!(ev, Event::AudioDevicesListed { .. })
        })
        .await;

        // The send track opens shortly after the connection. Inject until a
        // frame reaches the SFU peer, then let the stragglers land.
        let mut index = 0u64;
        while sfu.arrivals.lock().expect("arrivals").is_empty() {
            assert!(index < 1000, "no voice frame reached the SFU peer");
            mic.frame(index);
            index += 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;

        let tx = cmd_tx.clone();
        let result = tokio::task::spawn_blocking(move || body(mic, calls, arrivals, tx))
            .await
            .expect("the measurement thread");
        drop(cmd_tx);
        result
    };
    let (_, result) = tokio::time::timeout(Duration::from_secs(120), async {
        tokio::join!(client.run(cmd_rx), driver)
    })
    .await
    .expect("the loop ends when the command channel closes");
    assert_eq!(
        client.voice.voice_mode(),
        VoiceMode::SFU,
        "voice must run over the fake SFU, not the P2P fallback"
    );
    drop(client);
    if let Ok(peer) = sfu.peer.await {
        peer.destroy();
    }
    result
}

/// Regression (stage 2): with the command loop held, encoded voice frames
/// still reach the SFU peer. Before the packet sink, they waited in
/// libmello's queue until the loop's voice tick ran again.
#[tokio::test]
async fn sfu_voice_frames_reach_the_peer_while_the_loop_is_held() {
    let window = with_sfu_voice(|mic, calls, arrivals, cmd_tx| {
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        cmd_tx
            .send(Command::TestHoldLoop { token, ms: 3000 })
            .expect("the loop listens");
        measure(
            mic,
            calls,
            arrivals,
            10_000,
            50,
            Duration::from_millis(10),
            Some(token),
        )
    })
    .await;
    report("loop held", &window);
    assert_eq!(
        window.arrived_while_held,
        Some(true),
        "the frames reached the SFU peer only after the loop was released"
    );
}

/// The mic test during an SFU call: the frames leave from the capture
/// thread, and the sink also feeds them to the local decoder as the
/// `loopback` peer, as the voice tick did before.
#[tokio::test]
async fn the_mic_test_in_an_sfu_call_hears_the_microphone() {
    let received = with_sfu_voice(|mic, _calls, _arrivals, cmd_tx| {
        let rtp_recv_total = || {
            // SAFETY: zeroed is a valid MelloDebugStats; the context is live
            // while the loop runs.
            unsafe {
                let mut s: mello_sys::MelloDebugStats = std::mem::zeroed();
                mello_sys::mello_get_debug_stats(mic.ctx as *mut mello_sys::MelloContext, &mut s);
                s.rtp_recv_total
            }
        };
        // The fake SFU sends no audio, so only the mic test feeds the decoder.
        assert_eq!(rtp_recv_total(), 0);
        cmd_tx
            .send(Command::SetLoopback { enabled: true })
            .expect("the loop listens");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut index = 30_000u64;
        while rtp_recv_total() == 0 && Instant::now() < deadline {
            mic.frame(index);
            index += 1;
            std::thread::sleep(Duration::from_millis(20));
        }
        rtp_recv_total()
    })
    .await;
    assert!(received > 0, "the mic test fed no frame to the decoder");
}

/// The stage 2 gate measurement: 1000 frames with the loop idle, then 1000
/// frames while one command holds the loop for 11 s (the 10 s of frames
/// fit inside it). Encode-to-send is the
/// time from libmello's sink call (right after Opus encode) until
/// `mello_peer_send_audio_frame` returned. Encode-to-wire ends when the
/// fake SFU's native peer received the RTP packet. Frames are paced 10 ms
/// apart so that 1000 fit in the 10 s hold.
///
/// Run: `cargo test -p mello-core --lib send_path_latency -- --ignored --nocapture`
#[tokio::test]
#[ignore = "about 30 s; the stage 2 gate measurement, run on demand"]
async fn send_path_latency_with_the_loop_idle_and_held() {
    const FRAMES: usize = 1000;
    let (idle, held) = with_sfu_voice(|mic, calls, arrivals, cmd_tx| {
        let idle = measure(
            mic,
            Arc::clone(&calls),
            Arc::clone(&arrivals),
            10_000,
            FRAMES,
            Duration::from_millis(10),
            None,
        );
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        cmd_tx
            .send(Command::TestHoldLoop { token, ms: 11_000 })
            .expect("the loop listens");
        let held = measure(
            mic,
            calls,
            arrivals,
            20_000,
            FRAMES,
            Duration::from_millis(10),
            Some(token),
        );
        let w = loop_hold::window(token).expect("the hold ran");
        let first = held.frames.first().expect("frames").call.entered;
        let last = held.frames.last().expect("frames").arrived;
        assert!(
            first >= w.started && w.ended.is_none_or(|end| last <= end),
            "the held window did not cover the measurement"
        );
        (idle, held)
    })
    .await;

    let (_, idle_wire) = report("loop idle", &idle);
    let (held_send, held_wire) = report("loop held", &held);
    assert_eq!(held.arrived_while_held, Some(true));
    assert!(
        held_send.p99 < 2.0 && held_wire.p99 < 2.0,
        "stage 2 gate: encode-to-send p99 {:.3} ms, encode-to-wire p99 {:.3} ms (limit 2 ms)",
        held_send.p99,
        held_wire.p99
    );
    assert!(
        idle_wire.p99 < 2.0,
        "idle encode-to-wire p99 {:.3} ms",
        idle_wire.p99
    );
}
