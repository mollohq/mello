//! The delay and rate shim of the streamed playback row (plans/GAME-CLIPS.md
//! 10.3): an HTTP/1.1 server on 127.0.0.1 that serves one clip file with
//! range requests, as the local MinIO serves an uploaded clip, through a
//! link of a fixed rate and delay. std::net only.
//!
//! | Rule | Value |
//! |---|---|
//! | `Range: bytes=a-b`, `a-` or `-n` | 206 with `Content-Range`; a range that starts past the end: 416 |
//! | No `Range`, or one the shim does not serve (several ranges, another unit, a bad range) | 200 and the whole file (RFC 9110 14.2 lets a server ignore the header) |
//! | Delay | `delay` from the request's last header line to the response's first byte |
//! | Rate | a token bucket over every response byte (head and body): `rate_bps`, at most `burst_bytes` at once, writes of `write_bytes`; one bucket for all connections, as one link |
//! | Connections | keep-alive (the HTTP/1.1 default), one thread each |
//!
//! Each response goes into a log (`Served`): when the request came, when its
//! first and last bytes went, the range and the status.

use crate::session::clock_us;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// The link of one shim.
#[derive(Clone, Copy, Debug)]
pub struct Link {
    pub rate_bps: u64,
    pub delay: Duration,
    pub burst_bytes: usize,
    pub write_bytes: usize,
}

/// The link of the streamed playback row: 20 Mbps, 50 ms (plan 6.2 and
/// 10.3).
pub const PLAYBACK_LINK: Link = Link {
    rate_bps: 20_000_000,
    delay: Duration::from_millis(50),
    burst_bytes: 16 * 1024,
    write_bytes: 4 * 1024,
};

/// The path the shim serves its file at.
pub const PATH: &str = "/clip.mp4";

/// What a `Range` header asks for, against a file of a known length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeAsk {
    /// No range, or one the shim does not serve: the whole file (200).
    Whole,
    /// Bytes `first..=last` (206).
    Part(u64, u64),
    /// A range that starts past the end (416).
    Unsatisfiable,
}

/// Parse the value of a `Range` header (RFC 9110 14.1.2): one range of
/// bytes, `a-b`, `a-` or the suffix `-n`. A last byte past the end is the
/// end.
pub fn parse_range(value: Option<&str>, length: u64) -> RangeAsk {
    let Some(value) = value else {
        return RangeAsk::Whole;
    };
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return RangeAsk::Whole;
    };
    if spec.contains(',') {
        return RangeAsk::Whole;
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return RangeAsk::Whole;
    };
    let (a, b) = (a.trim(), b.trim());
    let num = |s: &str| -> Option<u64> {
        if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    };
    if a.is_empty() {
        // The last n bytes.
        let Some(n) = num(b) else {
            return RangeAsk::Whole;
        };
        if n == 0 || length == 0 {
            return RangeAsk::Unsatisfiable;
        }
        return RangeAsk::Part(length - n.min(length), length - 1);
    }
    let Some(first) = num(a) else {
        return RangeAsk::Whole;
    };
    let last = if b.is_empty() {
        u64::MAX
    } else {
        match num(b) {
            Some(l) if l >= first => l,
            _ => return RangeAsk::Whole,
        }
    };
    if first >= length {
        return RangeAsk::Unsatisfiable;
    }
    RangeAsk::Part(first, last.min(length - 1))
}

/// A token bucket in bytes: tokens come at the link rate, up to `burst`.
#[derive(Clone, Debug)]
pub struct Pacer {
    bytes_per_s: f64,
    burst: f64,
    tokens: f64,
    last_us: u64,
}

impl Pacer {
    /// A full bucket at `now_us`.
    pub fn new(rate_bps: u64, burst_bytes: usize, now_us: u64) -> Self {
        Self {
            bytes_per_s: rate_bps.max(1) as f64 / 8.0,
            burst: burst_bytes as f64,
            tokens: burst_bytes as f64,
            last_us: now_us,
        }
    }

