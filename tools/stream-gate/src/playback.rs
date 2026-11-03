//! The streamed playback row of the replay lane (CLIPS.md 15.13):
//! a clip MP4 of the run plays over HTTP as a viewer plays it from the feed.
//!
//! The path. `http_shim` serves the MP4 on 127.0.0.1 through a link of
//! 20 Mbps and 50 ms (`PLAYBACK_LINK`) -> mello-core's `RangeReader` (1 MiB
//! ranges, a cache file) -> `clip_play::read_clip`, the client's adapter ->
//! `mello_test_clip_player_start` (the Media Foundation demux, the viewer's
//! decoder, the CPU frame callback at each frame's present time). The
//! player's clock is its sound, and with the test audio backend no device
//! thread takes it: `SoundPull` takes the voice output 10 ms at a time
//! (`mello_voice_test_pull_output`) on its own thread, paced on the steady
//! clock, as the device thread does.
//!
//! Two plays over HTTP, each with an empty cache, and one from the file:
//! - play: from the start call to the end of the clip, in real time.
//! - seek: a second play that seeks to 20 s right after its first frame.
//!   The range of 20 s is not in the cache then, so the seek waits for the
//!   network as a viewer's first scrub does.
//! - local: the file itself (`LocalClip`) to the first frame: the player's
//!   own start, without the network (`playback_local_first_frame_ms`).
//!
//! The details go to `<run>/playback/`: frames.csv (every shown frame),
//! state.csv (every state read), requests.csv (the shim's responses),
//! read_waits.csv and reads.csv (the player's read calls).
//! The load guard covers the two plays over HTTP.
//!
//! The truth. The clip's units, decoded one at a time (`replay::decode`),
//! give the frame code of every unit. A shown frame's loop index unwraps to
//! the absolute frame nearest to its present time (`replay::frame_order`).
//! The clip loop is 20 s (1200 frames at 60 fps), so the frame of 20 s has
//! the loop index of the clip's first frame: after the seek call, a frame
//! counts as one from the seek once the player's position jumped (its state
//! shows it right after the flush of the old frames), or once a frame of the
//! seek's GOP showed.
//!
//! | Metric | Definition | Gate |
//! |---|---|---|
//! | `playback_first_frame_ms` | the start call (`RangeReader::open`, as `ClipPlayback::start`) to the first frame shown | 1000 ms |
//! | `playback_buffering_ms` | time the player's state says buffering after the first frame, to the end (the state read every 5 ms) | 0 |
//! | `playback_frames_out_of_order` | shown frames whose code goes back or repeats | 0 |
//! | `playback_frames_skipped` | units of the clip that never showed | 0 |
//! | `playback_frames_unreadable` | shown frames whose code does not read | 0 |
//! | `playback_seek_ms` | the seek call to the first frame at or after the clip frame of 20 s | 1000 ms |

use crate::code::{self, FrameCode};
use crate::http_shim::{self, HttpShim, Served};
use crate::load;
use crate::profile::Gate;
use crate::replay::{self, Frame};
use crate::report::Metrics;
use crate::session::{clock_us, Ctx};
use mello_core::clip_play::range_reader::{ClipBytes, LocalClip, RangeReader, READ_WAIT};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The row's name in gates.json and results.json.
pub const ROW: &str = "playback";
/// The press whose clip the row plays.
pub const SOURCE_PRESS: &str = "press-35s";
/// The seek of the second play.
pub const SEEK_TO_MS: u64 = 20_000;
/// The state read interval.
const POLL: Duration = Duration::from_millis(5);
/// One sound pull: 10 ms at 48 kHz, as the device thread.
const PULL_FRAMES: usize = 480;
const PULL_PERIOD: Duration = Duration::from_millis(10);
/// The seek play stops this far past the target, or this long after the
/// seek.
const SEEK_PLAY_PAST_MS: u64 = 2_000;
const SEEK_PLAY_MAX: Duration = Duration::from_secs(8);
/// A play that does not end this long after the clip's length failed.
const PLAY_SLACK: Duration = Duration::from_secs(20);

// ---- the truth -------------------------------------------------------------------

/// The clip as its units decode one at a time: what the plays are checked
/// against.
#[derive(Clone, Debug, Default)]
pub struct Truth {
    /// The time of each unit in the clip, µs.
    pub unit_us: Vec<i64>,
    /// The absolute frame of each unit; None: its code does not read.
    pub abs: Vec<Option<i64>>,
    pub fps: u32,
    /// The loop of the reference clip, in frames.
    pub clip_frames: u32,
    pub decode_s: f64,
}

impl Truth {
    /// Decode every unit of `mp4`. `first_expect`: the absolute frame
    /// expected at the clip's start (`replay::frame_order`).
    pub fn read(
        ctx: &Ctx,
        mp4: &Path,
        size: (u32, u32),
        fps: u32,
        clip_frames: u32,
        first_expect: i64,
    ) -> Result<Self, String> {
        let (units, _, _) = replay::read_units(mp4)?;
        let dec = replay::decode(ctx, &units, size.0, size.1, fps, None)?;
        if dec.frames.len() != units.len() {
            return Err(format!(
                "{}: {} units decode to {} frames",
                mp4.display(),
                units.len(),
                dec.frames.len()
            ));
        }
        let order = replay::frame_order(&dec.frames, first_expect, clip_frames, fps);
        Ok(Self {
            unit_us: units.iter().map(|u| u.time_us).collect(),
            abs: order.abs,
            fps,
            clip_frames,
            decode_s: dec.seconds,
        })
    }

    pub fn units(&self) -> usize {
        self.unit_us.len()
    }

    /// The unit of each absolute frame.
    pub fn unit_of(&self) -> BTreeMap<i64, usize> {
        self.abs
            .iter()
            .enumerate()
            .filter_map(|(u, a)| a.map(|a| (a, u)))
            .collect()
    }

    /// The unit on screen at clip time `t_us`: the last one at or before it.
    pub fn unit_at(&self, t_us: i64) -> Option<usize> {
        self.unit_us.iter().rposition(|&u| u <= t_us)
    }

