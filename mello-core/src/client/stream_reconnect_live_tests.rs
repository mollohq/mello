//! The stage 6 gate against a real SFU with a pulled cable
//! (streaming-quality stage 6). Ignored by default: it needs a local
//! mello-sfu, a monitor to capture and a hardware encoder and decoder.
//!
//! Two clients run their real command loops in this process: a host that
//! streams monitor 0 and a viewer. A fake Nakama answers the stream RPCs
//! with test-mode SFU tokens. The host's signaling socket goes through a
//! TCP proxy and every UDP flow through a proxy on the SFU's one ICE
//! candidate (`SFU_PUBLIC_IP=127.0.0.2`, as the stream gate's shim). A
//! "cable pull" freezes the host's TCP and drops the host's UDP flows; the
//! viewer's flows pass.
//!
//! It reports, and checks against the stage 6 gate:
//! - click to first RTP on the host (local SFU: the EU number is Bob's run),
//! - Watch to first frame on the viewer,
//! - a 3 s and a 10 s pull: "reconnecting" on the viewer within 1 s, and
//!   the picture back within 3 s of the cable, same session, no click,
//! - a 35 s pull: both sides end with a message.
//!
//! ```bash
//! # mello-sfu (the deployed SFU), from Git Bash
//! GOTOOLCHAIN=go1.24.9 go build -o /tmp/sfu6/sfu.exe ./cmd/sfu
//! SFU_ADMIN_PASSWORD=devpass SFU_PUBLIC_IP=127.0.0.2 SFU_ICE_TCP_PORT=0 \
//!   SFU_ADDR=127.0.0.1:18443 SFU_ADMIN_ADDR=127.0.0.1:18080 SFU_REGION=test \
//!   /tmp/sfu6/sfu.exe
//! # mello
//! MELLO_TEST_SFU=ws://127.0.0.1:18443/ws MELLO_TEST_SFU_PASSWORD=devpass \
//!   cargo test -p mello-core --lib -- --ignored --nocapture stream_reconnect_live
//! ```

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::Client;
use crate::command::Command;
use crate::config::Config;
use crate::events::{Event, StreamRole};
use crate::voice::VoiceManager;

const SESSION_PREFIX: &str = "stage6-live";
/// The SFU's ICE candidate; the proxy sits on it.
const SHIM: &str = "127.0.0.2:10000";
const SFU_UDP: &str = "127.0.0.1:10000";

// ---------------------------------------------------------------------------
// The cable: a TCP proxy for the host's signaling and a UDP proxy on the
// SFU's candidate
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Cable {
    /// The host's cable is out.
    pulled: Arc<AtomicBool>,
    /// The client addresses of the viewer's UDP flows; every other flow is
    /// the host's.
    viewer_flows: Arc<Mutex<HashSet<SocketAddr>>>,
    /// Count the flows that open from now on as the viewer's.
    marking_viewer: Arc<AtomicBool>,
}

/// Forward TCP from a local port to `upstream`. While the cable is out
/// nothing moves (both ways), as on a pulled cable: the bytes wait.
async fn tcp_proxy(upstream: SocketAddr, cable: Cable) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the TCP proxy");
    let addr = listener.local_addr().expect("proxy address");
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let cable = cable.clone();
            tokio::spawn(async move {
                // A connect during a pull waits for the cable.
                while cable.pulled.load(Ordering::Acquire) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let Ok(server) = tokio::net::TcpStream::connect(upstream).await else {
                    return;
                };
                let (mut cr, mut cw) = client.into_split();
                let (mut sr, mut sw) = server.into_split();
                let c1 = cable.clone();
                let up = async move {
                    let mut buf = vec![0u8; 16 * 1024];
                    loop {
                        while c1.pulled.load(Ordering::Acquire) {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        let n = match cr.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        while c1.pulled.load(Ordering::Acquire) {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        if sw.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                };
                let c2 = cable.clone();
                let down = async move {
                    let mut buf = vec![0u8; 16 * 1024];
                    loop {
                        while c2.pulled.load(Ordering::Acquire) {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        let n = match sr.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        while c2.pulled.load(Ordering::Acquire) {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        if cw.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                };
                tokio::join!(up, down);
            });
        }
    });
    addr
}

/// A UDP proxy on the SFU's candidate: one upstream socket per client
/// flow. The host's flows drop while the cable is out.
async fn udp_proxy(cable: Cable) {
    let shim = Arc::new(
        tokio::net::UdpSocket::bind(SHIM)
            .await
            .expect("bind 127.0.0.2:10000 (is a stream gate running?)"),
    );
    let flows: Arc<Mutex<std::collections::HashMap<SocketAddr, Arc<tokio::net::UdpSocket>>>> =
        Arc::default();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        loop {
            let Ok((n, from)) = shim.recv_from(&mut buf).await else {
                return;
            };
            let existing = flows.lock().expect("flows").get(&from).cloned();
            let upstream = match existing {
                Some(u) => u,
                None => {
                    let Ok(u) = tokio::net::UdpSocket::bind("127.0.0.1:0").await else {
                        continue;
                    };
                    if u.connect(SFU_UDP).await.is_err() {
                        continue;
                    }
                    let u = Arc::new(u);
                    if cable.marking_viewer.load(Ordering::Acquire) {
                        cable
                            .viewer_flows
                            .lock()
                            .expect("viewer flows")
                            .insert(from);
                    }
                    flows.lock().expect("flows").insert(from, Arc::clone(&u));
                    let back = Arc::clone(&u);
                    let shim = Arc::clone(&shim);
                    let cable = cable.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 2048];
                        while let Ok(n) = back.recv(&mut buf).await {
                            if cable.is_cut(&from) {
                                continue;
                            }
                            let _ = shim.send_to(&buf[..n], from).await;
                        }
                    });
                    u
                }
            };
            if cable.is_cut(&from) {
                continue;
            }
            let _ = upstream.send(&buf[..n]).await;
        }
    });
}

