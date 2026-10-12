//! Stage 4 gate measurements against a real SFU (plans/voice-quality.md
//! stage 4): join latency and speaking latency through receive slots.
//!
//! Ignored by default: it needs a running mello-sfu in test-password mode.
//!
//! ```bash
//! # mello-sfu
//! SFU_PUBLIC_IP=127.0.0.1 SFU_ADMIN_PASSWORD=devpass go run ./cmd/sfu
//! # mello
//! MELLO_TEST_SFU=ws://127.0.0.1:8443/ws MELLO_TEST_SFU_PASSWORD=devpass \
//!   cargo test -p mello-core --lib -- --ignored --nocapture slots_live
//! ```
//!
//! Each member is a real libmello context on the device-free audio backend
//! with a real `SfuConnection` and `VoiceManager`. Native threads inject
//! microphone frames at the capture pace.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::VoiceManager;
use crate::events::Event;
use crate::transport::SfuConnection;

const CREW: &str = "slots-live";
const FRAME: usize = 960;
/// Members in the channel before the measured one joins: the plan's gate
/// is a join into an 8-person channel.
const EXISTING: usize = 7;
const SPEECH_SPURTS: usize = 6;

type Stamped = Arc<Mutex<Vec<(Instant, Event)>>>;

struct Member {
    user: String,
    voice: VoiceManager,
    conn: Arc<SfuConnection>,
    events: Stamped,
}

fn sfu_env() -> Option<(String, String)> {
    let endpoint = std::env::var("MELLO_TEST_SFU").ok()?;
    let password = std::env::var("MELLO_TEST_SFU_PASSWORD").ok()?;
    Some((endpoint, password))
}

/// Join `channel` as `user` through the real connection and voice manager.
async fn join(endpoint: &str, password: &str, channel: &str, user: &str) -> Member {
    let (tx, rx) = std_mpsc::channel();
    let events: Stamped = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    std::thread::spawn(move || {
        while let Ok(ev) = rx.recv() {
            sink.lock().expect("events").push((Instant::now(), ev));
        }
    });
    let mut voice = VoiceManager::with_test_audio_backend(tx);
    // Test-password mode: the query parameters ride on the token argument.
    let token = format!(
        "unused&test_password={password}&test_type=voice&test_user_id={user}&test_username={user}"
    );
    let mut conn = SfuConnection::connect(endpoint, &token)
        .await
        .expect("connect to the SFU");
    // SAFETY: the context is live; the peer counts dropped slot packets there.
    let peer = unsafe { SfuConnection::create_peer(voice.mello_ctx(), &[]) }.expect("peer");
    conn.join_voice(peer, CREW, channel).await.expect("join");
    conn.wait_for_datachannel_open()
        .await
        .expect("data channels open");
    let conn = Arc::new(conn);
    voice.set_push_to_talk(true);
    voice.join_voice_sfu(user, CREW, Arc::clone(&conn));
    // The test backend has no microphone: frames come from spawn_mic.
    assert!(voice.start_capture_inject(), "capture inject mode");
    Member {
        user: user.to_string(),
        voice,
        conn,
        events,
    }
}

/// Inject 20 ms frames from `source` into the context at the capture pace
/// until `stop`.
fn spawn_mic(
    ctx: *mut mello_sys::MelloContext,
    stop: Arc<AtomicBool>,
    source: impl Fn(u64) -> Vec<i16> + Send + 'static,
) -> std::thread::JoinHandle<()> {
    let ctx = ctx as usize;
    std::thread::spawn(move || {
        let mut next = Instant::now();
        let mut index = 0u64;
        while !stop.load(Ordering::Relaxed) {
            let frame = source(index);
            // SAFETY: the context outlives the mic (the test stops the mic
            // before it drops the voice manager); libmello copies the samples.
            unsafe {
                mello_sys::mello_voice_inject_capture(
                    ctx as *mut mello_sys::MelloContext,
                    frame.as_ptr(),
                    frame.len() as i32,
                )
            };
            index += 1;
            next += Duration::from_millis(20);
            if let Some(sleep) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(sleep);
            }
        }
    })
}

fn tone(index: u64) -> Vec<i16> {
    (0..FRAME)
        .map(|i| {
            let t = (index * FRAME as u64 + i as u64) as f64 / 48_000.0;
            (6000.0 * (2.0 * std::f64::consts::PI * 300.0 * t).sin()) as i16
        })
        .collect()
}