    /// The clip's length: the time of its last unit plus one frame, µs.
    pub fn length_us(&self) -> i64 {
        self.unit_us
            .last()
            .map_or(0, |t| t + 1_000_000 / i64::from(self.fps.max(1)))
    }
}

// ---- one play ----------------------------------------------------------------------

/// One frame the player showed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shown {
    /// Its present time (steady clock, µs).
    pub at_us: u64,
    pub code: Option<FrameCode>,
}

/// One read of the player's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Poll {
    pub at_us: u64,
    pub position_ms: u64,
    pub buffering: bool,
    pub ended: bool,
    pub failed: bool,
}

/// A time a read of the player waited: the range reader did not have its
/// bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadWait {
    pub from_us: u64,
    pub to_us: u64,
    pub offset: u64,
}

/// What one play did.
#[derive(Clone, Debug, Default)]
pub struct Play {
    /// The start call: just before `RangeReader::open` (or
    /// `LocalClip::open`), as in `ClipPlayback::start`.
    pub start_us: u64,
    /// The open returned: over HTTP, the first range is in the cache.
    pub opened_us: u64,
    /// `mello_test_clip_player_start` returned.
    pub player_us: u64,
    pub seek_us: Option<u64>,
    pub stop_us: u64,
    pub shown: Vec<Shown>,
    /// Frames of another size than the clip: their code is not read.
    pub wrong_size: u64,
    pub polls: Vec<Poll>,
    pub waits: Vec<ReadWait>,
    /// The first read calls that did not wait.
    pub reads: Vec<ReadCall>,
    pub served: Vec<Served>,
    pub timed_out: bool,
    /// CPU that other processes used during the play, %.
    pub other_cpu_pct: Option<f64>,
}

/// How a play goes.
#[derive(Clone, Copy, Debug)]
pub enum Plan {
    /// To the end of the clip; failed after this long.
    ToTheEnd(Duration),
    /// A seek to this position (ms) right after the first frame.
    SeekAfterFirstFrame(u64),
    /// Until the first frame.
    FirstFrame,
}

/// Where a play reads the clip.
#[derive(Clone, Copy)]
pub enum Source<'a> {
    /// Over HTTP: the shim and a new range reader, its cache file in the
    /// directory.
    Shim(&'a HttpShim, &'a Path),
    /// The file itself (`LocalClip`, as the clipper plays it).
    Local(&'a Path),
}

/// The CPU frame callback's target: each frame's code, read on the present
/// thread (the code is 96 blocks of the top-left corner; no copy).
struct Tap {
    width: u32,
    height: u32,
    shown: Mutex<Vec<Shown>>,
    count: AtomicU64,
    wrong_size: AtomicU64,
}

unsafe extern "C" fn on_frame(user: *mut c_void, rgba: *const u8, w: u32, h: u32, ts: u64) {
    // SAFETY: user is the Tap, which outlives the player (stopped first).
    let tap = unsafe { &*(user as *const Tap) };
    let code = if !rgba.is_null() && w == tap.width && h == tap.height {
        // SAFETY: libmello hands w * h RGBA pixels for the duration of the call.
        let px = unsafe { std::slice::from_raw_parts(rgba, (w as usize) * (h as usize) * 4) };
        code::read_rgba(px, w as usize)
    } else {
        tap.wrong_size.fetch_add(1, Ordering::Relaxed);
        None
    };
    tap.shown
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(Shown { at_us: ts, code });
    tap.count.fetch_add(1, Ordering::Release);
}

/// One call of the player's read callback that did not wait: when, where,
/// how many bytes it asked for and how many it got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadCall {
    pub at_us: u64,
    pub offset: u64,
    pub asked: u64,
    pub got: i64,
}

/// The read calls a play keeps (the first ones show how the demux opens).
const READ_LOG: usize = 400;

/// The reads that waited.
#[derive(Default)]
struct WaitLog {
    /// The wait that runs: (from, offset).
    running: Option<(u64, u64)>,
    done: Vec<ReadWait>,
}

/// The clip's bytes with a log of the reads.
struct Tapped {
    inner: Box<dyn ClipBytes>,
    waits: Mutex<WaitLog>,
    reads: Mutex<Vec<ReadCall>>,
}

