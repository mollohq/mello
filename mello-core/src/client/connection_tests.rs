//! The post-auth connect sequence (`Client::on_connected`) against a Nakama
//! stand-in that records the RPCs it receives.

use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::Client;
use crate::config::Config;
use crate::voice::VoiceManager;

/// The RPC ids a stand-in saw, in arrival order.
type Seen = Arc<Mutex<Vec<String>>>;

/// A Nakama stand-in on a local port. It answers every request with an empty
/// RPC payload and records the id of each `/v2/rpc/<id>` call.
async fn recording_nakama() -> (Config, Seen) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local port");
    let port = listener
        .local_addr()
        .expect("listener has an address")
        .port();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(serve_connection(socket, Arc::clone(&recorder)));
        }
    });
    let config = Config {
        nakama_host: "127.0.0.1".into(),
        nakama_port: port,
        nakama_ssl: false,
        ..Config::default()
    };
    (config, seen)
}

/// Serve HTTP/1.1 requests on one keep-alive connection until it closes.
async fn serve_connection(mut socket: tokio::net::TcpStream, seen: Seen) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let head_len = loop {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
            let mut chunk = [0u8; 4096];
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_len]).into_owned();
        let content_length = head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let total = head_len + 4 + content_length;
        while buf.len() < total {
            let mut chunk = [0u8; 4096];
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        buf.drain(..total);

        let path = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default();
        if let Some(id) = path.strip_prefix("/v2/rpc/") {
            let id = id.split('?').next().unwrap_or(id);
            seen.lock().expect("recorder lock").push(id.to_string());
        }

        let body = r#"{"payload":"{}"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        if socket.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// The server's session-start hook sets a connecting account online. A
/// `presence_update` from the client at the same moment wrote the same row,
/// and on a fresh account Nakama refused the second insert (code 13,
/// "failed to update presence"). The client sends none at connect.
#[tokio::test(flavor = "current_thread")]
async fn connect_sequence_sends_no_presence_update() {
    let (config, seen) = recording_nakama().await;
    let (event_tx, _events) = std::sync::mpsc::channel();
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
    client.nakama.install_session_for_tests("test-session");

    client.on_connected().await;

    let ids = seen.lock().expect("recorder lock").clone();
    assert!(
        ids.iter().any(|id| id == "health"),
        "the connect sequence reaches the server: {ids:?}"
    );
    assert!(
        !ids.iter().any(|id| id == "presence_update"),
        "presence at connect belongs to the session-start hook: {ids:?}"
    );
}

/// Stage 6: the source picker opening probes the SFU regions again when the
/// last probe is a minute old, so the stream goes to the region that is
/// fastest now. A fresh probe is not repeated.
#[tokio::test(flavor = "current_thread")]
async fn a_stream_start_probes_the_regions_again_when_the_ranking_is_stale() {
    let (config, health_hits) = recording_nakama().await;
    let health = format!(
        "http://{}:{}/health",
        config.nakama_host, config.nakama_port
    );
    let (event_tx, _events) = std::sync::mpsc::channel();
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
    client.sfu_region_list = vec![crate::transport::sfu_region::SfuRegionEndpoint {
        region: "eu-west".into(),
        health_url: health,
    }];

    // Probed 10 s ago: no new probe.
    client.region_probed_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(10));
    client.probe_regions_for_stream();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        crate::transport::sfu_region::preferred_region(&client.sfu_regions).is_none(),
        "a fresh ranking is not probed again"
    );

    // Probed 2 minutes ago: the picker opening probes now.
    client.region_probed_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(120));
    client.probe_regions_for_stream();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while crate::transport::sfu_region::preferred_region(&client.sfu_regions).is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "no probe at stream start"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        crate::transport::sfu_region::preferred_region(&client.sfu_regions).as_deref(),
        Some("eu-west")
    );
    drop(health_hits);
}