    /// Take `n` bytes (at most the burst) at `now_us`. Returns how long the
    /// caller waits before it sends them, µs. The bucket can go into debt,
    /// so a second caller also waits for the bytes of the first.
    pub fn take(&mut self, n: usize, now_us: u64) -> u64 {
        if now_us > self.last_us {
            let more = (now_us - self.last_us) as f64 * self.bytes_per_s / 1e6;
            self.tokens = (self.tokens + more).min(self.burst);
            self.last_us = now_us;
        }
        self.tokens -= n as f64;
        if self.tokens >= 0.0 {
            0
        } else {
            (-self.tokens * 1e6 / self.bytes_per_s).ceil() as u64
        }
    }
}

/// One response of the shim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Served {
    /// The connection (from 1) and the request on it (from 1).
    pub conn: u32,
    pub request: u32,
    pub status: u16,
    /// The bytes of the body: `first..=last` of the file; None: no body.
    pub range: Option<(u64, u64)>,
    /// The steady clock (µs): the request's last header line read, the
    /// response's first byte written, its last byte written.
    pub received_us: u64,
    pub first_byte_us: u64,
    pub done_us: u64,
}

impl Served {
    pub fn body_bytes(&self) -> u64 {
        self.range.map_or(0, |(a, b)| b - a + 1)
    }
}

struct Shared {
    body: Vec<u8>,
    link: Link,
    pacer: Mutex<Pacer>,
    stop: AtomicBool,
    log: Mutex<Vec<Served>>,
    conns: Mutex<Vec<JoinHandle<()>>>,
    next_conn: AtomicU32,
}

/// The shim, serving one file until it is dropped.
pub struct HttpShim {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

impl HttpShim {
    /// Read `file` and serve it at `PATH` on 127.0.0.1 (a free port).
    pub fn start(file: &Path, link: Link) -> Result<Self, String> {
        let body = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|e| format!("shim: bind 127.0.0.1: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| format!("shim: local address: {e}"))?;
        let shared = Arc::new(Shared {
            body,
            link,
            pacer: Mutex::new(Pacer::new(link.rate_bps, link.burst_bytes, clock_us())),
            stop: AtomicBool::new(false),
            log: Mutex::new(Vec::new()),
            conns: Mutex::new(Vec::new()),
            next_conn: AtomicU32::new(1),
        });
        let s = Arc::clone(&shared);
        let accept = std::thread::Builder::new()
            .name("http-shim-accept".into())
            .spawn(move || accept_main(&listener, &s))
            .map_err(|e| format!("shim: accept thread: {e}"))?;
        Ok(Self {
            addr,
            shared,
            accept: Some(accept),
        })
    }

    /// The URL of the file.
    pub fn url(&self) -> String {
        format!("http://{}{PATH}", self.addr)
    }

    /// The responses so far, and an empty log after.
    pub fn take_served(&self) -> Vec<Served> {
        std::mem::take(&mut *self.shared.log.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

impl Drop for HttpShim {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        // Wake the accept call.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(500));
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
        let conns =
            std::mem::take(&mut *self.shared.conns.lock().unwrap_or_else(|p| p.into_inner()));
        for t in conns {
            let _ = t.join();
        }
    }
}

fn accept_main(listener: &TcpListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        let Ok(stream) = stream else { continue };
        let conn = shared.next_conn.fetch_add(1, Ordering::Relaxed);
        let s = Arc::clone(shared);
        match std::thread::Builder::new()
            .name(format!("http-shim-{conn}"))
            .spawn(move || serve_conn(stream, &s, conn))
        {
            Ok(t) => shared
                .conns
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(t),
            Err(e) => log::warn!("shim: connection thread: {e}"),
        }
    }
}

/// One request head.
#[derive(Debug, PartialEq, Eq)]
struct Request {
    method: String,
    target: String,
    keep_alive: bool,
    range: Option<String>,
}

/// Parse a request head (without its blank line).
fn parse_head(head: &str) -> Option<Request> {
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let (method, target, version) = (first.next()?, first.next()?, first.next()?);
    let mut keep_alive = version == "HTTP/1.1";
    let mut range = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.trim().eq_ignore_ascii_case("range") {
            range = Some(value.to_owned());
        } else if name.trim().eq_ignore_ascii_case("connection") {
            if value.eq_ignore_ascii_case("close") {
                keep_alive = false;
            } else if value.eq_ignore_ascii_case("keep-alive") {
                keep_alive = true;
            }
        }
    }
    Some(Request {
        method: method.to_owned(),
        target: target.to_owned(),
        keep_alive,
        range,
    })
}

/// Requests of one connection until it closes or the shim stops.
fn serve_conn(mut stream: TcpStream, shared: &Shared, conn: u32) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let mut request = 0u32;
    loop {
        // The head: up to its blank line.
        let end = loop {
            if let Some(i) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            if pending.len() > 64 * 1024 || shared.stop.load(Ordering::Acquire) {
                return;
            }
            match stream.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => pending.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return,
            }
        };
        let received_us = clock_us();
        let head = String::from_utf8_lossy(&pending[..end]).into_owned();
        pending.drain(..end + 4);
        request += 1;
        let Some(req) = parse_head(&head) else {
            return;
        };
        if !respond(&mut stream, shared, &req, conn, request, received_us) || !req.keep_alive {
            return;
        }
    }
}