impl ClipBytes for Tapped {
    fn length(&self) -> u64 {
        self.inner.length()
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> i64 {
        let r = self.inner.read(offset, buf);
        if r != READ_WAIT {
            let mut reads = self.reads.lock().unwrap_or_else(|p| p.into_inner());
            if reads.len() < READ_LOG {
                reads.push(ReadCall {
                    at_us: clock_us(),
                    offset,
                    asked: buf.len() as u64,
                    got: r,
                });
            }
        }
        let mut w = self.waits.lock().unwrap_or_else(|p| p.into_inner());
        if r == READ_WAIT {
            if w.running.is_none() {
                w.running = Some((clock_us(), offset));
            }
        } else if let Some((from_us, offset)) = w.running.take() {
            w.done.push(ReadWait {
                from_us,
                to_us: clock_us(),
                offset,
            });
        }
        r
    }
}

fn player_state(player: *mut mello_sys::MelloClipPlayer) -> Poll {
    // SAFETY: plain data, filled by the call.
    let mut s: mello_sys::MelloClipPlayerState = unsafe { std::mem::zeroed() };
    // SAFETY: player is live until mello_clip_player_stop.
    unsafe { mello_sys::mello_clip_player_get_state(player, &mut s) };
    Poll {
        at_us: clock_us(),
        position_ms: s.position_ms,
        buffering: s.buffering,
        ended: s.ended,
        failed: s.failed,
    }
}

/// Play the clip once through the clip player. A `SoundPull` must run.
pub fn play(ctx: &Ctx, source: Source<'_>, size: (u32, u32), plan: Plan) -> Result<Play, String> {
    if let Source::Shim(shim, _) = source {
        let _ = shim.take_served();
    }
    let load_start = load::sample(&[]);
    let start_us = clock_us();
    let inner: Box<dyn ClipBytes> = match source {
        Source::Shim(shim, cache_dir) => Box::new(RangeReader::open(&shim.url(), cache_dir)?),
        Source::Local(path) => Box::new(LocalClip::open(path)?),
    };
    let opened_us = clock_us();
    let tapped = Arc::new(Tapped {
        inner,
        waits: Mutex::new(WaitLog::default()),
        reads: Mutex::new(Vec::new()),
    });
    let length = tapped.length();
    let bytes: Arc<dyn ClipBytes> = tapped.clone();
    let user = Box::into_raw(Box::new(bytes));
    let tap = Box::new(Tap {
        width: size.0,
        height: size.1,
        shown: Mutex::new(Vec::new()),
        count: AtomicU64::new(0),
        wrong_size: AtomicU64::new(0),
    });
    let config = mello_sys::MelloClipPlayerConfig {
        length,
        read: Some(mello_core::clip_play::read_clip),
        user_data: user as *mut c_void,
    };
    // SAFETY: ctx is live; `user` and `tap` outlive the player (stopped
    // below before either is freed).
    let player = unsafe {
        mello_sys::mello_test_clip_player_start(
            ctx.0,
            &config,
            Some(on_frame),
            &*tap as *const Tap as *mut c_void,
        )
    };
    let player_us = clock_us();
    if player.is_null() {
        // SAFETY: from Box::into_raw above; the player never started.
        drop(unsafe { Box::from_raw(user) });
        return Err("mello_test_clip_player_start: no clip player on this platform".into());
    }

    let deadline = Instant::now()
        + match plan {
            Plan::ToTheEnd(t) => t,
            Plan::SeekAfterFirstFrame(_) => SEEK_PLAY_MAX + PLAY_SLACK,
            Plan::FirstFrame => PLAY_SLACK,
        };
    let mut polls = Vec::new();
    let mut seek_us = None;
    let mut timed_out = false;
    loop {
        let s = player_state(player);
        polls.push(s);
        if s.failed || s.ended {
            break;
        }
        let shown = tap.count.load(Ordering::Acquire);
        if matches!(plan, Plan::FirstFrame) && shown > 0 {
            break;
        }
        if let Plan::SeekAfterFirstFrame(to_ms) = plan {
            if seek_us.is_none() && shown > 0 {
                // SAFETY: player is live.
                unsafe { mello_sys::mello_clip_player_seek(player, to_ms) };
                seek_us = Some(clock_us());
            }
            if let Some(t) = seek_us {
                if s.position_ms >= to_ms + SEEK_PLAY_PAST_MS
                    || clock_us() > t + SEEK_PLAY_MAX.as_micros() as u64
                {
                    break;
                }
            }
        }
        if Instant::now() > deadline {
            timed_out = true;
            break;
        }
        // Tight until the first frame: the seek follows it at once.
        std::thread::sleep(if shown == 0 {
            Duration::from_millis(1)
        } else {
            POLL
        });
    }
    // SAFETY: player from start above; the read callback and the frame
    // callback are not called after it returns.
    unsafe { mello_sys::mello_clip_player_stop(player) };
    let stop_us = clock_us();
    // SAFETY: from Box::into_raw above; the player is stopped.
    drop(unsafe { Box::from_raw(user) });
    let shown = std::mem::take(&mut *tap.shown.lock().unwrap_or_else(|p| p.into_inner()));
    let wrong_size = tap.wrong_size.load(Ordering::Relaxed);
    drop(tap);
    let waits = {
        let mut w = tapped.waits.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((from_us, offset)) = w.running.take() {
            w.done.push(ReadWait {
                from_us,
                to_us: stop_us,
                offset,
            });
        }
        std::mem::take(&mut w.done)
    };
    let reads = std::mem::take(&mut *tapped.reads.lock().unwrap_or_else(|p| p.into_inner()));
    // The range reader stops its fetch thread here.
    drop(tapped);
    let served = match source {
        Source::Shim(shim, _) => shim.take_served(),
        Source::Local(_) => Vec::new(),
    };
    Ok(Play {
        start_us,
        opened_us,
        player_us,
        seek_us,
        stop_us,
        shown,
        wrong_size,
        polls,
        waits,
        reads,
        served,
        timed_out,
        other_cpu_pct: load::other_pct(load_start, load::sample(&[])),
    })
}

/// What the sound pull did.
#[derive(Clone, Copy, Debug, Default)]
pub struct PullStats {
    pub pulls: u64,
    /// Pulls with sound in them (a clip or a peer).
    pub audible: u64,
    /// The latest pull after its due time, µs.
    pub late_max_us: u64,
}

/// The device thread of the test audio backend: the voice output, 10 ms at
/// a time on the steady clock, for as long as it runs.
pub struct SoundPull {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<PullStats>>,
}

impl SoundPull {
    pub fn start(ctx: Arc<Ctx>) -> Result<Self, String> {
        let mut probe = [0i16; PULL_FRAMES];
        // SAFETY: ctx is live; the buffer holds PULL_FRAMES samples.
        let r = unsafe {
            mello_sys::mello_voice_test_pull_output(ctx.0, probe.as_mut_ptr(), PULL_FRAMES as i32)
        };
        if r < 0 {
            return Err(
                "mello_voice_test_pull_output: the context has no test audio backend (MELLO_AUDIO_BACKEND=test before mello_init)"
                    .into(),
            );
        }
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("playback-sound-pull".into())
            .spawn(move || {
                let mut buf = [0i16; PULL_FRAMES];
                let mut stats = PullStats::default();
                let mut next = Instant::now();
                while !s.load(Ordering::Acquire) {
                    next += PULL_PERIOD;
                    let now = Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    }
                    let late = Instant::now().saturating_duration_since(next);
                    stats.late_max_us = stats.late_max_us.max(late.as_micros() as u64);
                    // SAFETY: ctx is live while the thread runs (the Arc);
                    // the buffer holds PULL_FRAMES samples.
                    let r = unsafe {
                        mello_sys::mello_voice_test_pull_output(
                            ctx.0,
                            buf.as_mut_ptr(),
                            PULL_FRAMES as i32,
                        )
                    };
                    stats.pulls += 1;
                    stats.audible += u64::from(r > 0);
                }
                stats
            })
            .map_err(|e| format!("sound pull thread: {e}"))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    pub fn finish(mut self) -> PullStats {
        self.stop.store(true, Ordering::Release);
        self.thread
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for SoundPull {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---- analysis ----------------------------------------------------------------------

/// The frames of a play to its end, against the truth.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayOrder {
    /// The absolute frame of each shown frame.
    pub abs: Vec<Option<i64>>,
    pub unreadable: u64,
    pub out_of_order: u64,
    /// Frames between two shown frames that did not show.
    pub missing_between: u64,
    /// Units of the clip that never showed.
    pub skipped: usize,
    /// Shown frames that are no unit of the clip.
    pub not_in_clip: u64,
    /// The unit of the first shown frame.
    pub first_unit: Option<usize>,
}

/// The order of a play's frames. A frame's loop index unwraps to the pass
/// nearest to the first unit's frame plus its present time since the first
/// frame.
pub fn play_order(shown: &[Shown], truth: &Truth) -> PlayOrder {
    let first_expect = truth.abs.first().copied().flatten().unwrap_or(0);
    let t0 = shown.first().map_or(0, |s| s.at_us);
    let frames: Vec<Frame> = shown
        .iter()
        .map(|s| Frame {
            time_us: s.at_us.saturating_sub(t0) as i64,
            code: s.code,
        })
        .collect();
    let o = replay::frame_order(&frames, first_expect, truth.clip_frames, truth.fps);
    let unit_of = truth.unit_of();
    let mut seen = BTreeSet::new();
    let mut not_in_clip = 0;
    for a in o.abs.iter().flatten() {
        match unit_of.get(a) {
            Some(&u) => {
                seen.insert(u);
            }
            None => not_in_clip += 1,
        }
    }
    PlayOrder {
        first_unit: o
            .abs
            .first()
            .copied()
            .flatten()
            .and_then(|a| unit_of.get(&a).copied()),
        abs: o.abs,
        unreadable: o.unreadable,
        out_of_order: o.out_of_order,
        missing_between: o.missing,
        skipped: truth.units().saturating_sub(seen.len()),
        not_in_clip,
    }
}

/// The player's buffering state after `from_us`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Buffering {
    /// Time the state said buffering.
    pub ms: f64,
    /// Times the state turned to buffering.
    pub events: u64,
    /// Of `ms`: the position moved (the sound played on), or held.
    pub clock_ran_ms: f64,
    pub clock_held_ms: f64,
    /// The longest time the position held, after it first moved.
    pub held_max_ms: f64,
}

/// The buffering state between polls: each interval counts with the state
/// at its start. The end (ended or failed) stops the count.
pub fn buffering(polls: &[Poll], from_us: u64) -> Buffering {
    let mut b = Buffering::default();
    let mut was = false;
    let mut moved = false;
    let mut held_since: Option<u64> = None;
    for w in polls.windows(2) {
        let (p, q) = (w[0], w[1]);
        if p.at_us < from_us {
            continue;
        }
        if p.ended || p.failed {
            break;
        }
        let dt = (q.at_us - p.at_us) as f64 / 1000.0;
        let advanced = q.position_ms > p.position_ms;
        if p.buffering {
            b.ms += dt;
            if advanced {
                b.clock_ran_ms += dt;
            } else {
                b.clock_held_ms += dt;
            }
            if !was {
                b.events += 1;
            }
        }
        was = p.buffering;
        if advanced {
            moved = true;
            held_since = None;
        } else if moved && !q.ended {
            let since = *held_since.get_or_insert(p.at_us);
            b.held_max_ms = b.held_max_ms.max((q.at_us - since) as f64 / 1000.0);
        }
    }
    b
}

/// Where a shown frame of the seek play comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Before the seek call.
    Before,
    /// After the seek call, from the old position.
    Old,
    /// From the seek.
    New,
}

/// The seek of the second play.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Seek {
    pub target_unit: usize,
    pub target_abs: i64,
    /// The first frame from the seek: (present time, absolute frame).
    pub first_new: Option<(u64, Option<i64>)>,
    /// The first frame at or after the target.
    pub reached: Option<(u64, i64)>,
    /// Frames from the old position shown after the seek call.
    pub old_after_seek: u64,
    /// Frames from the seek whose code goes back or repeats.
    pub out_of_order: u64,
    /// The phase and absolute frame of every shown frame.
    pub frames: Vec<(Phase, Option<i64>)>,
}