/// The speech corpus clip, 48 kHz mono 16-bit.
fn speech() -> Vec<i16> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../libmello/tests/fixtures/voice/m1_davison.wav"
    );
    let bytes = std::fs::read(path).expect("speech fixture");
    let data = bytes
        .windows(4)
        .position(|w| w == b"data")
        .expect("wav data chunk")
        + 8;
    bytes[data..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

fn wait(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn summary(label: &str, mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let at = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    println!(
        "[slots-live] {label:<34} n={} min={:.1} p50={:.1} max={:.1} ms",
        v.len(),
        v[0],
        at(0.5),
        v[v.len() - 1]
    );
    v[v.len() - 1]
}

/// Speaking state changes of `member_id` that `m` saw, in order.
fn activity(m: &Member, member_id: &str) -> Vec<(Instant, bool)> {
    m.events
        .lock()
        .expect("events")
        .iter()
        .filter_map(|(at, ev)| match ev {
            Event::VoiceActivity {
                member_id: id,
                speaking,
            } if id == member_id => Some((*at, *speaking)),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a local SFU: MELLO_TEST_SFU and MELLO_TEST_SFU_PASSWORD"]
async fn slots_live_join_and_speaking_latency() {
    let Some((endpoint, password)) = sfu_env() else {
        println!("[slots-live] skipped: MELLO_TEST_SFU is not set");
        return;
    };
    let channel = format!("live-{}", std::process::id());

    // The channel: EXISTING members, each talking (push-to-talk, tone).
    let mut existing = Vec::new();
    let mut mics = Vec::new();
    let stop = Arc::new(AtomicBool::new(false));
    for i in 0..EXISTING {
        let m = join(&endpoint, &password, &channel, &format!("live-member-{i}")).await;
        mics.push(spawn_mic(m.voice.mello_ctx(), Arc::clone(&stop), tone));
        existing.push(m);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The measured join. Its microphone starts when voice starts, as in
    // the client.
    let mut joiner = join(&endpoint, &password, &channel, "live-joiner").await;
    let joiner_stop = Arc::new(AtomicBool::new(false));
    let joiner_mic = spawn_mic(joiner.voice.mello_ctx(), Arc::clone(&joiner_stop), tone);

    let joiner_user = joiner.user.clone();
    wait("audio both ways", Duration::from_secs(10), || {
        existing.iter().all(|e| {
            joiner.conn.first_audio_from(&e.user).is_some()
                && e.conn.first_audio_from(&joiner_user).is_some()
        })
    });
    let joined_at = joiner
        .conn
        .first_audio_from(&existing[0].user)
        .expect("joined")
        .0;
    let heard_by_joiner: Vec<f64> = existing
        .iter()
        .map(|e| ms(joiner.conn.first_audio_from(&e.user).expect("rx").1 - joined_at))
        .collect();
    let joiner_heard: Vec<f64> = existing
        .iter()
        .map(|e| ms(e.conn.first_audio_from(&joiner_user).expect("rx").1 - joined_at))
        .collect();
    let max_in = summary("join: joined -> joiner hears member", heard_by_joiner);
    let max_out = summary("join: joined -> member hears joiner", joiner_heard);

    // Speaking: the joiner switches to the VAD and talks in spurts with
    // silence between them.
    joiner_stop.store(true, Ordering::Relaxed);
    joiner_mic.join().expect("mic thread");
    joiner.voice.set_push_to_talk(false);
    let clip = Arc::new(speech());
    let spurt = 75u64; // frames: 1.5 s of speech, then 1.5 s of silence
    let speech_stop = Arc::new(AtomicBool::new(false));
    let speech_mic = {
        let clip = Arc::clone(&clip);
        spawn_mic(
            joiner.voice.mello_ctx(),
            Arc::clone(&speech_stop),
            move |index| {
                let cycle = index / spurt;
                if cycle.is_multiple_of(2) || cycle / 2 >= SPEECH_SPURTS as u64 {
                    return vec![0; FRAME];
                }
                // Skip the clip's leading pause; each spurt takes new speech.
                let offset = 24_000 + ((cycle / 2) * spurt + index % spurt) as usize * FRAME;
                let offset = offset % (clip.len() - FRAME);
                clip[offset..offset + FRAME].to_vec()
            },
        )
    };
    let listener = &existing[0];
    tokio::time::sleep(Duration::from_secs(3 * SPEECH_SPURTS as u64 + 4)).await;
    speech_stop.store(true, Ordering::Relaxed);
    speech_mic.join().expect("speech thread");

    let local = activity(&joiner, &joiner_user);
    let remote = activity(listener, &joiner_user);
    // Each remote ring against the VAD open that caused it: the latest
    // local open before it. A VAD flap inside 100 ms is one change on the
    // wire (the rate limit), so local opens can outnumber remote rings.
    let mut ring = Vec::new();
    for (rx_at, _) in remote.iter().filter(|(_, on)| *on) {
        if let Some((at, _)) = local.iter().rev().find(|(l, on)| *on && l <= rx_at) {
            ring.push(ms(*rx_at - *at));
        }
    }
    assert!(
        !ring.is_empty(),
        "no speaking change reached the listener: local {local:?} remote {remote:?}"
    );
    let max_ring = summary("speaking: VAD open -> remote ring", ring);
    let offs = remote.iter().filter(|(_, s)| !*s).count();
    println!(
        "[slots-live] local VAD changes={} remote changes={} (remote off={})",
        local.len(),
        remote.len(),
        offs
    );
    for _ in 0..3 {
        joiner.conn.send_ping();
        listener.conn.send_ping();
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    println!(
        "[slots-live] control-channel RTT joiner={:.1} ms member={:.1} ms",
        joiner.conn.rtt_ms(),
        listener.conn.rtt_ms()
    );

    stop.store(true, Ordering::Relaxed);
    for mic in mics {
        mic.join().expect("mic thread");
    }
    joiner.voice.leave_voice();
    for mut e in existing {
        e.voice.leave_voice();
    }

    assert!(max_in < 300.0 && max_out < 300.0, "join gate: 300 ms");
    assert!(max_ring < 100.0, "speaking gate: 100 ms");
}
