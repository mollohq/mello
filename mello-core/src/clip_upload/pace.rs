//! The upload pace (CLIPS.md 15.7; 12-STREAMING.md 3.7, G5 and G8): the rate
//! cap that the state of the PC sets, and a reader that holds the MP4 bytes
//! to that rate in small writes.
//!
//! The cap: no cap with no game, 3 Mbps while a game runs, and the stream's
//! budget while the user streams (`stream/upload_budget.rs`, G2 to G4). A
//! budget of 0 pauses the upload. The burst is the risk to a stream, not the
//! average rate, so the reader sends at most one 16 KB write at a time and
//! keeps no credit from an idle time.

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The cap while a game runs and no stream runs (G8).
pub const GAME_RATE_BPS: u64 = 3_000_000;
/// The poster goes at once in every state (G1): it never pauses and never
/// goes slower than this. It is under 1 MB, so it takes at most 8 s.
pub const POSTER_FLOOR_BPS: u64 = 1_000_000;
/// The largest write the reader gives at one time (G5).
pub const WRITE_BYTES: usize = 16 * 1024;
/// A pause that lasts this long ends the part in flight. A pause then costs
/// at most that part (G6), and no request waits open for minutes.
pub const PAUSE_ABORT: Duration = Duration::from_secs(10);
/// How often a paused or waiting reader looks at the cap and the stop flag.
const POLL: Duration = Duration::from_millis(100);

const NO_STREAM: u64 = u64::MAX;

/// The inputs of the cap, set by the stream manager and the client.
pub struct UploadPace {
    stream_bps: AtomicU64,
    game_running: AtomicBool,
}

impl UploadPace {
    pub const fn new() -> Self {
        Self {
            stream_bps: AtomicU64::new(NO_STREAM),
            game_running: AtomicBool::new(false),
        }
    }

    /// The stream's budget for the upload, in bps. 0 pauses the upload.
    pub fn set_stream_budget(&self, bps: u64) {
        self.stream_bps
            .store(bps.min(NO_STREAM - 1), Ordering::Relaxed);
    }

    /// The stream ended: the game rule applies again.
    pub fn clear_stream(&self) {
        self.stream_bps.store(NO_STREAM, Ordering::Relaxed);
    }

    pub fn set_game_running(&self, running: bool) {
        self.game_running.store(running, Ordering::Relaxed);
    }

    /// The cap now: `None` is no cap, `Some(0)` is a pause.
    pub fn rate_bps(&self) -> Option<u64> {
        let stream = self.stream_bps.load(Ordering::Relaxed);
        if stream != NO_STREAM {
            return Some(stream);
        }
        self.game_running
            .load(Ordering::Relaxed)
            .then_some(GAME_RATE_BPS)
    }
}

impl Default for UploadPace {
    fn default() -> Self {
        Self::new()
    }
}

static PACE: UploadPace = UploadPace::new();

/// The one pace of the process: one upload runs at a time.
pub fn pace() -> &'static UploadPace {
    &PACE
}

/// The cap for one request: the poster has a floor, the MP4 has none.
pub(crate) fn with_floor(rate: Option<u64>, floor_bps: u64) -> Option<u64> {
    rate.map(|bps| bps.max(floor_bps))
}

/// The pacer: each write waits until the writes before it have had their
/// time at the rate. An idle time gives no credit, so there is no burst.
#[derive(Debug, Default)]
pub(crate) struct Bucket {
    next_at: Option<Instant>,
}

impl Bucket {
    /// The wait before `bytes` may go at `bps`.
    pub(crate) fn take(&mut self, now: Instant, bytes: usize, bps: u64) -> Duration {
        let start = match self.next_at {
            Some(at) if at > now => at,
            _ => now,
        };
        let cost = Duration::from_secs_f64(bytes as f64 * 8.0 / bps.max(1) as f64);
        self.next_at = Some(start + cost);
        start - now
    }
}

/// The cap that the reader reads before each write.
pub(crate) type RateSource = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

/// A reader over one part of a file that gives its bytes at the cap.
pub(crate) struct PacedReader<R> {
    inner: R,
    rate: RateSource,
    floor_bps: u64,
    stop: Arc<AtomicBool>,
    bucket: Bucket,
}