/// Find the frame of `seek_ms` after the seek call. A frame is from the seek
/// once a poll after the call saw the position jump (within 1 s before the
/// target, or past it), or once a frame of the seek's GOP (up to two GOPs
/// before the target) showed; every frame after one from the seek is from
/// the seek (the flush drops the old ones).
pub fn seek_result(
    shown: &[Shown],
    polls: &[Poll],
    seek_us: u64,
    seek_ms: u64,
    truth: &Truth,
) -> Option<Seek> {
    let target_unit = truth.unit_at(seek_ms as i64 * 1000)?;
    let target_abs = truth.abs.get(target_unit).copied().flatten()?;
    let jump_us = polls
        .iter()
        .find(|p| p.at_us >= seek_us && p.position_ms + 1000 >= seek_ms)
        .map(|p| p.at_us);
    let window = 2 * i64::from(truth.fps);
    let mut s = Seek {
        target_unit,
        target_abs,
        ..Default::default()
    };
    let mut new_seen = false;
    let mut last_new: Option<i64> = None;
    for f in shown {
        if f.at_us <= seek_us {
            s.frames.push((Phase::Before, None));
            continue;
        }
        let abs = f
            .code
            .map(|c| replay::unwrap_nearest(c.index, target_abs, truth.clip_frames));
        let in_gop = abs.is_some_and(|a| a <= target_abs && a >= target_abs - window);
        if !(new_seen || jump_us.is_some_and(|j| f.at_us >= j) || in_gop) {
            s.old_after_seek += 1;
            s.frames.push((Phase::Old, None));
            continue;
        }
        new_seen = true;
        s.frames.push((Phase::New, abs));
        if s.first_new.is_none() {
            s.first_new = Some((f.at_us, abs));
        }
        if let Some(a) = abs {
            if last_new.is_some_and(|l| a <= l) {
                s.out_of_order += 1;
            }
            last_new = Some(a);
            if a >= target_abs && s.reached.is_none() {
                s.reached = Some((f.at_us, a));
            }
        }
    }
    Some(s)
}