impl Cable {
    fn is_cut(&self, flow: &SocketAddr) -> bool {
        self.pulled.load(Ordering::Acquire)
            && !self
                .viewer_flows
                .lock()
                .expect("viewer flows")
                .contains(flow)
    }
}

// ---------------------------------------------------------------------------
// A fake Nakama for the stream RPCs
// ---------------------------------------------------------------------------

/// Answers the stream RPCs with test-mode SFU tokens: `start_stream` and
/// `refresh_stream_token` from the host get `host_endpoint`, `watch_stream`
/// the SFU directly.
async fn stream_nakama(
    session: String,
    host_endpoint: String,
    viewer_endpoint: String,
    password: String,
) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local port");
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (session, host_ep, viewer_ep, password) = (
                session.clone(),
                host_endpoint.clone(),
                viewer_endpoint.clone(),
                password.clone(),
            );
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut reader = BufReader::new(read);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                    return;
                }
                let mut content_length = 0usize;
                let mut auth = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                    if lower.starts_with("authorization:") {
                        auth = line.trim().to_string();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0u8; content_length];
                if reader.read_exact(&mut body).await.is_err() {
                    return;
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or_default();
                let name = path
                    .strip_prefix("/v2/rpc/")
                    .unwrap_or(path)
                    .split('?')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let host = auth.contains("host-session");
                let user = if host { "stage6-host" } else { "stage6-viewer" };
                let token = format!(
                    "unused&test_password={password}&test_type=stream&test_user_id={user}&test_username={user}"
                );
                let reply = match name.as_str() {
                    "start_stream" => serde_json::json!({
                        "session_id": session, "stream_id": session, "mode": "sfu",
                        "sfu_endpoint": host_ep, "sfu_token": token, "advertise": "on_live",
                    }),
                    "watch_stream" => serde_json::json!({
                        "mode": "sfu", "sfu_endpoint": viewer_ep, "sfu_token": token,
                        "width": 0, "height": 0, "bitrate_kbps": 8000,
                    }),
                    "refresh_stream_token" => serde_json::json!({
                        "sfu_endpoint": if host { host_ep } else { viewer_ep },
                        "sfu_token": token,
                        "role": if host { "host" } else { "viewer" },
                    }),
                    _ => serde_json::json!({}),
                };
                let wrapped = serde_json::json!({ "payload": reply.to_string() }).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{wrapped}",
                    wrapped.len()
                );
                let _ = write.write_all(response.as_bytes()).await;
            });
        }
    });
    port
}

fn live_client(port: u16, session_token: &str) -> (Client, mpsc::Receiver<Event>) {
    let mut config = Config::development();
    config.nakama_host = "127.0.0.1".into();
    config.nakama_port = port;
    config.nakama_ssl = false;
    let (event_tx, events) = mpsc::channel();
    let voice = VoiceManager::with_test_audio_backend(event_tx.clone());
    let mut client = Client::with_voice(
        config,
        event_tx,
        voice,
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(None)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicU8::new(0)),
        false,
        false,
    );
    client.nakama.set_test_session(session_token, None);
    (client, events)
}

/// Events of one client with the time each arrived.
struct Timeline {
    rx: mpsc::Receiver<Event>,
    seen: Vec<(Instant, Event)>,
}