impl<R: Read> PacedReader<R> {
    pub(crate) fn new(inner: R, rate: RateSource, floor_bps: u64, stop: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            rate,
            floor_bps,
            stop,
            bucket: Bucket::default(),
        }
    }

    fn stopped(&self) -> io::Result<()> {
        if self.stop.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "upload stopped"));
        }
        Ok(())
    }

    /// Waits while the cap is a pause, at most `PAUSE_ABORT`. Returns the cap.
    fn wait_for_rate(&self) -> io::Result<Option<u64>> {
        let mut paused_since: Option<Instant> = None;
        loop {
            self.stopped()?;
            let rate = with_floor((self.rate)(), self.floor_bps);
            if rate != Some(0) {
                return Ok(rate);
            }
            let since = *paused_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= PAUSE_ABORT {
                return Err(io::Error::other("upload paused"));
            }
            std::thread::sleep(POLL);
        }
    }

    fn sleep(&self, mut wait: Duration) -> io::Result<()> {
        while !wait.is_zero() {
            self.stopped()?;
            let slice = wait.min(POLL);
            std::thread::sleep(slice);
            wait -= slice;
        }
        Ok(())
    }
}

impl<R: Read> Read for PacedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let rate = self.wait_for_rate()?;
        let len = buf.len().min(WRITE_BYTES);
        let n = self.inner.read(&mut buf[..len])?;
        if n == 0 {
            return Ok(0);
        }
        if let Some(bps) = rate {
            let wait = self.bucket.take(Instant::now(), n, bps);
            self.sleep(wait)?;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cap_follows_the_stream_then_the_game() {
        let pace = UploadPace::new();
        assert_eq!(pace.rate_bps(), None, "no game: no cap");
        pace.set_game_running(true);
        assert_eq!(pace.rate_bps(), Some(GAME_RATE_BPS));
        pace.set_stream_budget(500_000);
        assert_eq!(pace.rate_bps(), Some(500_000), "the stream rules");
        pace.set_stream_budget(0);
        assert_eq!(pace.rate_bps(), Some(0), "a pause");
        pace.clear_stream();
        assert_eq!(pace.rate_bps(), Some(GAME_RATE_BPS));
        pace.set_game_running(false);
        assert_eq!(pace.rate_bps(), None);
    }

    #[test]
    fn the_poster_never_pauses() {
        assert_eq!(
            with_floor(Some(0), POSTER_FLOOR_BPS),
            Some(POSTER_FLOOR_BPS)
        );
        assert_eq!(
            with_floor(Some(3_000_000), POSTER_FLOOR_BPS),
            Some(3_000_000)
        );
        assert_eq!(with_floor(None, POSTER_FLOOR_BPS), None);
        assert_eq!(with_floor(Some(0), 0), Some(0));
    }

    #[test]
    fn the_bucket_spaces_writes_at_the_rate() {
        let mut bucket = Bucket::default();
        let t0 = Instant::now();
        // 16 KB at 1 Mbps is 131 ms.
        assert_eq!(bucket.take(t0, WRITE_BYTES, 1_000_000), Duration::ZERO);
        let wait = bucket.take(t0, WRITE_BYTES, 1_000_000);
        assert_eq!(wait.as_millis(), 131);
        // The third write waits for both before it.
        let wait = bucket.take(t0, WRITE_BYTES, 1_000_000);
        assert_eq!(wait.as_millis(), 262);
    }

    #[test]
    fn an_idle_time_gives_no_burst() {
        let mut bucket = Bucket::default();
        let t0 = Instant::now();
        bucket.take(t0, WRITE_BYTES, 1_000_000);
        // Ten seconds of nothing, then two writes: the second still waits.
        let later = t0 + Duration::from_secs(10);
        assert_eq!(bucket.take(later, WRITE_BYTES, 1_000_000), Duration::ZERO);
        assert_eq!(bucket.take(later, WRITE_BYTES, 1_000_000).as_millis(), 131);
    }

    #[test]
    fn the_reader_gives_small_writes_and_all_bytes() {
        let data: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        let stop = Arc::new(AtomicBool::new(false));
        let mut reader = PacedReader::new(&data[..], Arc::new(|| None), 0, stop);
        let mut buf = vec![0u8; 64 * 1024];
        let mut out = Vec::new();
        loop {
            let n = reader.read(&mut buf).expect("read");
            if n == 0 {
                break;
            }
            assert!(n <= WRITE_BYTES, "a write of {n} bytes");
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, data);
    }

    #[test]
    fn a_stop_ends_the_read() {
        let stop = Arc::new(AtomicBool::new(true));
        let mut reader = PacedReader::new(&[1u8, 2, 3][..], Arc::new(|| Some(0)), 0, stop);
        let err = reader.read(&mut [0u8; 8]).expect_err("stopped");
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }
}