fn ms(from_us: u64, to_us: u64) -> f64 {
    (to_us as f64 - from_us as f64) / 1000.0
}

fn round(x: f64, digits: i32) -> f64 {
    let p = 10f64.powi(digits);
    (x * p).round() / p
}

/// The time the reads waited in `[from_us, to_us)`, ms.
fn waited_ms(waits: &[ReadWait], from_us: u64, to_us: u64) -> f64 {
    waits
        .iter()
        .map(|w| {
            let (a, b) = (w.from_us.max(from_us), w.to_us.min(to_us));
            if b > a {
                (b - a) as f64 / 1000.0
            } else {
                0.0
            }
        })
        .sum()
}

/// The shim's rate over a play's responses: body bytes over the time from
/// each response's first byte to its last, Mbps.
fn shim_mbps(served: &[Served]) -> Option<f64> {
    let bytes: u64 = served.iter().map(Served::body_bytes).sum();
    let us: u64 = served
        .iter()
        .map(|s| s.done_us.saturating_sub(s.first_byte_us))
        .sum();
    (us > 0).then(|| bytes as f64 * 8.0 / us as f64)
}

/// The row's metrics and details from its two plays.
pub fn score(first: &Play, seek: &Play, truth: &Truth) -> (Metrics, Value) {
    let mut m = Metrics::new();
    let ff = first.shown.first().map(|s| s.at_us);
    m.insert(
        "playback_first_frame_ms".into(),
        ff.map(|t| ms(first.start_us, t)),
    );
    m.insert(
        "playback_open_ms".into(),
        Some(ms(first.start_us, first.opened_us)),
    );
    m.insert(
        "playback_player_first_frame_ms".into(),
        ff.map(|t| ms(first.opened_us, t)),
    );
    m.insert(
        "playback_first_frame_2_ms".into(),
        seek.shown.first().map(|s| ms(seek.start_us, s.at_us)),
    );

    // Stall: the state after the first frame.
    let end_us = first
        .polls
        .iter()
        .find(|p| p.ended || p.failed)
        .map_or(first.stop_us, |p| p.at_us);
    if let Some(t) = ff {
        let b = buffering(&first.polls, t);
        m.insert("playback_buffering_ms".into(), Some(round(b.ms, 1)));
        m.insert("playback_buffering_events".into(), Some(b.events as f64));
        m.insert(
            "playback_buffering_clock_ran_ms".into(),
            Some(round(b.clock_ran_ms, 1)),
        );
        m.insert(
            "playback_buffering_clock_held_ms".into(),
            Some(round(b.clock_held_ms, 1)),
        );
        m.insert(
            "playback_clock_held_max_ms".into(),
            Some(round(b.held_max_ms, 1)),
        );
        m.insert(
            "playback_read_wait_ms".into(),
            Some(round(waited_ms(&first.waits, t, end_us), 1)),
        );
        m.insert(
            "playback_play_s".into(),
            Some(round(ms(t, end_us) / 1000.0, 3)),
        );
    }
    let gap_max = first
        .shown
        .windows(2)
        .skip(1)
        .map(|w| ms(w[0].at_us, w[1].at_us))
        .fold(None, |a: Option<f64>, g| Some(a.map_or(g, |a| a.max(g))));
    m.insert(
        "playback_frame_gap_max_ms".into(),
        gap_max.map(|g| round(g, 1)),
    );

    // Order over the whole clip.
    let order = play_order(&first.shown, truth);
    let ended = first.polls.last().is_some_and(|p| p.ended);
    let failed = first.polls.iter().any(|p| p.failed);
    m.insert("playback_units".into(), Some(truth.units() as f64));
    m.insert(
        "playback_frames_shown".into(),
        Some(first.shown.len() as f64),
    );
    m.insert(
        "playback_frames_out_of_order".into(),
        Some(order.out_of_order as f64),
    );
    m.insert("playback_frames_skipped".into(), Some(order.skipped as f64));
    m.insert(
        "playback_frames_missing_between".into(),
        Some(order.missing_between as f64),
    );
    m.insert(
        "playback_frames_unreadable".into(),
        Some(order.unreadable as f64),
    );
    m.insert(
        "playback_frames_not_in_clip".into(),
        Some(order.not_in_clip as f64),
    );
    m.insert("playback_ended".into(), Some(f64::from(u8::from(ended))));
    m.insert("playback_failed".into(), Some(f64::from(u8::from(failed))));

    // The seek.
    let sk = seek
        .seek_us
        .and_then(|t| seek_result(&seek.shown, &seek.polls, t, SEEK_TO_MS, truth));
    if let (Some(t), Some(s)) = (seek.seek_us, sk.as_ref()) {
        m.insert(
            "playback_seek_ms".into(),
            s.reached.map(|(at, _)| ms(t, at)),
        );
        m.insert(
            "playback_seek_first_frame_ms".into(),
            s.first_new.map(|(at, _)| ms(t, at)),
        );
        m.insert(
            "playback_seek_lead_frames".into(),
            s.first_new
                .and_then(|(_, a)| a)
                .map(|a| (s.target_abs - a) as f64),
        );
        let until = s.reached.map_or(seek.stop_us, |(at, _)| at);
        m.insert(
            "playback_seek_read_wait_ms".into(),
            Some(round(waited_ms(&seek.waits, t, until), 1)),
        );
        m.insert(
            "playback_seek_old_frames".into(),
            Some(s.old_after_seek as f64),
        );
        m.insert(
            "playback_seek_out_of_order".into(),
            Some(s.out_of_order as f64),
        );
    }

    m.insert(
        "playback_shim_mbps".into(),
        shim_mbps(&first.served).map(|x| round(x, 2)),
    );
    m.insert("playback_requests".into(), Some(first.served.len() as f64));
    m.insert(
        "playback_other_cpu_pct".into(),
        [first.other_cpu_pct, seek.other_cpu_pct]
            .into_iter()
            .flatten()
            .reduce(f64::max),
    );

    let first_range = first.served.first().map(|s| {
        json!({
            "range": s.range,
            "delay_ms": round(ms(s.received_us, s.first_byte_us), 2),
            "transfer_ms": round(ms(s.first_byte_us, s.done_us), 2),
            "client_ms": round(ms(first.start_us, s.done_us), 2),
        })
    });
    let seek_fetch = seek.seek_us.and_then(|t| {
        let w = seek.waits.iter().find(|w| w.from_us >= t)?;
        Some(json!({
            "read_wait_from_ms": round(ms(t, w.from_us), 2),
            "read_wait_to_ms": round(ms(t, w.to_us), 2),
            "offset": w.offset,
            "range_in_flight_at_seek": seek.served.iter().find(|s| s.received_us <= t && s.done_us > t).map(|s| json!({
                "range": s.range,
                "done_ms": round(ms(t, s.done_us), 2),
            })),
            "next_range": seek.served.iter().find(|s| s.received_us >= t).map(|s| json!({
                "range": s.range,
                "received_ms": round(ms(t, s.received_us), 2),
                "done_ms": round(ms(t, s.done_us), 2),
            })),
        }))
    });
    let info = json!({
        "units": truth.units(),
        "length_s": round(truth.length_us() as f64 / 1e6, 3),
        "truth_decode_s": round(truth.decode_s, 1),
        "first_unit_shown": order.first_unit,
        "wrong_size": [first.wrong_size, seek.wrong_size],
        "timed_out": [first.timed_out, seek.timed_out],
        "first_range": first_range,
        "requests": [first.served.len(), seek.served.len()],
        "read_waits": [first.waits.len(), seek.waits.len()],
        "seek": sk.as_ref().map(|s| json!({
            "to_ms": SEEK_TO_MS,
            "target_unit": s.target_unit,
            "target_frame": s.target_abs,
            "first_frame": s.first_new.and_then(|(_, a)| a),
            "reached_frame": s.reached.map(|(_, a)| a),
            "fetch": seek_fetch,
        })),
        "other_cpu_pct": [first.other_cpu_pct, seek.other_cpu_pct],
    });
    (m, info)
}