impl Timeline {
    fn drain(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            self.seen.push((Instant::now(), ev));
        }
    }

    /// The first event at or after `since` that `pick` accepts.
    async fn wait(
        &mut self,
        since: Instant,
        limit: Duration,
        what: &str,
        pick: impl Fn(&Event) -> bool,
    ) -> Option<Instant> {
        let deadline = Instant::now() + limit;
        loop {
            self.drain();
            if let Some((at, _)) = self.seen.iter().find(|(at, ev)| *at >= since && pick(ev)) {
                return Some(*at);
            }
            if Instant::now() >= deadline {
                println!("  (no {what} within {limit:?})");
                return None;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// Prints the stream lines of the client log with the time since the test
/// started: "Stream start timing", "Stream watch timing", the link changes.
struct StreamLogPrinter(Instant);

impl log::Log for StreamLogPrinter {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        let msg = record.args().to_string();
        if msg.contains("Stream") || msg.contains("stream_live") {
            println!("[{:>6} ms] {}", self.0.elapsed().as_millis(), msg);
        }
    }

    fn flush(&self) {}
}

fn ms(d: Option<Duration>) -> String {
    d.map_or("none".into(), |d| format!("{} ms", d.as_millis()))
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "needs a local SFU on 127.0.0.2's candidate, a monitor and a hardware encoder: MELLO_TEST_SFU, MELLO_TEST_SFU_PASSWORD"]
async fn stream_reconnect_live() {
    let (Ok(endpoint), Ok(password)) = (
        std::env::var("MELLO_TEST_SFU"),
        std::env::var("MELLO_TEST_SFU_PASSWORD"),
    ) else {
        panic!("set MELLO_TEST_SFU and MELLO_TEST_SFU_PASSWORD");
    };
    if log::set_logger(Box::leak(Box::new(StreamLogPrinter(Instant::now())))).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    let session = format!(
        "{SESSION_PREFIX}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default()
    );
    let sfu_addr: SocketAddr = endpoint
        .trim_start_matches("ws://")
        .split('/')
        .next()
        .and_then(|a| a.parse().ok())
        .expect("MELLO_TEST_SFU is ws://ip:port/ws");

    let cable = Cable::default();
    udp_proxy(cable.clone()).await;
    let host_proxy = tcp_proxy(sfu_addr, cable.clone()).await;
    let host_endpoint = format!("ws://{host_proxy}/ws");
    let port = stream_nakama(session.clone(), host_endpoint, endpoint.clone(), password).await;

    let (mut host, host_events) = live_client(port, "host-session");
    let (mut viewer, viewer_events) = live_client(port, "viewer-session");
    let (host_tx, host_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
    let (viewer_tx, viewer_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
    let mut host_tl = Timeline {
        rx: host_events,
        seen: Vec::new(),
    };
    let mut viewer_tl = Timeline {
        rx: viewer_events,
        seen: Vec::new(),
    };
    let mut report = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    let driver = async {
        // 1. Start: click to first RTP. The live line carries the steps.
        let clicked = Instant::now();
        host_tx
            .send(Command::StartStream {
                crew_id: "crew-live".into(),
                title: "stage 6 live".into(),
                capture_mode: "monitor".into(),
                monitor_index: Some(0),
                hwnd: None,
                pid: None,
                preset: 0,
                exe: String::new(),
                igdb_id: 0,
            })
            .expect("host loop");
        let live = host_tl
            .wait(clicked, Duration::from_secs(20), "StreamStarted", |e| {
                matches!(e, Event::StreamStarted { .. })
            })
            .await
            .expect("the stream goes live");
        report.push(format!(
            "click to live: {} ms",
            (live - clicked).as_millis()
        ));
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 2. Watch: to the first frame.
        cable.marking_viewer.store(true, Ordering::Release);
        let watched = Instant::now();
        viewer_tx
            .send(Command::WatchStream {
                host_id: "stage6-host".into(),
                session_id: session.clone(),
                width: 1920,
                height: 1080,
            })
            .expect("viewer loop");
        viewer_tl
            .wait(watched, Duration::from_secs(10), "StreamWatching", |e| {
                matches!(e, Event::StreamWatching { .. })
            })
            .await
            .expect("the viewer watches");
        // The first frame: a debug event with frames presented.
        let first = viewer_tl
            .wait(watched, Duration::from_secs(10), "first frame", |e| {
                matches!(e, Event::StreamDebugStats { frames_presented, .. } if *frames_presented > 0)
            })
            .await;
        report.push(format!(
            "Watch to first frame (1 s debug event, upper bound): {}",
            ms(first.map(|f| f - watched))
        ));
        tokio::time::sleep(Duration::from_secs(2)).await;
        cable.marking_viewer.store(false, Ordering::Release);

        // 3. Pulls of 3 s and 10 s.
        for pull in [3u64, 10] {
            let pulled_at = Instant::now();
            cable.pulled.store(true, Ordering::Release);
            let shown = viewer_tl
                .wait(
                    pulled_at,
                    Duration::from_secs(3),
                    "viewer StreamReconnecting",
                    |e| {
                        matches!(
                            e,
                            Event::StreamReconnecting {
                                role: StreamRole::Viewer
                            }
                        )
                    },
                )
                .await;
            let host_shown = host_tl
                .wait(
                    pulled_at,
                    Duration::from_secs(3),
                    "host StreamReconnecting",
                    |e| {
                        matches!(
                            e,
                            Event::StreamReconnecting {
                                role: StreamRole::Host
                            }
                        )
                    },
                )
                .await;
            tokio::time::sleep(Duration::from_secs(pull).saturating_sub(pulled_at.elapsed())).await;
            let returned = Instant::now();
            cable.pulled.store(false, Ordering::Release);
            let back = viewer_tl
                .wait(
                    returned,
                    Duration::from_secs(10),
                    "viewer StreamResumed",
                    |e| {
                        matches!(
                            e,
                            Event::StreamResumed {
                                role: StreamRole::Viewer
                            }
                        )
                    },
                )
                .await;
            let host_back = host_tl
                .wait(
                    returned,
                    Duration::from_secs(10),
                    "host StreamResumed",
                    |e| {
                        matches!(
                            e,
                            Event::StreamResumed {
                                role: StreamRole::Host
                            }
                        )
                    },
                )
                .await;
            report.push(format!(
                "pull {pull} s: viewer reconnecting after {}, host after {}; viewer picture back {} after the cable, host link back {}",
                ms(shown.map(|t| t - pulled_at)),
                ms(host_shown.map(|t| t - pulled_at)),
                ms(back.map(|t| t - returned)),
                ms(host_back.map(|t| t - returned)),
            ));
            if shown.is_none_or(|t| t - pulled_at > Duration::from_secs(1)) {
                failures.push(format!("pull {pull} s: no viewer reconnecting within 1 s"));
            }
            if back.is_none_or(|t| t - returned > Duration::from_secs(3)) {
                failures.push(format!("pull {pull} s: no picture within 3 s of the cable"));
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            // A second dip after the picture came back.
            viewer_tl.drain();
            let dips = viewer_tl
                .seen
                .iter()
                .filter(|(at, e)| {
                    *at > returned
                        && matches!(
                            e,
                            Event::StreamReconnecting {
                                role: StreamRole::Viewer
                            }
                        )
                })
                .count();
            report.push(format!(
                "pull {pull} s: viewer reconnecting again within 3 s after the picture came back: {dips} time(s)"
            ));
        }
        let joins_same_session = !host_tl
            .seen
            .iter()
            .chain(viewer_tl.seen.iter())
            .any(|(_, e)| matches!(e, Event::StreamEnded { .. } | Event::StreamWatchingStopped));
        report.push(format!(
            "same session, no end through the pulls: {joins_same_session}"
        ));
        if !joins_same_session {
            failures.push("a pull ended the stream".into());
        }

        // 4. A 35 s pull ends the stream on both sides with a message.
        let pulled_at = Instant::now();
        cable.pulled.store(true, Ordering::Release);
        let host_end = host_tl
            .wait(
                pulled_at,
                Duration::from_secs(40),
                "host StreamEnded",
                |e| {
                    matches!(
                        e,
                        Event::StreamEnded {
                            role: StreamRole::Host,
                            ..
                        }
                    )
                },
            )
            .await;
        let viewer_end = viewer_tl
            .wait(
                pulled_at,
                Duration::from_secs(10),
                "viewer StreamEnded",
                |e| {
                    matches!(
                        e,
                        Event::StreamEnded {
                            role: StreamRole::Viewer,
                            ..
                        }
                    )
                },
            )
            .await;
        cable.pulled.store(false, Ordering::Release);
        let reason = |tl: &Timeline, role: StreamRole| {
            tl.seen.iter().find_map(|(_, e)| match e {
                Event::StreamEnded {
                    role: r, reason, ..
                } if *r == role => Some(reason.clone()),
                _ => None,
            })
        };
        report.push(format!(
            "pull 35 s: host ended after {} ({:?}), viewer ended after {} ({:?})",
            ms(host_end.map(|t| t - pulled_at)),
            reason(&host_tl, StreamRole::Host),
            ms(viewer_end.map(|t| t - pulled_at)),
            reason(&viewer_tl, StreamRole::Viewer),
        ));
        if host_end.is_none() || viewer_end.is_none() {
            failures.push("pull 35 s: not both sides ended".into());
        }

        drop(host_tx);
        drop(viewer_tx);
    };
    tokio::join!(host.run(host_rx), viewer.run(viewer_rx), driver);
    if let Some((attached, first)) = host
        .stream_host_sfu_sink
        .as_ref()
        .and_then(|s| s.first_video_sent())
    {
        println!(
            "host sink: first access unit {} ms after the attach",
            first.duration_since(attached).as_millis()
        );
    }
    println!("--- stage 6 live ---");
    for line in &report {
        println!("{line}");
    }
    assert!(failures.is_empty(), "stage 6 gate: {failures:?}");
}
