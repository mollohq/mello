//! Stage 6 reconnect measurement against a real SFU in Docker
//! (plans/voice-quality.md stage 6, spec 10 section 7.6).
//!
//! Ignored by default. It needs a mello-sfu container in test-password mode
//! that the test may kill, pause, start and unpause. Start it from the
//! mello-sfu compose; its container is `mello-sfu-sfu-1`, the default of
//! `MELLO_TEST_SFU_CONTAINER`:
//!
//! ```bash
//! # mello-sfu
//! SFU_ADMIN_PASSWORD=devpass docker compose \
//!   -f docker-compose.yml -f docker-compose.test-mode.yml up -d --build
//! # mello
//! ```
//!
//! ```bash
//! MELLO_TEST_SFU=ws://127.0.0.1:8443/ws MELLO_TEST_SFU_PASSWORD=devpass \
//! MELLO_TEST_SFU_HEALTH=http://127.0.0.1:8080/health \
//! MELLO_TEST_FAULT=kill \
//! MELLO_TEST_OUTAGE_MS=3000 \
//!   cargo test -p mello-core --lib -- --ignored --nocapture sfu_reconnect_live
//! ```
//!
//! The client under test is a real `Client` loop on the device-free audio
//! backend; the SFU voice token is a test-password token, so no backend is
//! needed (the reconnect reuses it like a valid voice token). An observer
//! member talks with a tone and rejoins as soon as the SFU is healthy.
//!
//! Faults (`MELLO_TEST_FAULT`):
//! - `kill`: `docker kill`, then `docker start` after the outage. The SFU
//!   process dies: its sockets close.
//! - `pause`: `docker pause`, then `docker unpause` after the outage. The
//!   link goes silent: nothing closes, as with a pulled cable.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::crew_state::VoiceJoinResponse;
use crate::events::Event;
use crate::transport::SfuConnection;
use crate::voice::VoiceManager;

const CREW: &str = "reconnect-live";
const CLIENT: &str = "rc-client";
const OBSERVER: &str = "rc-observer";
const FRAME: usize = 960;
const ROUNDS: usize = 3;

struct Live {
    endpoint: String,
    password: String,
    health: String,
    container: String,
    fault: String,
    outage: Duration,
    ice_servers: Vec<String>,
}