/// The row's files in `dir`: frames.csv (every shown frame: its present
/// time from the play's start call, its code and its unit), state.csv
/// (every state read), requests.csv (the shim's responses), read_waits.csv
/// and reads.csv (the first read calls that did not wait).
pub fn write_csvs(
    dir: &Path,
    first: &Play,
    seek: &Play,
    local: &Play,
    truth: &Truth,
) -> Result<(), String> {
    let unit_of = truth.unit_of();
    let order = play_order(&first.shown, truth);
    let sk = seek
        .seek_us
        .and_then(|t| seek_result(&seek.shown, &seek.polls, t, SEEK_TO_MS, truth));
    let mut csv = String::from("play,seq,present_ms,index,clock_ms,abs_frame,unit,unit_ms,phase\n");
    let mut row = |name: &str, play: &Play, k: usize, abs: Option<i64>, phase: &str| {
        let s = play.shown[k];
        let unit = abs.and_then(|a| unit_of.get(&a).copied());
        csv.push_str(&format!(
            "{name},{k},{:.3},{},{},{},{},{},{phase}\n",
            ms(play.start_us, s.at_us),
            s.code.map_or(String::new(), |c| c.index.to_string()),
            s.code.map_or(String::new(), |c| c.clock_ms.to_string()),
            abs.map_or(String::new(), |a| a.to_string()),
            unit.map_or(String::new(), |u| u.to_string()),
            unit.map_or(String::new(), |u| format!(
                "{:.3}",
                truth.unit_us[u] as f64 / 1000.0
            )),
        ));
    };
    for k in 0..first.shown.len() {
        row("play", first, k, order.abs.get(k).copied().flatten(), "");
    }
    for k in 0..seek.shown.len() {
        let (phase, abs) = sk
            .as_ref()
            .and_then(|s| s.frames.get(k).copied())
            .unwrap_or((Phase::Before, None));
        let name = match phase {
            Phase::Before => "before",
            Phase::Old => "old",
            Phase::New => "new",
        };
        row("seek", seek, k, abs, name);
    }
    let local_order = play_order(&local.shown, truth);
    for k in 0..local.shown.len() {
        row(
            "local",
            local,
            k,
            local_order.abs.get(k).copied().flatten(),
            "",
        );
    }
    let mut state = String::from("play,t_ms,position_ms,buffering,ended,failed\n");
    let mut requests = String::from(
        "play,conn,request,status,range_first,range_last,received_ms,first_byte_ms,done_ms,mbps\n",
    );
    let mut waits = String::from("play,from_ms,to_ms,offset\n");
    let mut reads = String::from("play,at_ms,offset,asked,got\n");
    for (name, p) in [("play", first), ("seek", seek), ("local", local)] {
        for q in &p.polls {
            state.push_str(&format!(
                "{name},{:.3},{},{},{},{}\n",
                ms(p.start_us, q.at_us),
                q.position_ms,
                u8::from(q.buffering),
                u8::from(q.ended),
                u8::from(q.failed)
            ));
        }
        for s in &p.served {
            let (a, b) = s.range.map_or((String::new(), String::new()), |(a, b)| {
                (a.to_string(), b.to_string())
            });
            let us = s.done_us.saturating_sub(s.first_byte_us);
            requests.push_str(&format!(
                "{name},{},{},{},{a},{b},{:.3},{:.3},{:.3},{}\n",
                s.conn,
                s.request,
                s.status,
                ms(p.start_us, s.received_us),
                ms(p.start_us, s.first_byte_us),
                ms(p.start_us, s.done_us),
                if us > 0 {
                    format!("{:.2}", s.body_bytes() as f64 * 8.0 / us as f64)
                } else {
                    String::new()
                }
            ));
        }
        for r in &p.reads {
            reads.push_str(&format!(
                "{name},{:.3},{},{},{}\n",
                ms(p.start_us, r.at_us),
                r.offset,
                r.asked,
                r.got
            ));
        }
        for w in &p.waits {
            waits.push_str(&format!(
                "{name},{:.3},{:.3},{}\n",
                ms(p.start_us, w.from_us),
                ms(p.start_us, w.to_us),
                w.offset
            ));
        }
    }
    for (file, text) in [
        ("frames.csv", csv),
        ("state.csv", state),
        ("requests.csv", requests),
        ("read_waits.csv", waits),
        ("reads.csv", reads),
    ] {
        let p = dir.join(file);
        std::fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    Ok(())
}

// ---- the row ------------------------------------------------------------------------

/// What the row plays.
pub struct RowSpec<'a> {
    pub mp4: &'a Path,
    /// The row's files and its cache: `<dir>/cache`.
    pub dir: &'a Path,
    pub size: (u32, u32),
    pub fps: u32,
    pub clip_frames: u32,
    /// The absolute frame expected at the clip's start.
    pub first_expect: i64,
}

