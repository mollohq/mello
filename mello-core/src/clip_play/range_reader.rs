//! The bytes of a game clip for the player (plans/GAME-CLIPS.md 6.2).
//!
//! A remote clip comes in range requests of 1 MiB on one connection, a few
//! ranges ahead of the last read, into a cache file at their offsets. The
//! player's reads never wait on the network: a read of bytes that are not
//! there asks for their range and returns `READ_WAIT`, and the player asks
//! again. A range ahead that is in flight when a read waits for a range far
//! from it (a seek) stops, so the wanted range comes next. The clipper's own
//! file is read as it is.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// The bytes are not there yet (libmello's `MELLO_CLIP_READ_WAIT`).
pub const READ_WAIT: i64 = -2;
/// A read that cannot be served.
pub const READ_ERROR: i64 = -1;

/// One range request.
pub const CHUNK: u64 = 1024 * 1024;
/// Ranges fetched ahead of the last read: about 6 s at the default quality
/// (plan 6.2 asks for 3 s ahead; the margin covers a slow request).
const AHEAD_CHUNKS: u64 = 4;
const RETRIES: u32 = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The bytes of one clip.
pub trait ClipBytes: Send + Sync {
    fn length(&self) -> u64;
    /// The bytes read into `buf` (0 at the end), `READ_WAIT`, or
    /// `READ_ERROR`. Called on Media Foundation threads.
    fn read(&self, offset: u64, buf: &mut [u8]) -> i64;
}

/// The clipper's own file.
pub struct LocalClip {
    file: Mutex<File>,
    length: u64,
}

impl LocalClip {
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let length = file
            .metadata()
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len();
        Ok(Self {
            file: Mutex::new(file),
            length,
        })
    }
}

impl ClipBytes for LocalClip {
    fn length(&self) -> u64 {
        self.length
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> i64 {
        read_file_at(&self.file, offset, buf, self.length)
    }
}

fn read_file_at(file: &Mutex<File>, offset: u64, buf: &mut [u8], length: u64) -> i64 {
    if offset >= length {
        return 0;
    }
    let n = buf.len().min((length - offset) as usize);
    let mut file = file.lock().unwrap_or_else(|e| e.into_inner());
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return READ_ERROR;
    }
    match file.read(&mut buf[..n]) {
        Ok(read) => read as i64,
        Err(_) => READ_ERROR,
    }
}

/// Which 1 MiB ranges are in the cache file, and what the reads ask for.
#[derive(Debug)]
pub(crate) struct Coverage {
    have: Vec<bool>,
    /// A range a read waits for: fetched before any range ahead.
    wanted: Option<u64>,
    /// The range of the last read: the fetch keeps `AHEAD_CHUNKS` past it.
    head: u64,
}

impl Coverage {
    pub(crate) fn new(length: u64) -> Self {
        Self {
            have: vec![false; length.div_ceil(CHUNK) as usize],
            wanted: None,
            head: 0,
        }
    }

    pub(crate) fn mark(&mut self, chunk: u64) {
        if let Some(h) = self.have.get_mut(chunk as usize) {
            *h = true;
        }
        if self.wanted == Some(chunk) {
            self.wanted = None;
        }
    }

    pub(crate) fn has(&self, chunk: u64) -> bool {
        self.have.get(chunk as usize).copied().unwrap_or(false)
    }

    /// The bytes from `offset` that are in the cache, up to `len`; 0 when its
    /// range is missing (it becomes the wanted one).
    pub(crate) fn readable(&mut self, offset: u64, len: u64, length: u64) -> u64 {
        let first = offset / CHUNK;
        // The head follows the reads forward. A read far behind it is a seek
        // back: the ranges ahead start there again.
        if first > self.head || first + AHEAD_CHUNKS < self.head {
            self.head = first;
        }
        if !self.has(first) {
            self.wanted = Some(first);
            return 0;
        }
        let end = (offset + len).min(length);
        let mut until = offset;
        let mut chunk = first;
        while until < end && self.has(chunk) {
            until = ((chunk + 1) * CHUNK).min(end);
            chunk += 1;
        }
        until - offset
    }

    /// A read waits for a range more than `AHEAD_CHUNKS` from `chunk` (a
    /// seek): a fetch ahead of `chunk` stops for it. A range near `chunk`
    /// waits: `chunk` is soon read too.
    pub(crate) fn wants_far_from(&self, chunk: u64) -> bool {
        self.wanted
            .is_some_and(|w| w.abs_diff(chunk) > AHEAD_CHUNKS && !self.has(w))
    }

