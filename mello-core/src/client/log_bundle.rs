//! The client log bundle (mello-admin's Troubleshoot page; mello-sfu
//! 01-SFU.md section 8.8).
//!
//! An admin asks for a connected user's app log. The SFU sends
//! `request_logs` on the user's signaling socket. The client gzips the last
//! `since_hours` (24) of its app log, at most `max_bytes` (5 MB), and sends it
//! back on the same socket as `logs_bundle` chunks. The socket is the
//! client's authenticated connection, so the upload needs no new route.
//!
//! Consent: the app shows a one-time notice (Bob's default, the brief's
//! section 7). Here that is an info log line and `Event::LogsRequested` for
//! the UI to show; there is no prompt.

use std::fs;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use base64::Engine;
use flate2::write::GzEncoder;
use flate2::Compression;

use crate::events::Event;

/// The largest bundle a client sends, whatever the request says.
pub const MAX_BUNDLE_BYTES: usize = 5 * 1024 * 1024;
/// The largest chunk a client sends, whatever the request says.
pub const MAX_CHUNK_BYTES: usize = 192 * 1024;
/// The window a client sends at most, whatever the request says.
pub const MAX_SINCE_HOURS: u64 = 72;
/// The prefix of the app's daily log files (`client/src/lib.rs`
/// `init_logging`: `tracing_appender::rolling::daily(log_dir, "mello.log")`).
pub const LOG_FILE_PREFIX: &str = "mello.log";
/// Overrides the log directory (tests and tools).
pub const LOG_DIR_ENV: &str = "MELLO_LOG_DIR";

static EVENT_TX: OnceLock<Mutex<Option<std::sync::mpsc::Sender<Event>>>> = OnceLock::new();

/// The client registers its event channel, for `Event::LogsRequested`.
pub fn set_event_sender(tx: std::sync::mpsc::Sender<Event>) {
    let slot = EVENT_TX.get_or_init(|| Mutex::new(None));
    if let Ok(mut s) = slot.lock() {
        *s = Some(tx);
    }
}

fn emit(event: Event) {
    if let Some(slot) = EVENT_TX.get() {
        if let Ok(s) = slot.lock() {
            if let Some(tx) = s.as_ref() {
                let _ = tx.send(event);
            }
        }
    }
}

/// The app's log directory, as `init_logging` makes it:
/// `<data dir of app.mello.mello>/logs`, or `MELLO_LOG_DIR`.
pub fn log_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(LOG_DIR_ENV) {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    directories::ProjectDirs::from("app", "mello", "mello").map(|d| d.data_dir().join("logs"))
}

/// One request from the SFU, bounded by the client's own limits.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub request_id: String,
    pub since: Duration,
    pub max_bytes: usize,
    pub chunk_bytes: usize,
}

/// Reads `request_logs` data. A request id is 32 lowercase hex digits.
pub fn parse_request(data: &serde_json::Value) -> Option<Request> {
    let id = data.get("request_id")?.as_str()?;
    if id.len() != 32
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let num = |k: &str, default: u64| data.get(k).and_then(|v| v.as_u64()).unwrap_or(default);
    let hours = num("since_hours", 24).clamp(1, MAX_SINCE_HOURS);
    Some(Request {
        request_id: id.to_string(),
        since: Duration::from_secs(hours * 3600),
        max_bytes: (num("max_bytes", MAX_BUNDLE_BYTES as u64) as usize)
            .clamp(1024, MAX_BUNDLE_BYTES),
        chunk_bytes: (num("chunk_bytes", MAX_CHUNK_BYTES as u64) as usize)
            .clamp(1024, MAX_CHUNK_BYTES),
    })
}

/// The log files written in the window, oldest first: the daily files whose
/// last write is in the window (the file of the day before the window
/// starts holds lines of the window too when it was written into it).
pub fn files_in_window(dir: &Path, since: Duration, now: SystemTime) -> Vec<PathBuf> {
    let start = now.checked_sub(since).unwrap_or(SystemTime::UNIX_EPOCH);
    let mut files: Vec<(SystemTime, PathBuf)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?;
            if !name.starts_with(LOG_FILE_PREFIX) {
                return None;
            }
            let modified = e.metadata().ok()?.modified().ok()?;
            (modified >= start).then_some((modified, path))
        })
        .collect();
    files.sort();
    files.into_iter().map(|(_, p)| p).collect()
}

fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)?;
    enc.finish()
}