/// Write one response. False: the connection is gone.
fn respond(
    stream: &mut TcpStream,
    shared: &Shared,
    req: &Request,
    conn: u32,
    request: u32,
    received_us: u64,
) -> bool {
    let length = shared.body.len() as u64;
    let (status, reason, range) = if req.method != "GET" && req.method != "HEAD" {
        (405, "Method Not Allowed", None)
    } else if req.target != PATH {
        (404, "Not Found", None)
    } else {
        match parse_range(req.range.as_deref(), length) {
            RangeAsk::Whole if length == 0 => (200, "OK", None),
            RangeAsk::Whole => (200, "OK", Some((0, length - 1))),
            RangeAsk::Part(a, b) => (206, "Partial Content", Some((a, b))),
            RangeAsk::Unsatisfiable => (416, "Range Not Satisfiable", None),
        }
    };
    let content_length = range.map_or(0, |(a, b)| b - a + 1);
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    if status == 200 || status == 206 {
        head.push_str("Content-Type: video/mp4\r\nAccept-Ranges: bytes\r\n");
    }
    head.push_str(&format!("Content-Length: {content_length}\r\n"));
    if let Some((a, b)) = range.filter(|_| status == 206) {
        head.push_str(&format!("Content-Range: bytes {a}-{b}/{length}\r\n"));
    } else if status == 416 {
        head.push_str(&format!("Content-Range: bytes */{length}\r\n"));
    }
    head.push_str(if req.keep_alive {
        "Connection: keep-alive\r\n\r\n"
    } else {
        "Connection: close\r\n\r\n"
    });
    let body: &[u8] = match range {
        Some((a, b)) if req.method == "GET" => &shared.body[a as usize..=b as usize],
        _ => &[],
    };

    // The link's delay, then its rate.
    let due = received_us + shared.link.delay.as_micros() as u64;
    let now = clock_us();
    if due > now {
        std::thread::sleep(Duration::from_micros(due - now));
    }
    let mut first_byte_us = 0;
    let ok = [head.as_bytes(), body].iter().all(|part| {
        part.chunks(shared.link.write_bytes.max(1)).all(|chunk| {
            if shared.stop.load(Ordering::Acquire) {
                return false;
            }
            let wait = shared
                .pacer
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take(chunk.len(), clock_us());
            if wait > 0 {
                std::thread::sleep(Duration::from_micros(wait));
            }
            if first_byte_us == 0 {
                first_byte_us = clock_us();
            }
            stream.write_all(chunk).is_ok()
        })
    });
    let done_us = clock_us();
    if ok {
        shared
            .log
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Served {
                conn,
                request,
                status,
                range: if req.method == "GET" { range } else { None },
                received_us,
                first_byte_us,
                done_us,
            });
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_header_parses() {
        let n = 1000;
        assert_eq!(parse_range(None, n), RangeAsk::Whole);
        assert_eq!(parse_range(Some("bytes=0-99"), n), RangeAsk::Part(0, 99));
        assert_eq!(
            parse_range(Some(" bytes=10 - 19 "), n),
            RangeAsk::Part(10, 19)
        );
        // A last byte past the end is the end; an open range goes to it.
        assert_eq!(
            parse_range(Some("bytes=900-5000"), n),
            RangeAsk::Part(900, 999)
        );
        assert_eq!(parse_range(Some("bytes=990-"), n), RangeAsk::Part(990, 999));
        // The last n bytes.
        assert_eq!(parse_range(Some("bytes=-100"), n), RangeAsk::Part(900, 999));
        assert_eq!(parse_range(Some("bytes=-5000"), n), RangeAsk::Part(0, 999));
        // Past the end.
        assert_eq!(
            parse_range(Some("bytes=1000-1099"), n),
            RangeAsk::Unsatisfiable
        );
        assert_eq!(parse_range(Some("bytes=-0"), n), RangeAsk::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=0-0"), 0), RangeAsk::Unsatisfiable);
        // Ranges the shim does not serve: the whole file.
        assert_eq!(parse_range(Some("bytes=0-9,20-29"), n), RangeAsk::Whole);
        assert_eq!(parse_range(Some("items=0-9"), n), RangeAsk::Whole);
        assert_eq!(parse_range(Some("bytes=20-10"), n), RangeAsk::Whole);
        assert_eq!(parse_range(Some("bytes=a-10"), n), RangeAsk::Whole);
        assert_eq!(parse_range(Some("bytes=+5-10"), n), RangeAsk::Whole);
        assert_eq!(parse_range(Some("bytes=-"), n), RangeAsk::Whole);
    }

    #[test]
    fn a_request_head_parses() {
        let r =
            parse_head("GET /clip.mp4 HTTP/1.1\r\nhost: x\r\nRANGE: bytes=0-9").expect("parses");
        assert_eq!(r.method, "GET");
        assert_eq!(r.target, PATH);
        assert!(r.keep_alive);
        assert_eq!(r.range.as_deref(), Some("bytes=0-9"));
        let r = parse_head("GET / HTTP/1.1\r\nConnection: close").expect("parses");
        assert!(!r.keep_alive);
        let r = parse_head("GET / HTTP/1.0").expect("parses");
        assert!(!r.keep_alive);
        assert!(parse_head("GET").is_none());
    }

    /// Send `total` bytes in writes of `write` through the pacer on a
    /// simulated clock: (the time of each write, the end).
    fn paced(p: &mut Pacer, start_us: u64, total: usize, write: usize) -> (Vec<(u64, usize)>, u64) {
        let mut now = start_us;
        let mut sent = 0;
        let mut writes = Vec::new();
        while sent < total {
            let n = write.min(total - sent);
            now += p.take(n, now);
            writes.push((now, n));
            sent += n;
        }
        (writes, now)
    }

    #[test]
    fn a_range_of_1_mib_takes_its_time_at_20_mbps() {
        let l = PLAYBACK_LINK;
        let mut p = Pacer::new(l.rate_bps, l.burst_bytes, 0);
        let (_, end) = paced(&mut p, 0, 1 << 20, l.write_bytes);
        // The full bucket goes at once, the rest at 2.5 MB/s.
        let want = ((1 << 20) - l.burst_bytes) as f64 / 2.5e6 * 1e6;
        assert!(
            (end as f64 - want).abs() < 300.0,
            "{end} µs, want {want:.0}"
        );
        // A second range after a pause of 100 ms: the bucket holds at most
        // the burst, not 100 ms of tokens.
        let (writes, _) = paced(&mut p, end + 100_000, 64 * 1024, l.write_bytes);
        let at_once: usize = writes
            .iter()
            .filter(|(t, _)| *t == end + 100_000)
            .map(|(_, n)| n)
            .sum();
        assert_eq!(at_once, l.burst_bytes);
    }

    #[test]
    fn no_window_carries_more_than_the_rate_and_one_burst() {
        let l = PLAYBACK_LINK;
        let mut p = Pacer::new(l.rate_bps, l.burst_bytes, 0);
        let (writes, _) = paced(&mut p, 0, 3 << 20, l.write_bytes);
        for (i, &(t0, _)) in writes.iter().enumerate() {
            for window_us in [1_000u64, 10_000, 100_000] {
                let bytes: usize = writes[i..]
                    .iter()
                    .take_while(|(t, _)| *t < t0 + window_us)
                    .map(|(_, n)| n)
                    .sum();
                let cap =
                    l.burst_bytes as f64 + 2.5e6 * window_us as f64 / 1e6 + l.write_bytes as f64;
                assert!(
                    bytes as f64 <= cap,
                    "{bytes} bytes in {window_us} µs from {t0}"
                );
            }
        }
    }

    #[test]
    fn two_senders_share_the_link() {
        let mut p = Pacer::new(20_000_000, 16 * 1024, 0);
        assert_eq!(p.take(16 * 1024, 0), 0);
        // A second sender at the same time waits for 4 KB of tokens; a third
        // for 8 KB: the first one's debt counts.
        assert_eq!(p.take(4096, 0), 1639);
        assert_eq!(p.take(4096, 0), 3277);
    }

    fn get(addr: &str, head: &str) -> (String, Vec<u8>, Duration) {
        let mut s = TcpStream::connect(addr).expect("connect");
        let t = std::time::Instant::now();
        s.write_all(head.as_bytes()).expect("send");
        let mut out = Vec::new();
        s.read_to_end(&mut out).expect("read");
        let elapsed = t.elapsed();
        let split = out
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("a head");
        (
            String::from_utf8_lossy(&out[..split]).into_owned(),
            out[split + 4..].to_vec(),
            elapsed,
        )
    }

    #[test]
    fn the_shim_serves_ranges_late_and_at_its_rate() {
        let dir = std::env::temp_dir().join(format!("http-shim-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let file = dir.join("clip.mp4");
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&file, &data).expect("write");
        let shim = HttpShim::start(&file, PLAYBACK_LINK).expect("start");
        let addr = shim.url();
        let addr = addr
            .trim_start_matches("http://")
            .trim_end_matches(PATH)
            .to_owned();

        let (head, body, elapsed) = get(
            &addr,
            "GET /clip.mp4 HTTP/1.1\r\nRange: bytes=1000-100999\r\nConnection: close\r\n\r\n",
        );
        assert!(head.starts_with("HTTP/1.1 206"), "{head}");
        assert!(
            head.contains("Content-Range: bytes 1000-100999/200000"),
            "{head}"
        );
        assert_eq!(body, data[1000..101_000]);
        // 50 ms, then 100 kB less the burst at 2.5 MB/s: 34 ms.
        assert!(elapsed >= Duration::from_millis(80), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(400), "{elapsed:?}");

        let (head, body, _) = get(&addr, "GET /clip.mp4 HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, data);
        let (head, _, _) = get(
            &addr,
            "GET /clip.mp4 HTTP/1.1\r\nRange: bytes=300000-\r\nConnection: close\r\n\r\n",
        );
        assert!(head.starts_with("HTTP/1.1 416"), "{head}");
        assert!(head.contains("Content-Range: bytes */200000"), "{head}");

        // Two requests on one connection.
        let mut s = TcpStream::connect(&addr).expect("connect");
        s.write_all(
            b"GET /clip.mp4 HTTP/1.1\r\nRange: bytes=0-9\r\n\r\nGET /clip.mp4 HTTP/1.1\r\nRange: bytes=10-19\r\nConnection: close\r\n\r\n",
        )
        .expect("send");
        let mut out = Vec::new();
        s.read_to_end(&mut out).expect("read");
        let text = String::from_utf8_lossy(&out);
        assert_eq!(text.matches("HTTP/1.1 206").count(), 2, "{text}");

        let served = shim.take_served();
        assert_eq!(served.len(), 5);
        assert_eq!(served[0].range, Some((1000, 100_999)));
        assert_eq!(served[0].body_bytes(), 100_000);
        assert!(served[0].first_byte_us >= served[0].received_us + 50_000);
        assert_eq!((served[3].conn, served[3].request), (served[4].conn, 1));
        assert_eq!(served[4].request, 2);
        drop(shim);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
