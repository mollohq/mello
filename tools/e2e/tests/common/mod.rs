//! Helpers shared by the e2e test binaries.

// Each test binary uses a part of this module.
#![allow(dead_code)]

/// One request captured by the stub push Worker.
pub struct CapturedSend {
    pub head: String,
    pub body: serde_json::Value,
    pub at: std::time::Instant,
}

/// Serves `POST /send` on the host port that `scripts/e2e.sh` gave Nakama as
/// `PUSH_WORKER_URL`, answers like the real Worker, and reports each request.
pub fn stub_push_worker(port: u16) -> std::sync::mpsc::Receiver<CapturedSend> {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind(("0.0.0.0", port)).expect("bind stub push worker");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut head = String::new();
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let mut raw = vec![0u8; len];
            let _ = reader.read_exact(&mut raw);
            let body: serde_json::Value = serde_json::from_slice(&raw).unwrap_or_default();
            let token = body["tokens"][0]["token"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let reply = serde_json::json!({
                "results": [{"token": token, "status": "sent", "apns_id": "e2e"}],
                "prune": [],
            })
            .to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            if tx
                .send(CapturedSend {
                    head,
                    body,
                    at: std::time::Instant::now(),
                })
                .is_err()
            {
                return;
            }
        }
    });
    rx
}

/// Waits for the next `/send` without blocking the single-threaded runtime:
/// the socket writer task must run to deliver messages at all.
pub async fn next_send(rx: &std::sync::mpsc::Receiver<CapturedSend>, what: &str) -> CapturedSend {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Ok(got) = rx.try_recv() {
            return got;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no /send within 15 s: {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Waits for the next `/send` of one notification type and skips the others.
/// A crew join or a belt push can arrive before the push a test waits for.
pub async fn next_send_of_type(
    rx: &std::sync::mpsc::Receiver<CapturedSend>,
    kind: &str,
    what: &str,
) -> CapturedSend {
    loop {
        let got = next_send(rx, what).await;
        if got.body["notification"]["type"] == kind {
            return got;
        }
    }
}

/// Takes every `/send` that arrives within `quiet`, then returns them.
pub async fn drain_sends(
    rx: &std::sync::mpsc::Receiver<CapturedSend>,
    quiet: std::time::Duration,
) -> Vec<CapturedSend> {
    tokio::time::sleep(quiet).await;
    rx.try_iter().collect()
}