/// The bundle: the files, each under a header line, gzipped to at most
/// `max_bytes`. Over the cap, the oldest part goes first, so the bundle
/// keeps the newest lines.
pub fn build_bundle(files: &[PathBuf], max_bytes: usize) -> Result<Vec<u8>, String> {
    let mut text = Vec::new();
    for f in files {
        let body = fs::read(f).map_err(|e| format!("cannot read {}: {}", f.display(), e))?;
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        text.extend_from_slice(format!("===== {} =====\n", name).as_bytes());
        text.extend_from_slice(&body);
        if !body.ends_with(b"\n") {
            text.push(b'\n');
        }
    }
    if text.is_empty() {
        return Err("no app log in the window".into());
    }
    let mut keep = text.len();
    for _ in 0..8 {
        let start = text.len() - keep;
        // Start at a line, so the first line is whole.
        let start = text[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| if start == 0 { 0 } else { start + i + 1 })
            .unwrap_or(start);
        let mut slice = Vec::new();
        if start > 0 {
            slice.extend_from_slice(b"===== older lines left out: the bundle cap =====\n");
        }
        slice.extend_from_slice(&text[start..]);
        let out = gzip(&slice).map_err(|e| e.to_string())?;
        if out.len() <= max_bytes {
            return Ok(out);
        }
        // Keep the share that fits, with a margin for the ratio's spread.
        let share = max_bytes as f64 / out.len() as f64 * 0.9;
        keep = ((keep as f64) * share) as usize;
        if keep == 0 {
            break;
        }
    }
    Err("the log does not fit the bundle cap".into())
}

/// The `logs_bundle` messages of a bundle: chunks of at most `chunk_bytes`,
/// in order, the last one marked.
pub fn chunk_messages(request_id: &str, bundle: &[u8], chunk_bytes: usize) -> Vec<String> {
    let chunks: Vec<&[u8]> = bundle.chunks(chunk_bytes.max(1)).collect();
    let n = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(seq, part)| {
            serde_json::json!({
                "type": "logs_bundle",
                "seq": 0,
                "data": {
                    "request_id": request_id,
                    "seq": seq,
                    "last": seq + 1 == n,
                    "data": base64::engine::general_purpose::STANDARD.encode(part),
                },
            })
            .to_string()
        })
        .collect()
}

fn error_message(request_id: &str, error: &str) -> String {
    serde_json::json!({
        "type": "logs_bundle",
        "seq": 0,
        "data": {"request_id": request_id, "error": error},
    })
    .to_string()
}

/// Handles `request_logs`: the notice, then the bundle in the background,
/// sent with `send` (one signaling text message per call).
pub fn on_request<F, Fut>(data: &serde_json::Value, send: F)
where
    F: Fn(String) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), String>> + Send,
{
    on_request_in(data, log_dir(), send);
}