/// The row's result.
pub struct Row {
    pub metrics: Metrics,
    pub info: Value,
    /// The most CPU other processes used during a play, %.
    pub other_cpu_pct: Option<f64>,
}

/// Decode the clip for the truth, serve it through the shim, play it to its
/// end, play it again with a seek, and score both plays. A third play from
/// the file to its first frame gives the player's own start.
pub fn run_row(ctx: &Arc<Ctx>, spec: &RowSpec<'_>) -> Result<Row, String> {
    let _ = std::fs::remove_dir_all(spec.dir);
    let cache = spec.dir.join("cache");
    std::fs::create_dir_all(&cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    let truth = Truth::read(
        ctx,
        spec.mp4,
        spec.size,
        spec.fps,
        spec.clip_frames,
        spec.first_expect,
    )?;
    let shim = HttpShim::start(spec.mp4, http_shim::PLAYBACK_LINK)?;
    let pull = SoundPull::start(Arc::clone(ctx))?;
    let length = Duration::from_micros(truth.length_us().max(0) as u64);
    let over_http = Source::Shim(&shim, &cache);
    let first = play(
        ctx,
        over_http,
        spec.size,
        Plan::ToTheEnd(length + PLAY_SLACK),
    );
    let seek = first.as_ref().ok().map(|_| {
        play(
            ctx,
            over_http,
            spec.size,
            Plan::SeekAfterFirstFrame(SEEK_TO_MS),
        )
    });
    // The player's own start: the same clip from the file, no network.
    let local = play(ctx, Source::Local(spec.mp4), spec.size, Plan::FirstFrame);
    let pulled = pull.finish();
    drop(shim);
    // The cache files are this row's own.
    let _ = std::fs::remove_dir_all(&cache);
    let first = first?;
    let seek = seek.ok_or("the seek play did not run")??;
    let local = local?;
    write_csvs(spec.dir, &first, &seek, &local, &truth)?;
    let (mut metrics, mut info) = score(&first, &seek, &truth);
    metrics.insert(
        "playback_local_first_frame_ms".into(),
        local.shown.first().map(|s| ms(local.start_us, s.at_us)),
    );
    metrics.insert(
        "playback_pull_late_max_ms".into(),
        Some(round(pulled.late_max_us as f64 / 1000.0, 2)),
    );
    if let Some(o) = info.as_object_mut() {
        o.insert(
            "sound_pull".into(),
            json!({ "pulls": pulled.pulls, "audible": pulled.audible, "late_max_ms": round(pulled.late_max_us as f64 / 1000.0, 2) }),
        );
        o.insert(
            "link".into(),
            json!({
                "rate_bps": http_shim::PLAYBACK_LINK.rate_bps,
                "delay_ms": http_shim::PLAYBACK_LINK.delay.as_millis() as u64,
                "burst_bytes": http_shim::PLAYBACK_LINK.burst_bytes,
                "write_bytes": http_shim::PLAYBACK_LINK.write_bytes,
            }),
        );
    }
    Ok(Row {
        metrics,
        info,
        // The load guard covers the plays over HTTP: the local play is a
        // few tens of ms, too short a window to measure the load.
        other_cpu_pct: [first.other_cpu_pct, seek.other_cpu_pct]
            .into_iter()
            .flatten()
            .reduce(f64::max),
    })
}

// ---- gates ------------------------------------------------------------------------

fn gate(metric: &str, max: f64, note: &str) -> Gate {
    Gate {
        metric: metric.to_owned(),
        min: None,
        max: Some(max),
        enforced: true,
        note: note.to_owned(),
    }
}

/// The gates of the row (CLIPS.md 15.13), absolute: `--write-baseline` writes
/// them into gates.json; a test in replay.rs keeps the committed file equal.
pub fn gates() -> Vec<Gate> {
    vec![
        gate(
            "playback_first_frame_ms",
            1000.0,
            "CLIPS.md 15.9 and 15.13: the first frame under 1 s from the start call on a 20 Mbps link with 50 ms delay",
        ),
        gate(
            "playback_buffering_ms",
            0.0,
            "CLIPS.md 15.13: no stall at 20 Mbps: the player's state says buffering at no time after the first frame",
        ),
        gate(
            "playback_frames_out_of_order",
            0.0,
            "CLIPS.md 15.13: every frame code in order: no shown frame goes back or repeats",
        ),
        gate(
            "playback_frames_skipped",
            0.0,
            "CLIPS.md 15.13: every unit of the clip shows when it plays to its end in real time",
        ),
        gate(
            "playback_frames_unreadable",
            0.0,
            "every shown frame's code reads",
        ),
        gate(
            "playback_seek_ms",
            1000.0,
            "CLIPS.md 15.13: a seek to 20 s shows the frame of 20 s within 1 s",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clip of `n` units from absolute frame `first` at 60 fps.
    fn truth(first: i64, n: usize) -> Truth {
        Truth {
            unit_us: (0..n as i64).map(|k| k * 1_000_000 / 60).collect(),
            abs: (0..n as i64).map(|k| Some(first + k)).collect(),
            fps: 60,
            clip_frames: 1200,
            decode_s: 0.0,
        }
    }

    fn code(abs: i64) -> Option<FrameCode> {
        let index = abs.rem_euclid(1200) as u32;
        Some(FrameCode {
            index,
            clock_ms: index * 1000 / 60,
        })
    }

    /// Unit k shown at 5 s + its clip time + `lag_us`.
    fn shown(t: &Truth, units: impl Iterator<Item = usize>, lag_us: u64) -> Vec<Shown> {
        units
            .map(|k| Shown {
                at_us: 5_000_000 + t.unit_us[k] as u64 + lag_us,
                code: code(t.abs[k].expect("abs")),
            })
            .collect()
    }

    #[test]
    fn a_clean_play_shows_every_unit_in_order() {
        let t = truth(300, 1800);
        let o = play_order(&shown(&t, 0..1800, 0), &t);
        assert_eq!(
            (o.out_of_order, o.skipped, o.unreadable, o.not_in_clip),
            (0, 0, 0, 0)
        );
        assert_eq!(o.first_unit, Some(0));
        assert_eq!(o.abs.last(), Some(&Some(2099)));
    }

    #[test]
    fn a_dropped_frame_is_skipped_and_a_late_one_is_out_of_order() {
        let t = truth(300, 1800);
        let mut s = shown(&t, (0..1800).filter(|&k| k != 700 && k != 701), 0);
        let o = play_order(&s, &t);
        assert_eq!((o.skipped, o.missing_between, o.out_of_order), (2, 2, 0));
        // A frame of 0.5 s shown again at 10 s.
        s[600].code = code(330);
        let o = play_order(&s, &t);
        assert_eq!(o.out_of_order, 1);
        // The tail that never played is skipped too.
        let o = play_order(&shown(&t, 0..1500, 0), &t);
        assert_eq!(o.skipped, 300);
    }

    #[test]
    fn a_play_that_starts_late_still_unwraps_its_frames() {
        // The first frame at once, the rest 400 ms later (the start rule).
        let t = truth(300, 1800);
        let mut s = shown(&t, 0..1800, 400_000);
        s[0].at_us -= 400_000;
        let o = play_order(&s, &t);
        assert_eq!((o.out_of_order, o.skipped), (0, 0));
    }

    fn poll(at_ms: u64, position_ms: u64, buffering: bool) -> Poll {
        Poll {
            at_us: at_ms * 1000,
            position_ms,
            buffering,
            ended: false,
            failed: false,
        }
    }

    #[test]
    fn buffering_counts_after_the_first_frame_and_splits_by_the_clock() {
        let mut polls = vec![poll(0, 0, true), poll(5, 0, true)];
        // Playing from 10 ms; a read waits from 100 to 120 ms while the
        // sound plays on; the clock holds from 200 to 260 ms.
        for t in (10..=400).step_by(5) {
            let pos = if t < 200 {
                t - 10
            } else if t < 260 {
                190
            } else {
                190 + (t - 260)
            };
            polls.push(poll(
                t,
                pos,
                (100..120).contains(&t) || (200..260).contains(&t),
            ));
        }
        let mut end = poll(405, 335, true);
        end.ended = true;
        polls.push(end);
        let b = buffering(&polls, 10_000);
        assert_eq!(b.events, 2);
        assert!((b.ms - 80.0).abs() < 1e-9, "{b:?}");
        assert!((b.clock_ran_ms - 20.0).abs() < 1e-9, "{b:?}");
        assert!((b.clock_held_ms - 60.0).abs() < 1e-9, "{b:?}");
        assert!((b.held_max_ms - 60.0).abs() < 1e-9, "{b:?}");
        // From the start, the start buffering counts too.
        assert!((buffering(&polls, 0).ms - 90.0).abs() < 1e-9);
    }

    #[test]
    fn the_seek_finds_the_frame_of_20_s_though_its_code_is_the_first_frames() {
        let t = truth(300, 1800);
        // Unit 1200 (20 s) is frame 1500: loop index 300, as unit 0.
        assert_eq!(t.unit_at(20_000_000), Some(1200));
        let seek_us = 5_000_000;
        let mut s = vec![Shown {
            at_us: 4_990_000,
            code: code(300),
        }];
        // Two old frames after the seek call, before the position jumps.
        for (k, at) in [(1, 5_010_000), (2, 5_020_000)] {
            s.push(Shown {
                at_us: at,
                code: code(300 + k),
            });
        }
        // The seek's key frame at 19 s (frame 1440), then the GOP to 20 s.
        s.push(Shown {
            at_us: 5_600_000,
            code: code(1440),
        });
        for (i, a) in (1490..=1520).enumerate() {
            s.push(Shown {
                at_us: 5_900_000 + i as u64 * 16_667,
                code: code(a),
            });
        }
        // The state shows the jump at 5.3 s.
        let polls = vec![poll(5_000, 30, true), poll(5_300, 20_000, true)];
        let r = seek_result(&s, &polls, seek_us, 20_000, &t).expect("a target");
        assert_eq!(r.target_abs, 1500);
        assert_eq!(r.old_after_seek, 2);
        assert_eq!(r.first_new, Some((5_600_000, Some(1440))));
        assert_eq!(r.reached, Some((5_900_000 + 10 * 16_667, 1500)));
        assert_eq!(r.out_of_order, 0);

        // Without a state read of the jump, the GOP frame marks the seek; an
        // old frame of 0.5 s is not the frame of 20 s.
        let r = seek_result(&s, &[], seek_us, 20_000, &t).expect("a target");
        assert_eq!(r.old_after_seek, 2);
        assert_eq!(r.reached.map(|x| x.1), Some(1500));
    }
}