    /// The next range to fetch: the wanted one, else the first missing one
    /// within `AHEAD_CHUNKS` of the head.
    pub(crate) fn next(&self) -> Option<u64> {
        if let Some(w) = self.wanted.filter(|&w| !self.has(w)) {
            return Some(w);
        }
        if self.have.is_empty() {
            return None;
        }
        let last = (self.head + AHEAD_CHUNKS).min(self.have.len() as u64 - 1);
        (self.head..=last).find(|&c| !self.has(c))
    }
}

struct Shared {
    url: String,
    length: u64,
    file: Mutex<File>,
    state: Mutex<FetchState>,
    cv: Condvar,
}

struct FetchState {
    coverage: Coverage,
    stop: bool,
    failed: bool,
}

/// A remote clip, fetched in ranges on its own thread.
pub struct RangeReader {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl RangeReader {
    /// Fetches the first range, which gives the length (`Content-Range`),
    /// and starts the fetch thread. Blocking: one request.
    pub fn open(url: &str, cache_dir: &Path) -> Result<Self, String> {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .pool_max_idle_per_host(1)
            .build()
            .map_err(|e| format!("no HTTP client: {e}"))?;
        let resp = http
            .get(url)
            .header(reqwest::header::RANGE, format!("bytes=0-{}", CHUNK - 1))
            .send()
            .map_err(|e| format!("GET {url}: {e}"))?;
        let status = resp.status().as_u16();
        let length = match status {
            206 => resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(content_range_total)
                .ok_or_else(|| format!("GET {url}: no length in Content-Range"))?,
            200 => resp
                .content_length()
                .ok_or_else(|| format!("GET {url}: no length"))?,
            s => return Err(format!("GET {url}: HTTP {s}")),
        };
        let body = resp.bytes().map_err(|e| format!("GET {url}: {e}"))?;

        std::fs::create_dir_all(cache_dir).map_err(|e| format!("{}: {e}", cache_dir.display()))?;
        let path = cache_path(cache_dir, url);
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        file.set_len(length)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        file.write_all(&body)
            .map_err(|e| format!("{}: {e}", path.display()))?;

        let mut coverage = Coverage::new(length);
        // A server that ignored the range sent the whole clip.
        let got = body.len() as u64;
        for chunk in 0..length.div_ceil(CHUNK) {
            if (chunk + 1) * CHUNK <= got || got >= length {
                coverage.mark(chunk);
            }
        }
        log::info!("[clip-play] {url}: {length} bytes, the first {got} at once (HTTP {status})");
        let shared = Arc::new(Shared {
            url: url.to_string(),
            length,
            file: Mutex::new(file),
            state: Mutex::new(FetchState {
                coverage,
                stop: false,
                failed: false,
            }),
            cv: Condvar::new(),
        });
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("clip-range-fetch".into())
            .spawn(move || fetch_main(http, thread_shared))
            .map_err(|e| format!("no fetch thread: {e}"))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }
}

impl ClipBytes for RangeReader {
    fn length(&self) -> u64 {
        self.shared.length
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> i64 {
        if offset >= self.shared.length {
            return 0;
        }
        let n = {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.failed {
                return READ_ERROR;
            }
            let n = state
                .coverage
                .readable(offset, buf.len() as u64, self.shared.length);
            self.shared.cv.notify_all();
            n
        };
        if n == 0 {
            return READ_WAIT;
        }
        read_file_at(
            &self.shared.file,
            offset,
            &mut buf[..n as usize],
            self.shared.length,
        )
    }
}

impl Drop for RangeReader {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.stop = true;
        }
        self.shared.cv.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn fetch_main(http: reqwest::blocking::Client, shared: Arc<Shared>) {
    loop {
        let (chunk, ahead) = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stop || state.failed {
                    return;
                }
                if let Some(c) = state.coverage.next() {
                    // Only a range ahead gives way: a range that a read waits
                    // for always completes.
                    break (c, state.coverage.wanted != Some(c));
                }
                state = shared
                    .cv
                    .wait_timeout(state, Duration::from_millis(200))
                    .map(|(s, _)| s)
                    .unwrap_or_else(|e| e.into_inner().0);
            }
        };
        let start = chunk * CHUNK;
        let end = ((chunk + 1) * CHUNK).min(shared.length) - 1;
        let give_way = || {
            let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.stop || (ahead && state.coverage.wants_far_from(chunk))
        };
        let mut done = false;
        let mut left = false;
        for attempt in 1..=RETRIES {
            match fetch_range(&http, &shared.url, start, end, &give_way) {
                Ok(None) => {
                    log::debug!("[clip-play] range {start}-{end} left for a wanted range");
                    left = true;
                    break;
                }
                Ok(Some(bytes)) if bytes.len() as u64 == end - start + 1 => {
                    let mut file = shared.file.lock().unwrap_or_else(|e| e.into_inner());
                    if file.seek(SeekFrom::Start(start)).is_ok() && file.write_all(&bytes).is_ok() {
                        done = true;
                    }
                    break;
                }
                Ok(Some(bytes)) => log::warn!(
                    "[clip-play] range {start}-{end}: {} bytes (attempt {attempt})",
                    bytes.len()
                ),
                Err(e) => log::warn!("[clip-play] range {start}-{end}: {e} (attempt {attempt})"),
            }
            if shared.state.lock().map(|s| s.stop).unwrap_or(true) {
                return;
            }
            std::thread::sleep(Duration::from_millis(250 << attempt));
        }
        if left {
            continue;
        }
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if done {
            state.coverage.mark(chunk);
        } else {
            log::warn!("[clip-play] {}: range {start}-{end} failed", shared.url);
            state.failed = true;
        }
    }
}