// on_request with the log directory given, so a test needs no env var.
fn on_request_in<F, Fut>(data: &serde_json::Value, dir: Option<PathBuf>, send: F)
where
    F: Fn(String) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), String>> + Send,
{
    let Some(req) = parse_request(data) else {
        log::warn!("Log bundle: request_logs with a bad request: {}", data);
        return;
    };
    log::info!(
        "Log bundle: Mello support asked for this app's log (request {}): the last {} h, \
         at most {} bytes, go to the voice and stream server",
        req.request_id,
        req.since.as_secs() / 3600,
        req.max_bytes
    );
    emit(Event::LogsRequested {
        request_id: req.request_id.clone(),
    });
    tokio::spawn(async move {
        let r = req.clone();
        let built = tokio::task::spawn_blocking(move || {
            let dir = dir.ok_or_else(|| "no app log directory".to_string())?;
            build_bundle(
                &files_in_window(&dir, r.since, SystemTime::now()),
                r.max_bytes,
            )
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        match built {
            Ok(bundle) => {
                let size = bundle.len();
                for msg in chunk_messages(&req.request_id, &bundle, req.chunk_bytes) {
                    if let Err(e) = send(msg).await {
                        log::warn!(
                            "Log bundle: send failed (request {}): {}",
                            req.request_id,
                            e
                        );
                        return;
                    }
                }
                log::info!(
                    "Log bundle: sent {} bytes (request {})",
                    size,
                    req.request_id
                );
            }
            Err(e) => {
                log::warn!("Log bundle: no bundle (request {}): {}", req.request_id, e);
                let _ = send(error_message(&req.request_id, &e)).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read;

    fn gunzip(b: &[u8]) -> String {
        let mut s = String::new();
        GzDecoder::new(b).read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn a_request_is_bounded_by_the_clients_limits() {
        let id = "0123456789abcdef0123456789abcdef";
        let r = parse_request(&serde_json::json!({
            "request_id": id, "since_hours": 1000, "max_bytes": 50_000_000, "chunk_bytes": 10_000_000
        }))
        .unwrap();
        assert_eq!(r.since, Duration::from_secs(MAX_SINCE_HOURS * 3600));
        assert_eq!(r.max_bytes, MAX_BUNDLE_BYTES);
        assert_eq!(r.chunk_bytes, MAX_CHUNK_BYTES);
        let d = parse_request(&serde_json::json!({ "request_id": id })).unwrap();
        assert_eq!(d.since, Duration::from_secs(24 * 3600));
        for bad in ["", "../x", "0123456789ABCDEF0123456789ABCDEF", "abc"] {
            assert!(
                parse_request(&serde_json::json!({ "request_id": bad })).is_none(),
                "{bad}"
            );
        }
    }

    #[test]
    fn only_the_log_files_of_the_window_go() {
        let dir = std::env::temp_dir().join(format!("mello-log-bundle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("mello.log.2026-10-25"), "a\n").unwrap();
        fs::write(dir.join("mello.log.2026-10-26"), "b\n").unwrap();
        fs::write(dir.join("crash.log"), "c\n").unwrap();
        let now = SystemTime::now();
        let files = files_in_window(&dir, Duration::from_secs(3600), now);
        assert_eq!(
            files.len(),
            2,
            "both written now, the crash log is not an app log"
        );
        let later = now + Duration::from_secs(48 * 3600);
        assert!(files_in_window(&dir, Duration::from_secs(24 * 3600), later).is_empty());
        let bundle = build_bundle(&files, MAX_BUNDLE_BYTES).unwrap();
        let text = gunzip(&bundle);
        assert!(text.contains("===== mello.log.2026-10-2"));
        assert!(text.contains("a\n") && text.contains("b\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn over_the_cap_the_newest_lines_stay() {
        let dir = std::env::temp_dir().join(format!("mello-log-bundle-cap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Lines that do not compress: a counter and pseudo-random hex.
        let mut s = String::new();
        let mut x: u64 = 0x9E3779B97F4A7C15;
        for i in 0..40_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            s.push_str(&format!("{i:06} {x:016x}{:016x}\n", x.rotate_left(17)));
        }
        let file = dir.join("mello.log.2026-10-26");
        fs::write(&file, &s).unwrap();
        let cap = 200_000;
        let bundle = build_bundle(&[file], cap).unwrap();
        assert!(bundle.len() <= cap, "{} over {}", bundle.len(), cap);
        let text = gunzip(&bundle);
        assert!(text.starts_with("===== older lines left out"));
        assert!(text.contains("039999 "), "the newest line stays");
        assert!(!text.contains("\n000000 "), "the oldest line goes");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunks_rebuild_the_bundle_in_order() {
        let bundle: Vec<u8> = (0..5000u32).flat_map(|i| i.to_le_bytes()).collect();
        let msgs = chunk_messages("0123456789abcdef0123456789abcdef", &bundle, 4096);
        assert_eq!(msgs.len(), 5);
        let mut back = Vec::new();
        for (i, m) in msgs.iter().enumerate() {
            let v: serde_json::Value = serde_json::from_str(m).unwrap();
            assert_eq!(v["type"], "logs_bundle");
            assert_eq!(v["data"]["seq"], i);
            assert_eq!(v["data"]["last"], i == 4);
            back.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(v["data"]["data"].as_str().unwrap())
                    .unwrap(),
            );
        }
        assert_eq!(back, bundle);
    }

    #[test]
    fn an_empty_window_is_an_error_not_an_empty_bundle() {
        assert!(build_bundle(&[], MAX_BUNDLE_BYTES).is_err());
    }

    async fn answer(dir: Option<PathBuf>, id: &str) -> Vec<serde_json::Value> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        on_request_in(
            &serde_json::json!({ "request_id": id, "since_hours": 24, "chunk_bytes": 1024 }),
            dir,
            move |text| {
                let tx = tx.clone();
                async move { tx.send(text).map_err(|e| e.to_string()) }
            },
        );
        let mut out = Vec::new();
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("the client answered within 10 s");
            let Some(text) = msg else { break };
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            let done = v["data"]["last"] == true || v["data"].get("error").is_some();
            out.push(v);
            if done {
                break;
            }
        }
        out
    }

    // The signaling handler's whole answer: the chunks of the log, or one
    // error message when there is no log, with the request id each time.
    #[tokio::test]
    async fn a_request_is_answered_with_chunks_or_an_error() {
        let dir = std::env::temp_dir().join(format!("mello-log-bundle-req-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut s = String::new();
        for i in 0..2000 {
            s.push_str(&format!(
                "{i} {:x}\n",
                (i as u64).wrapping_mul(0x9E3779B97F4A7C15)
            ));
        }
        fs::write(dir.join("mello.log.2026-10-26"), &s).unwrap();
        let id = "00112233445566778899aabbccddeeff";

        let msgs = answer(Some(dir.clone()), id).await;
        assert!(msgs.len() > 1, "a 1 KB chunk size gives several chunks");
        let mut bundle = Vec::new();
        for (i, m) in msgs.iter().enumerate() {
            assert_eq!(m["type"], "logs_bundle");
            assert_eq!(m["data"]["request_id"], id);
            assert_eq!(m["data"]["seq"], i);
            bundle.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(m["data"]["data"].as_str().unwrap())
                    .unwrap(),
            );
        }
        assert!(gunzip(&bundle).contains("1999 "));

        let empty = dir.join("empty");
        fs::create_dir_all(&empty).unwrap();
        let msgs = answer(Some(empty), id).await;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["data"]["request_id"], id);
        assert!(msgs[0]["data"]["error"]
            .as_str()
            .unwrap()
            .contains("no app log"));
        let _ = fs::remove_dir_all(&dir);
    }
}