fn live() -> Option<Live> {
    let var = |k: &str| std::env::var(k).ok();
    Some(Live {
        endpoint: var("MELLO_TEST_SFU")?,
        password: var("MELLO_TEST_SFU_PASSWORD")?,
        health: var("MELLO_TEST_SFU_HEALTH")?,
        container: var("MELLO_TEST_SFU_CONTAINER").unwrap_or_else(|| "mello-sfu-sfu-1".into()),
        fault: var("MELLO_TEST_FAULT").unwrap_or_else(|| "kill".into()),
        outage: Duration::from_millis(
            var("MELLO_TEST_OUTAGE_MS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(3000),
        ),
        ice_servers: var("MELLO_TEST_ICE")
            .map(|v| v.split(',').map(str::to_string).collect())
            .unwrap_or_default(),
    })
}

fn token(password: &str, user: &str) -> String {
    format!(
        "unused&test_password={password}&test_type=voice&test_user_id={user}&test_username={user}"
    )
}

async fn docker(args: &[&str]) {
    let out = tokio::process::Command::new("docker")
        .args(args)
        .output()
        .await
        .expect("run docker");
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn wait_healthy(http: &reqwest::Client, url: &str) -> Instant {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(resp) = http
            .get(url)
            .timeout(Duration::from_millis(500))
            .send()
            .await
        {
            if resp.status().is_success() {
                return Instant::now();
            }
        }
        assert!(Instant::now() < deadline, "the SFU did not come back");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn tone(index: u64) -> Vec<i16> {
    (0..FRAME)
        .map(|i| {
            let t = (index * FRAME as u64 + i as u64) as f64 / 48_000.0;
            (6000.0 * (2.0 * std::f64::consts::PI * 300.0 * t).sin()) as i16
        })
        .collect()
}

/// Inject a tone into `ctx` at the capture pace until `stop`. A frame
/// outside inject mode is refused; the thread goes on.
fn spawn_mic(ctx: usize, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut next = Instant::now();
        let mut index = 0u64;
        while !stop.load(Ordering::Relaxed) {
            let frame = tone(index);
            // SAFETY: the context outlives the mic: the test stops the mic
            // before it drops the voice manager; libmello copies the samples.
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

/// The other member: a voice manager with a real SFU connection that
/// talks a tone.
struct Observer {
    voice: VoiceManager,
    conn: Arc<SfuConnection>,
    stop: Arc<AtomicBool>,
    mic: Option<std::thread::JoinHandle<()>>,
}

impl Observer {
    async fn join(live: &Live, channel: &str) -> Observer {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match Self::try_join(live, channel).await {
                Ok(o) => return o,
                Err(e) => {
                    assert!(Instant::now() < deadline, "observer join: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    async fn try_join(live: &Live, channel: &str) -> Result<Observer, String> {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut voice = VoiceManager::with_test_audio_backend(tx);
        let mut conn = SfuConnection::connect(&live.endpoint, &token(&live.password, OBSERVER))
            .await
            .map_err(|e| e.to_string())?;
        // SAFETY: the context is live.
        let peer = unsafe { SfuConnection::create_peer(voice.mello_ctx(), &[]) }
            .map_err(|e| e.to_string())?;
        conn.join_voice(peer, CREW, channel)
            .await
            .map_err(|e| e.to_string())?;
        conn.wait_for_datachannel_open()
            .await
            .map_err(|e| e.to_string())?;
        let conn = Arc::new(conn);
        voice.set_push_to_talk(true);
        voice.join_voice_sfu(OBSERVER, CREW, Arc::clone(&conn));
        if !voice.start_capture_inject() {
            return Err("capture inject".into());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let mic = spawn_mic(voice.mello_ctx() as usize, Arc::clone(&stop));
        Ok(Observer {
            voice,
            conn,
            stop,
            mic: Some(mic),
        })
    }

    fn leave(self) {
        drop(self);
    }
}

impl Drop for Observer {
    // The mic thread must stop before the voice manager frees its context,
    // also when a failed assertion unwinds the test.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mic) = self.mic.take() {
            let _ = mic.join();
        }
        self.voice.leave_voice();
    }
}

/// Stops a mic thread when dropped. Declared after the voice manager it
/// feeds, so it drops first.
struct MicGuard {
    stop: Arc<AtomicBool>,
    mic: Option<std::thread::JoinHandle<()>>,
}

impl Drop for MicGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mic) = self.mic.take() {
            let _ = mic.join();
        }
    }
}

fn rx_inserted(ctx: usize) -> u64 {
    // SAFETY: zeroed is a valid MelloDebugStats; the context is live. The
    // driver runs on the loop's task, so no other call into the context runs
    // at the same time.
    unsafe {
        let mut s: mello_sys::MelloDebugStats = std::mem::zeroed();
        mello_sys::mello_get_debug_stats(ctx as *mut mello_sys::MelloContext, &mut s);
        s.rx_packets_inserted
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Events of the client, stamped on arrival by a thread of their own, so a
/// stamp is the time the core sent the event, not the time the driver read it.
struct Events {
    log: Arc<std::sync::Mutex<Vec<(Instant, Event)>>>,
    seen: std::cell::Cell<usize>,
}

impl Events {
    fn start(rx: std::sync::mpsc::Receiver<Event>) -> Self {
        let log: Arc<std::sync::Mutex<Vec<(Instant, Event)>>> = Arc::default();
        let sink = Arc::clone(&log);
        std::thread::spawn(move || {
            while let Ok(ev) = rx.recv() {
                sink.lock().expect("events").push((Instant::now(), ev));
            }
        });
        Self {
            log,
            seen: std::cell::Cell::new(0),
        }
    }

    /// The stamp of the first event after the ones already seen that
    /// `pick` accepts.
    async fn wait(&self, what: &str, limit: Duration, pick: impl Fn(&Event) -> bool) -> Instant {
        let deadline = Instant::now() + limit;
        loop {
            {
                let log = self.log.lock().expect("events");
                for (i, (at, ev)) in log.iter().enumerate().skip(self.seen.get()) {
                    if pick(ev) {
                        self.seen.set(i + 1);
                        return *at;
                    }
                }
                self.seen.set(log.len());
            }
            assert!(Instant::now() < deadline, "no {what} in {limit:?}");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
}

/// The time `done` first holds, or `None` after `limit`.
async fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> Option<Instant> {
    let deadline = Instant::now() + limit;
    loop {
        if done() {
            return Some(Instant::now());
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

fn after(t: Option<Instant>, from: Instant) -> String {
    t.map(|t| format!("{:.0} ms", ms(t.saturating_duration_since(from))))
        .unwrap_or_else(|| "none in 20 s".to_string())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "needs the mello-sfu compose container: MELLO_TEST_SFU, MELLO_TEST_SFU_PASSWORD and MELLO_TEST_SFU_HEALTH"]
async fn sfu_reconnect_live_timings() {
    let Some(live) = live() else {
        println!("[reconnect-live] skipped: MELLO_TEST_SFU is not set");
        return;
    };
    let http = reqwest::Client::new();
    wait_healthy(&http, &live.health).await;
    let channel = format!("rc-{}", std::process::id());

    let mut observer = Some(Observer::join(&live, &channel).await);

    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let voice = VoiceManager::with_test_audio_backend(event_tx.clone());
    let mut client = super::Client::with_voice(
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
    client.ice_servers = live.ice_servers.clone();
    let resp: VoiceJoinResponse = serde_json::from_value(serde_json::json!({
        "channel_id": channel,
        "voice_state": {
            "channel_id": channel,
            "members": [{ "user_id": CLIENT }, { "user_id": OBSERVER }],
        },
        "mode": "sfu",
        "sfu_endpoint": live.endpoint,
        "sfu_token": token(&live.password, CLIENT),
    }))
    .expect("valid voice_join response");
    client.last_voice_channel = Some(channel.clone());
    client.store_voice_join_grant(CREW, Some(CLIENT), &resp);
    client
        .start_voice_media(CREW, Some(CLIENT), &resp, false)
        .await;
    let ctx = client.voice.mello_ctx() as usize;
    let stop = Arc::new(AtomicBool::new(false));
    let client_mic = MicGuard {
        stop: Arc::clone(&stop),
        mic: Some(spawn_mic(ctx, Arc::clone(&stop))),
    };
    let events = Events::start(event_rx);
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let in_call = |ev: &Event| matches!(ev, Event::VoiceStateChanged { in_call: true, .. });

    let driver = async {
        let start_talking = || {
            cmd_tx
                .send(crate::command::Command::SetPushToTalk { enabled: true })
                .expect("the loop listens");
            cmd_tx
                .send(crate::command::Command::StartVoiceCaptureInject)
                .expect("the loop listens");
        };
        let joined = events
            .wait("first in_call", Duration::from_secs(20), in_call)
            .await;
        start_talking();
        let heard = wait_until(Duration::from_secs(10), || rx_inserted(ctx) > 0)
            .await
            .expect("the client hears the observer");
        println!(
            "[reconnect-live] first join: in_call -> client hears observer {:.0} ms",
            ms(heard - joined)
        );

        let mut rows = Vec::new();
        for round in 0..ROUNDS {
            let base_rx = rx_inserted(ctx);
            let (down, up) = match live.fault.as_str() {
                "pause" => ("pause", "unpause"),
                _ => ("kill", "start"),
            };
            let fault_at = Instant::now();
            docker(&[down, &live.container]).await;
            let fault_done = Instant::now();
            // The UI shows "reconnecting" on the first of: the session
            // dropped, or the link stalled (no pong for 1 s).
            let reconnecting = events
                .wait("reconnecting", Duration::from_secs(60), |ev| {
                    matches!(
                        ev,
                        Event::VoiceSfuDisconnected { .. }
                            | Event::VoiceLinkStalled { stalled: true, .. }
                    )
                })
                .await;
            if let Some(o) = observer.take() {
                o.leave();
            }
            // The outage counts from the fault.
            if let Some(rest) = live.outage.checked_sub(fault_at.elapsed()) {
                tokio::time::sleep(rest).await;
            }
            docker(&[up, &live.container]).await;
            let link_back = wait_healthy(&http, &live.health).await;
            observer = Some(Observer::join(&live, &channel).await);
            let observer_back = Instant::now();
            // Back: a new session (in_call), or the old one resumed (the
            // stall ended before the liveness check dropped it).
            let back_in_call = events
                .wait(
                    "the call back after the outage",
                    Duration::from_secs(60),
                    |ev| {
                        in_call(ev) || matches!(ev, Event::VoiceLinkStalled { stalled: false, .. })
                    },
                )
                .await;
            start_talking();
            let client_hears =
                wait_until(Duration::from_secs(20), || rx_inserted(ctx) > base_rx).await;
            let conn = Arc::clone(&observer.as_ref().expect("observer").conn);
            let observer_hears = wait_until(Duration::from_secs(20), || {
                conn.first_audio_from(CLIENT).is_some()
            })
            .await;
            let row = format!(
                "[reconnect-live] round {round} fault={} outage={:?}: docker {down} call {:.0} ms, call start -> reconnecting {:.0} ms; link back -> call back {:.0} ms, -> client hears {}, -> observer hears client {} (observer rejoined {:.0} ms after the link)",
                live.fault,
                live.outage,
                ms(fault_done - fault_at),
                ms(reconnecting.saturating_duration_since(fault_at)),
                ms(back_in_call.saturating_duration_since(link_back)),
                after(client_hears, link_back),
                after(observer_hears, link_back),
                ms(observer_back - link_back),
            );
            println!("{row}");
            for (at, ev) in events.log.lock().expect("events").iter() {
                if *at < fault_at {
                    continue;
                }
                let name = match ev {
                    Event::VoiceSfuDisconnected { reason, .. } => {
                        format!("VoiceSfuDisconnected({reason})")
                    }
                    Event::VoiceLinkStalled { stalled, .. } => {
                        format!("VoiceLinkStalled({stalled})")
                    }
                    Event::VoiceStateChanged { in_call, .. } => {
                        format!("VoiceStateChanged(in_call={in_call})")
                    }
                    _ => continue,
                };
                println!("[reconnect-live]   +{:.0} ms {name}", ms(*at - fault_at));
            }
            rows.push(row);
            // Let the call settle before the next fault.
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        drop(cmd_tx.clone());
        rows
    };
    let rows = tokio::time::timeout(Duration::from_secs(600), async {
        tokio::select! {
            _ = client.run(cmd_rx) => panic!("the loop ended early"),
            rows = driver => rows,
        }
    })
    .await
    .expect("the measurement ends");
    drop(client_mic);
    if let Some(o) = observer.take() {
        o.leave();
    }
    client.voice.leave_voice();
    assert_eq!(rows.len(), ROUNDS);
}