/// One range, or `None` when `give_way` said to stop before its end. A
/// stopped range drops its connection: the next request opens a new one.
fn fetch_range(
    http: &reqwest::blocking::Client,
    url: &str,
    start: u64,
    end: u64,
    give_way: &dyn Fn() -> bool,
) -> Result<Option<Vec<u8>>, String> {
    let mut resp = http
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
        .send()
        .map_err(|e| e.to_string())?;
    if resp.status().as_u16() != 206 {
        return Err(format!("HTTP {}", resp.status()));
    }
    let mut bytes = Vec::with_capacity((end - start + 1) as usize);
    let mut piece = vec![0u8; 16 * 1024];
    loop {
        if give_way() {
            return Ok(None);
        }
        let n = resp.read(&mut piece).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(Some(bytes));
        }
        bytes.extend_from_slice(&piece[..n]);
    }
}

/// `bytes 0-1048575/20971520` -> 20971520.
pub(crate) fn content_range_total(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse().ok()
}

/// One cache file per clip URL.
fn cache_path(dir: &Path, url: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(url.as_bytes());
    let name: String = digest.iter().take(12).map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{name}.mp4"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_gives_the_length() {
        assert_eq!(
            content_range_total("bytes 0-1048575/20971520"),
            Some(20_971_520)
        );
        assert_eq!(content_range_total("bytes */1000"), Some(1000));
        assert_eq!(content_range_total("bytes 0-9/*"), None);
    }

    #[test]
    fn a_read_of_missing_bytes_asks_for_their_range_first() {
        let length = 10 * CHUNK + 5;
        let mut c = Coverage::new(length);
        c.mark(0);
        assert_eq!(c.readable(10, 100, length), 100);
        // A seek to range 7: its range comes before the ranges ahead of 0.
        assert_eq!(c.readable(7 * CHUNK + 3, 100, length), 0);
        assert_eq!(c.next(), Some(7));
        c.mark(7);
        assert_eq!(c.readable(7 * CHUNK + 3, 100, length), 100);
        // Then the ranges ahead of the new head.
        assert_eq!(c.next(), Some(8));
    }

    #[test]
    fn a_seek_far_away_stops_the_fetch_ahead() {
        let length = 10 * CHUNK;
        let mut c = Coverage::new(length);
        c.mark(0);
        c.readable(10, 100, length);
        assert_eq!(c.next(), Some(1), "range 1 is fetched ahead");
        assert!(!c.wants_far_from(1), "no read waits");
        // A read waits for range 3, near range 1: range 1 goes on.
        assert_eq!(c.readable(3 * CHUNK, 100, length), 0);
        assert!(!c.wants_far_from(1));
        // A seek: a read waits for range 7 while range 1 is in flight.
        assert_eq!(c.readable(7 * CHUNK, 100, length), 0);
        assert!(c.wants_far_from(1), "range 1 stops for range 7");
        assert!(
            !c.wants_far_from(7),
            "range 7 is the one the read waits for"
        );
        c.mark(7);
        assert!(!c.wants_far_from(1), "range 7 is there");
    }

    #[test]
    fn a_seek_back_moves_the_ranges_ahead_back() {
        let length = 20 * CHUNK;
        let mut c = Coverage::new(length);
        for chunk in (0..=12).filter(|&c| c != 3) {
            c.mark(chunk);
        }
        c.readable(12 * CHUNK, 1, length);
        assert_eq!(c.next(), Some(13), "ahead of a read at range 12");
        // A seek back to range 1, which is there: range 3 is ahead of it.
        assert_eq!(c.readable(CHUNK, 1, length), 1);
        assert_eq!(c.next(), Some(3));
    }

    #[test]
    fn a_read_stops_at_the_first_missing_range() {
        let length = 4 * CHUNK;
        let mut c = Coverage::new(length);
        c.mark(0);
        c.mark(1);
        assert_eq!(c.readable(CHUNK - 10, 3 * CHUNK, length), CHUNK + 10);
        assert_eq!(
            c.readable(4 * CHUNK - 1, 10, length),
            0,
            "the last range is missing"
        );
    }

    #[test]
    fn the_fetch_stays_a_few_ranges_ahead() {
        let length = 20 * CHUNK;
        let mut c = Coverage::new(length);
        for chunk in 0..=AHEAD_CHUNKS {
            c.mark(chunk);
        }
        assert_eq!(c.next(), None, "far enough ahead of a read at range 0");
        c.readable(2 * CHUNK, 1, length);
        assert_eq!(c.next(), Some(AHEAD_CHUNKS + 1));
    }

    /// A clip in the local MinIO, read through the reader as the player reads
    /// it: from the start, then a jump, waiting where bytes are not there.
    /// `MELLO_LIVE_CLIP_URL` is the clip, `MELLO_LIVE_CLIP` its local copy.
    #[test]
    #[ignore = "needs the local stack and an uploaded clip"]
    fn a_remote_clip_reads_as_its_file() {
        let (Ok(url), Ok(local)) = (
            std::env::var("MELLO_LIVE_CLIP_URL"),
            std::env::var("MELLO_LIVE_CLIP"),
        ) else {
            panic!("set MELLO_LIVE_CLIP_URL and MELLO_LIVE_CLIP");
        };
        assert!(url.starts_with("http://localhost:"), "the local stack only");
        let want = std::fs::read(&local).expect("the local copy");
        let dir = std::env::temp_dir().join("mello-clip-play-live");
        let reader = RangeReader::open(&url, &dir).expect("open");
        assert_eq!(reader.length(), want.len() as u64);

        let read_all_from = |start: u64| {
            let mut got = Vec::new();
            let mut at = start;
            let mut waits = 0;
            let mut buf = vec![0u8; 64 * 1024];
            while at < reader.length() {
                match reader.read(at, &mut buf) {
                    READ_WAIT => {
                        waits += 1;
                        assert!(waits < 2000, "the bytes never came");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    n if n > 0 => {
                        got.extend_from_slice(&buf[..n as usize]);
                        at += n as u64;
                    }
                    n => panic!("read at {at}: {n}"),
                }
            }
            (got, waits)
        };
        // A jump past the ranges ahead first, as a seek does.
        let jump = (want.len() as u64 / CHUNK).saturating_sub(1) * CHUNK;
        let (tail, waits) = read_all_from(jump);
        assert_eq!(tail, want[jump as usize..], "the bytes after the jump");
        assert!(waits > 0, "the jumped-to range was not fetched first");
        let (all, _) = read_all_from(0);
        assert_eq!(all, want, "the whole clip");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_local_clip_reads_its_file() {
        let dir = std::env::temp_dir().join(format!("mello-clip-play-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("clip.mp4");
        std::fs::write(&path, (0..100u8).collect::<Vec<_>>()).expect("write");
        let clip = LocalClip::open(&path).expect("open");
        assert_eq!(clip.length(), 100);
        let mut buf = [0u8; 10];
        assert_eq!(clip.read(95, &mut buf), 5);
        assert_eq!(&buf[..5], &[95, 96, 97, 98, 99]);
        assert_eq!(clip.read(100, &mut buf), 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
