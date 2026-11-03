//! The replay lane (plans/GAME-CLIPS.md 10.3): the game clip buffer records
//! the reference clip, presses capture 30 s clips, and the gate reads each
//! clip back. No SFU and no shim: a clip is a file (the streamed playback
//! row, `playback.rs`, has its own HTTP shim).
//!
//! The path. The reference source plays the clip and its game audio track
//! (`mello_test_set_reference_clip`, `mello_test_set_reference_audio`) into a
//! replay session in place of a game window
//! (`mello_test_replay_start_reference`): the capture manager, the recording
//! encoder, the video and audio rings. `mello_replay_capture` muxes the last
//! 30 s into an MP4. The read-back demuxes the MP4
//! (`mello_test_mp4_read_video`, `mello_test_mp4_read_audio`) and decodes the
//! video through a stream viewer with the CPU frame callback. The viewer gets
//! the next unit only after the frame of the last one, so it shows every
//! frame.
//!
//! Frame numbers are absolute: counted from the source's first frame, over
//! the loops of the 1200-frame clip. The source's clock
//! (`mello_test_replay_reference_info`) turns a press time into the frame
//! that was captured then.
//!
//! The gates of a clip (`benchmarks/baselines/replay/gates.json`).
//!
//! | Metric | Definition | Gate |
//! |---|---|---|
//! | `start_lead_frames` | the frame captured 30 s before the press minus the clip's first frame | 0 to one GOP (60 frames): the clip starts at the key frame before its start |
//! | `frames_missing`, `frames_out_of_order` | frame indices that the clip skips, or that go back | 0 |
//! | `frames_unreadable`, `frames_undecoded` | frame codes that do not read, units without a picture | 0 |
//! | `psnr_y_mean`, `ssim_y_mean`, `vmaf_mean` | every frame against the clip frame of its index (VMAF on every n-th frame, per lane) | floors, measured |
//! | `av_offset_abs_max_ms` | the largest offset of a game audio marker onset to the clip time of its video frame (marker k: frame 15k) | 20 ms |
//! | `av_markers_missing` | markers whose video frame is in the clip and that the sound does not have | 0 |
//! | `duration_s` | `MelloReplayClip.duration_us` | 30 to 31 s |
//!
//! The loopback row (`--loopback`, hw lane, Windows PC). A small WASAPI
//! player (`libmello/tests/tone_player.cpp`) plays 1 kHz bursts and logs the
//! onset of each on the steady clock. The replay records its sound through
//! process loopback. Each burst in the clip's sound is compared with the clip
//! time of the frame that the source captured at its onset:
//! `loopback_offset_abs_max_ms` 20 ms.

use crate::audio::{self, AudioSpec, SAMPLE_RATE};
use crate::clip::ClipSpec;
use crate::code::{self, FrameCode};
use crate::load;
use crate::metrics::SourceClock;
use crate::profile::Gate;
use crate::session::{clock_us, Ctx};
use mello_core::events::GameClipQuality;
use std::collections::BTreeMap;
use std::ffi::{c_void, CStr, CString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Each press asks for this much.
pub const CLIP_S: f32 = 30.0;

/// The presses: name, seconds after the source's first frame. Each window
/// holds a scene cut of the clip (10 s into each 20 s loop); 47 s is the
/// plan's case for it. The windows of 60 s and 62 s overlap: the ring pins
/// the segments of both.
pub const PRESSES: &[(&str, f64)] = &[
    ("press-35s", 35.0),
    ("press-47s", 47.0),
    ("press-60s", 60.0),
    ("press-62s", 62.0),
];

/// The loopback row's one press: the ring holds more than 30 s by then.
pub const LOOPBACK: &str = "loopback";
pub const LOOPBACK_PRESS_S: f64 = 32.0;

/// A lane of the replay gate.
#[derive(Clone, Copy, Debug)]
pub struct ReplayLane {
    pub name: &'static str,
    /// The clip of `benchmarks/baselines/stream/profiles.json`.
    pub clip: &'static str,
    /// The clip quality. Its height, rate and bitrate come from
    /// `GameClipQuality::encoding` (mello-core `events.rs`).
    pub quality: GameClipQuality,
    /// OpenH264 encode and decode (`MELLO_SOFT_ENCODER=1`,
    /// `MELLO_SOFT_DECODER=1`).
    pub soft: bool,
    /// VMAF on every n-th frame. The pair files of a 1080p clip at every
    /// frame are 11 GB.
    pub vmaf_subsample: usize,
}

pub const LANES: [ReplayLane; 2] = [
    ReplayLane {
        name: "cpu",
        clip: "720p60",
        quality: GameClipQuality::P720Fps60,
        soft: true,
        vmaf_subsample: 1,
    },
    ReplayLane {
        name: "hw",
        clip: "1080p60",
        quality: GameClipQuality::P1080Fps60,
        soft: false,
        vmaf_subsample: 2,
    },
];

pub fn lane(name: &str) -> Option<ReplayLane> {
    LANES.iter().find(|l| l.name == name).copied()
}

fn c_path(p: &Path) -> Result<CString, String> {
    CString::new(p.to_string_lossy().as_bytes()).map_err(|e| format!("{}: {e}", p.display()))
}

/// The context's last error.
pub fn last_error(ctx: &Ctx) -> String {
    // SAFETY: ctx is live; the text is copied at once.
    let p = unsafe { mello_sys::mello_get_error(ctx.0) };
    if p.is_null() {
        return String::new();
    }
    // SAFETY: a NUL-terminated string that libmello owns.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

// ---- recording ----------------------------------------------------------------

/// The clip of one press.
#[derive(Clone, Debug)]
pub struct Capture {
    pub name: String,
    /// Seconds after the source's first frame.
    pub at_s: f64,
    /// The press: the steady clock just before `mello_replay_capture`.
    pub press_us: u64,
    /// When `mello_replay_capture` returned.
    pub done_us: u64,
    /// `mello_replay_state` just before the press.
    pub state: i32,
    pub mp4: PathBuf,
    pub poster: PathBuf,
    /// None: the capture failed (`error` says why).
    pub clip: Option<mello_sys::MelloReplayClip>,
    pub error: Option<String>,
}

/// What one recording did.
pub struct Recording {
    pub clock: SourceClock,
    /// The source at the end: frames handed over, late frames.
    pub source: mello_sys::MelloReferenceInfo,
    pub captures: Vec<Capture>,
    /// CPU that other processes used while the replay recorded, %.
    pub other_cpu_pct: Option<f64>,
    pub seconds: f64,
}

/// What to record.
pub struct RecordSpec<'a> {
    pub clip_path: &'a Path,
    pub clip: &'a ClipSpec,
    /// The game audio track (`mello_test_set_reference_audio`).
    pub track: Option<&'a Path>,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// A process whose sound the replay records through process loopback
    /// in place of the track; 0: the track.
    pub pid: u32,
    /// The ring files.
    pub buffer_dir: &'a Path,
    /// Each clip goes to `<out_dir>/<name>/clip.mp4`.
    pub out_dir: &'a Path,
    pub presses: Vec<(String, f64)>,
    /// Processes of the run, other than this one, that the load guard
    /// counts as the run's own CPU.
    pub own_processes: Vec<*mut c_void>,
}

fn reference_info(ctx: &Ctx) -> Option<mello_sys::MelloReferenceInfo> {
    // SAFETY: plain data, filled by the call.
    let mut info: mello_sys::MelloReferenceInfo = unsafe { std::mem::zeroed() };
    // SAFETY: ctx is live.
    unsafe { mello_sys::mello_test_replay_reference_info(ctx.0, &mut info) }.then_some(info)
}

/// Run the replay on the reference source, press at each time, stop it.
pub fn record(ctx: &Arc<Ctx>, spec: &RecordSpec<'_>) -> Result<Recording, String> {
    let clip_c = c_path(spec.clip_path)?;
    // SAFETY: ctx is live; the path is copied.
    let r = unsafe {
        mello_sys::mello_test_set_reference_clip(
            ctx.0,
            clip_c.as_ptr(),
            spec.clip.width,
            spec.clip.height,
            spec.clip.fps,
            mello_sys::MelloReferencePixelFormat_MELLO_REFERENCE_I420,
        )
    };
    if r != mello_sys::MelloResult_MELLO_OK {
        return Err(format!("mello_test_set_reference_clip: {r}"));
    }
    let track_c = spec.track.map(c_path).transpose()?;
    // SAFETY: ctx is live; the path is copied.
    unsafe {
        mello_sys::mello_test_set_reference_audio(
            ctx.0,
            track_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
        )
    };
    std::fs::create_dir_all(spec.buffer_dir)
        .map_err(|e| format!("{}: {e}", spec.buffer_dir.display()))?;
    let dir_c = c_path(spec.buffer_dir)?;
    let config = mello_sys::MelloReplayConfig {
        hwnd: std::ptr::null_mut(),
        pid: spec.pid,
        height: spec.height,
        fps: spec.fps,
        bitrate_kbps: spec.bitrate_kbps,
        buffer_dir: dir_c.as_ptr(),
    };
    let load_start = load::sample(&spec.own_processes);
    let started = Instant::now();
    // SAFETY: ctx is live; config and its path outlive the call.
    let r = unsafe { mello_sys::mello_test_replay_start_reference(ctx.0, &config) };
    if r != mello_sys::MelloResult_MELLO_OK {
        return Err(format!(
            "mello_test_replay_start_reference: {} ({r})",
            last_error(ctx)
        ));
    }
    // The replay runs from here: stop it on every way out.
    let result = record_running(ctx, spec);
    // SAFETY: ctx is live.
    unsafe { mello_sys::mello_replay_stop(ctx.0) };
    let mut rec = result?;
    rec.other_cpu_pct = load::other_pct(load_start, load::sample(&spec.own_processes));
    rec.seconds = started.elapsed().as_secs_f64();
    Ok(rec)
}

fn record_running(ctx: &Arc<Ctx>, spec: &RecordSpec<'_>) -> Result<Recording, String> {
    // The source's clock, once it hands over frames.
    let deadline = Instant::now() + Duration::from_secs(10);
    let info = loop {
        if let Some(i) = reference_info(ctx).filter(|i| i.start_us > 0 && i.frames_delivered > 0) {
            break i;
        }
        if Instant::now() > deadline {
            return Err(format!(
                "the replay's reference source gave no frame in 10 s ({})",
                last_error(ctx)
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if info.clip_frames != spec.clip.frames || info.fps != spec.clip.fps {
        return Err(format!(
            "the source plays {} frames at {} fps, the clip has {} at {}",
            info.clip_frames, info.fps, spec.clip.frames, spec.clip.fps
        ));
    }
    let clock = SourceClock {
        start_us: info.start_us,
        clip_frames: info.clip_frames,
        fps: info.fps,
    };
    // One thread per press: a press keeps its time while the mux of an
    // earlier one runs.
    let mut threads = Vec::new();
    for (name, at_s) in &spec.presses {
        let ctx = Arc::clone(ctx);
        let dir = spec.out_dir.join(name);
        let (name, at_s) = (name.clone(), *at_s);
        threads.push(
            std::thread::Builder::new()
                .name(format!("replay-{name}"))
                .spawn(move || press(&ctx, clock.start_us, name, at_s, &dir))
                .map_err(|e| format!("press thread: {e}"))?,
        );
    }
    let mut captures = Vec::new();
    for t in threads {
        captures.push(
            t.join()
                .map_err(|_| "a press thread panicked".to_string())?,
        );
    }
    Ok(Recording {
        clock,
        source: reference_info(ctx).unwrap_or(info),
        captures,
        other_cpu_pct: None,
        seconds: 0.0,
    })
}

fn press(ctx: &Ctx, start_us: u64, name: String, at_s: f64, dir: &Path) -> Capture {
    let mut cap = Capture {
        name,
        at_s,
        press_us: 0,
        done_us: 0,
        state: 0,
        mp4: dir.join("clip.mp4"),
        poster: dir.join("poster.jpg"),
        clip: None,
        error: None,
    };
    let paths = std::fs::create_dir_all(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))
        .and_then(|_| Ok((c_path(&cap.mp4)?, c_path(&cap.poster)?)));
    let (mp4_c, poster_c) = match paths {
        Ok(p) => p,
        Err(e) => {
            cap.error = Some(e);
            return cap;
        }
    };
    // Sleep to 20 ms before the press time (the default timer resolution
    // is 15.6 ms), then spin. The press lands on its time to the
    // microsecond, so every run takes the same window of the clip.
    let due = start_us + (at_s * 1e6) as u64;
    while clock_us() + 20_000 < due {
        std::thread::sleep(Duration::from_millis(5));
    }
    while clock_us() < due {
        std::hint::spin_loop();
    }
    // SAFETY: ctx is live.
    cap.state = unsafe { mello_sys::mello_replay_state(ctx.0) };
    // SAFETY: plain data, filled by the call.
    let mut clip: mello_sys::MelloReplayClip = unsafe { std::mem::zeroed() };
    cap.press_us = clock_us();
    // SAFETY: ctx is live; the paths outlive the call.
    let r = unsafe {
        mello_sys::mello_replay_capture(ctx.0, CLIP_S, mp4_c.as_ptr(), poster_c.as_ptr(), &mut clip)
    };
    cap.done_us = clock_us();
    if r == mello_sys::MelloResult_MELLO_OK {
        cap.clip = Some(clip);
    } else {
        cap.error = Some(format!("mello_replay_capture: {} ({r})", last_error(ctx)));
    }
    cap
}

// ---- read-back ------------------------------------------------------------------

/// One video unit of a clip, in Annex B.
pub struct Unit {
    pub annex_b: Vec<u8>,
    /// Its time in the clip, µs.
    pub time_us: i64,
    pub keyframe: bool,
}

/// Every video unit of a clip MP4 in decode order, and the picture size.
pub fn read_units(mp4: &Path) -> Result<(Vec<Unit>, u32, u32), String> {
    unsafe extern "C" fn on_unit(
        user: *mut c_void,
        data: *const u8,
        size: usize,
        time_us: i64,
        keyframe: bool,
    ) -> bool {
        // SAFETY: user is the Vec below, live for the read.
        let units = unsafe { &mut *(user as *mut Vec<Unit>) };
        let annex_b = if data.is_null() || size == 0 {
            Vec::new()
        } else {
            // SAFETY: libmello hands `size` bytes for the duration of the call.
            unsafe { std::slice::from_raw_parts(data, size) }.to_vec()
        };
        units.push(Unit {
            annex_b,
            time_us,
            keyframe,
        });
        true
    }
    let path = c_path(mp4)?;
    let mut units: Vec<Unit> = Vec::new();
    let (mut w, mut h) = (0u32, 0u32);
    // SAFETY: the callback writes only into `units`, which outlives the call.
    let r = unsafe {
        mello_sys::mello_test_mp4_read_video(
            path.as_ptr(),
            Some(on_unit),
            &mut units as *mut Vec<Unit> as *mut c_void,
            &mut w,
            &mut h,
        )
    };
    if r != mello_sys::MelloResult_MELLO_OK {
        return Err(format!("mello_test_mp4_read_video {}: {r}", mp4.display()));
    }
    Ok((units, w, h))
}

/// One decoded frame: the time of its unit in the clip and its code.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub time_us: i64,
    pub code: Option<FrameCode>,
}

/// The decoded video of a clip.
pub struct Decoded {
    /// One per unit that gave a picture, in decode order. The recording
    /// encoder makes no B-frames, so it is also the presentation order and
    /// frame k is the picture of unit k.
    pub frames: Vec<Frame>,
    /// Frames of another size than the clip: their code is not read.
    pub wrong_size: u64,
    /// Frames the decoder held back (a wait for a frame that did not come).
    pub held: u64,
    pub seconds: f64,
}

/// (RGBA pixels, width, height) of one shown frame.
type FrameMsg = (Vec<u8>, u32, u32);

/// Where the viewer's present thread puts each frame. The callback only
/// copies the pixels; the read-back thread reads them.
struct FrameTap {
    tx: Mutex<Option<mpsc::Sender<FrameMsg>>>,
    pool: Mutex<Vec<Vec<u8>>>,
}

unsafe extern "C" fn on_frame(user: *mut c_void, rgba: *const u8, w: u32, h: u32, _ts: u64) {
    // SAFETY: user is the FrameTap that outlives the viewer.
    let tap = unsafe { &*(user as *const FrameTap) };
    if rgba.is_null() || w == 0 || h == 0 {
        return;
    }
    // SAFETY: libmello hands w * h RGBA pixels for the duration of the call.
    let px = unsafe { std::slice::from_raw_parts(rgba, (w as usize) * (h as usize) * 4) };
    let mut buf = tap
        .pool
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pop()
        .unwrap_or_default();
    buf.clear();
    buf.extend_from_slice(px);
    if let Some(tx) = tap.tx.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        let _ = tx.send((buf, w, h));
    }
}

/// A wait for the first frame (the decoder's first key frame), and for each
/// frame after it. A wait that runs out means the decoder holds a frame.
const FIRST_FRAME_WAIT: Duration = Duration::from_secs(2);
const FRAME_WAIT: Duration = Duration::from_secs(1);
/// More frames held back than this: the decoder gives no pictures.
const MAX_HELD: u64 = 4;

/// What the read-back keeps of each frame.
struct FrameReader {
    width: u32,
    height: u32,
    luma: Vec<u8>,
    writer: Option<std::io::BufWriter<std::fs::File>>,
    frames: Vec<Frame>,
    wrong_size: u64,
}

impl FrameReader {
    fn take(&mut self, rgba: &[u8], w: u32, h: u32, time_us: i64) -> Result<(), String> {
        let fits =
            w == self.width && h == self.height && rgba.len() >= (w as usize) * (h as usize) * 4;
        let code = if fits {
            code::read_rgba(rgba, w as usize)
        } else {
            self.wrong_size += 1;
            None
        };
        if let Some(writer) = self.writer.as_mut() {
            // One luma plane per frame, so frame k is slot k of the file.
            if fits {
                for (i, y) in self.luma.iter_mut().enumerate() {
                    *y = code::luma_709(rgba[i * 4], rgba[i * 4 + 1], rgba[i * 4 + 2]);
                }
            } else {
                self.luma.fill(16);
            }
            writer.write_all(&self.luma).map_err(|e| e.to_string())?;
        }
        self.frames.push(Frame { time_us, code });
        Ok(())
    }
}

/// Decode the units through a stream viewer with the CPU frame callback.
///
/// Lockstep: the viewer's present thread shows the newest frame that is due
/// and drops older ones, so the read-back feeds a unit only when every unit
/// before it gave its frame. A decoder that holds frames back (its delay)
/// shows as a wait that runs out: one more unit then waits at a time. At the
/// end, the first unit (a key frame with its parameter sets) goes in again,
/// once per held frame, to push the last frames out; their own pictures are
/// not read. `luma`: write the luma of each frame there, frame k at slot k.
pub fn decode(
    ctx: &Ctx,
    units: &[Unit],
    width: u32,
    height: u32,
    fps: u32,
    luma: Option<&Path>,
) -> Result<Decoded, String> {
    let started = Instant::now();
    let writer = match luma {
        Some(p) => Some(std::io::BufWriter::with_capacity(
            16 << 20,
            std::fs::File::create(p).map_err(|e| format!("{}: {e}", p.display()))?,
        )),
        None => None,
    };
    let mut reader = FrameReader {
        width,
        height,
        luma: vec![0u8; (width as usize) * (height as usize)],
        writer,
        frames: Vec::with_capacity(units.len()),
        wrong_size: 0,
    };
    let (tx, rx) = mpsc::channel::<FrameMsg>();
    let tap = Box::new(FrameTap {
        tx: Mutex::new(Some(tx)),
        pool: Mutex::new(Vec::new()),
    });
    let config = mello_sys::MelloStreamConfig {
        width,
        height,
        fps,
        bitrate_kbps: 0,
    };
    let user = &*tap as *const FrameTap as *mut c_void;
    // SAFETY: ctx is live; tap outlives the viewer (stopped below first).
    let view =
        unsafe { mello_sys::mello_stream_start_viewer(ctx.0, &config, Some(on_frame), user) };
    if view.is_null() {
        return Err("mello_stream_start_viewer failed".into());
    }
    let feed = |u: &Unit| {
        // SAFETY: view is live until the stop below; the bytes are copied.
        unsafe {
            mello_sys::mello_stream_feed_packet(
                view,
                u.annex_b.as_ptr(),
                u.annex_b.len() as i32,
                u.keyframe,
            )
        }
    };

    let n = units.len();
    let (mut fed, mut flushes, mut got, mut held) = (0usize, 0usize, 0usize, 0u64);
    let mut feed_failures = 0u64;
    let mut pending: Option<FrameMsg> = None;
    let mut result = Ok(());
    while got < n {
        // At most `held + 1` units wait for their frame.
        while fed + flushes <= got + held as usize {
            if fed < n {
                feed_failures += u64::from(!feed(&units[fed]));
                fed += 1;
            } else if (flushes as u64) < held + 1 {
                feed_failures += u64::from(!feed(&units[0]));
                flushes += 1;
            } else {
                break;
            }
        }
        // Read the last frame while the decoder works on the next unit.
        if let Some((buf, w, h)) = pending.take() {
            result = reader.take(&buf, w, h, units[reader.frames.len()].time_us);
            tap.pool.lock().unwrap_or_else(|p| p.into_inner()).push(buf);
            if result.is_err() {
                break;
            }
        }
        let wait = if got == 0 {
            FIRST_FRAME_WAIT
        } else {
            FRAME_WAIT
        };
        match rx.recv_timeout(wait) {
            Ok(msg) => {
                got += 1;
                pending = Some(msg);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if held >= MAX_HELD {
                    break;
                }
                held += 1;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if let Some((buf, w, h)) = pending.take() {
        if result.is_ok() {
            result = reader.take(&buf, w, h, units[reader.frames.len()].time_us);
        }
    }
    // SAFETY: view from start_viewer above; no callback runs after it returns.
    unsafe { mello_sys::mello_stream_stop_viewer(view) };
    drop(tap);
    result?;
    if let Some(w) = reader.writer.as_mut() {
        w.flush().map_err(|e| e.to_string())?;
    }
    if feed_failures > 0 {
        log::warn!("replay read-back: the viewer refused {feed_failures} unit(s)");
    }
    Ok(Decoded {
        frames: reader.frames,
        wrong_size: reader.wrong_size,
        held,
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// The clip's sound: the left channel on the clip's time line.
#[derive(Clone, Debug, Default)]
pub struct ClipAudio {
    /// Clip time of sample 0, µs.
    pub start_us: i64,
    pub left: Vec<i16>,
}

impl ClipAudio {
    /// Clip time of a (fractional) sample, µs.
    pub fn time_us(&self, sample: f64) -> f64 {
        self.start_us as f64 + sample * 1e6 / SAMPLE_RATE as f64
    }

    pub fn end_us(&self) -> f64 {
        self.time_us(self.left.len() as f64)
    }
}

/// Decode the sound of a clip MP4. Each buffer goes to the place of its
/// time; a buffer within 2 samples of the end of the last one follows it.
pub fn read_audio(mp4: &Path) -> Result<ClipAudio, String> {
    type Buffers = Vec<(i64, Vec<i16>)>;
    unsafe extern "C" fn on_pcm(
        user: *mut c_void,
        stereo: *const i16,
        frames: usize,
        time_us: i64,
    ) -> bool {
        // SAFETY: user is the Vec below, live for the read.
        let out = unsafe { &mut *(user as *mut Buffers) };
        if stereo.is_null() || frames == 0 {
            return true;
        }
        // SAFETY: libmello hands `frames` stereo frames for the call.
        let s = unsafe { std::slice::from_raw_parts(stereo, frames * 2) };
        out.push((time_us, s.iter().step_by(2).copied().collect()));
        true
    }
    let path = c_path(mp4)?;
    let mut buffers: Buffers = Vec::new();
    // SAFETY: the callback writes only into `buffers`, which outlives the call.
    let r = unsafe {
        mello_sys::mello_test_mp4_read_audio(
            path.as_ptr(),
            Some(on_pcm),
            &mut buffers as *mut Buffers as *mut c_void,
        )
    };
    if r != mello_sys::MelloResult_MELLO_OK {
        return Err(format!("mello_test_mp4_read_audio {}: {r}", mp4.display()));
    }
    Ok(place_audio(&buffers))
}

/// The buffers of a read on one time line from the first buffer's time.
pub fn place_audio(buffers: &[(i64, Vec<i16>)]) -> ClipAudio {
    let Some(start_us) = buffers.first().map(|b| b.0) else {
        return ClipAudio::default();
    };
    let mut left: Vec<i16> = Vec::new();
    for (t, s) in buffers {
        let at = ((t - start_us) as f64 * SAMPLE_RATE as f64 / 1e6).round();
        let at = if at < 0.0 { 0 } else { at as usize };
        let at = if at.abs_diff(left.len()) <= 2 {
            left.len()
        } else {
            at
        };
        if left.len() < at + s.len() {
            left.resize(at + s.len(), 0);
        }
        left[at..at + s.len()].copy_from_slice(s);
    }
    ClipAudio { start_us, left }
}

// ---- analysis -------------------------------------------------------------------

/// Due time of absolute frame `n` on the source's clock: the source's own
/// formula (`reference_due_us`, start + n * 1e6 / fps in integers).
pub fn due_us(clock: &SourceClock, n: u64) -> u64 {
    clock.start_us + n * 1_000_000 / u64::from(clock.fps.max(1))
}

/// The absolute frame that the source captured at `t_us`: the last one due
/// at or before it.
pub fn frame_at(clock: &SourceClock, t_us: u64) -> Option<u64> {
    if t_us < clock.start_us {
        return None;
    }
    let mut n = (t_us - clock.start_us) * u64::from(clock.fps.max(1)) / 1_000_000;
    while due_us(clock, n + 1) <= t_us {
        n += 1;
    }
    while n > 0 && due_us(clock, n) > t_us {
        n -= 1;
    }
    Some(n)
}

/// The absolute frame of a loop index: the pass over the clip that puts it
/// nearest to `expect`.
pub fn unwrap_nearest(index: u32, expect: i64, clip_frames: u32) -> i64 {
    let cf = i64::from(clip_frames.max(1));
    let loops = ((expect - i64::from(index)) as f64 / cf as f64).round() as i64;
    i64::from(index) + loops * cf
}

/// The order of a clip's frames.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Order {
    /// The absolute frame of each frame; None: its code does not read.
    pub abs: Vec<Option<i64>>,
    pub unreadable: u64,
    /// Frames that the clip skips between two of its frames.
    pub missing: u64,
    /// Frames that show the same frame as one before them, or an earlier one.
    pub out_of_order: u64,
}

/// The absolute frame of every frame and the gaps between them. Frame k's
/// expected frame is `first_expect` plus its clip time in frames; its loop
/// index is unwrapped to the pass nearest to that.
pub fn frame_order(frames: &[Frame], first_expect: i64, clip_frames: u32, fps: u32) -> Order {
    let mut o = Order::default();
    let mut last: Option<(usize, i64)> = None;
    for (k, f) in frames.iter().enumerate() {
        let Some(code) = f.code else {
            o.unreadable += 1;
            o.abs.push(None);
            continue;
        };
        let offset = (f.time_us as f64 * f64::from(fps) / 1e6).round() as i64;
        let a = unwrap_nearest(code.index, first_expect + offset, clip_frames);
        if let Some((i, p)) = last {
            // Positions between two readable frames are the unreadable ones.
            let step = a - p;
            let positions = (k - i) as i64;
            if step <= 0 || step < positions {
                o.out_of_order += 1;
            } else if step > positions {
                o.missing += (step - positions) as u64;
            }
        }
        last = Some((k, a));
        o.abs.push(Some(a));
    }
    o
}

/// One game audio marker of a clip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Marker {
    /// Absolute marker number: its video frame is 15k.
    pub k: u64,
    /// Clip time of its video frame, µs.
    pub video_us: f64,
    /// Clip time of its onset in the sound, µs; None: not found.
    pub audio_us: Option<f64>,
}

impl Marker {
    pub fn offset_ms(&self) -> Option<f64> {
        self.audio_us.map(|a| (a - self.video_us) / 1000.0)
    }
}

/// A marker this close to either end of the sound is not expected: the
/// detector needs the whole burst.
const MARKER_EDGE_US: f64 = 50_000.0;

/// The markers whose video frame is in the clip, each with its onset in the
/// sound. `frames`: absolute frame to clip time (µs) of the readable frames.
/// `bias`: the detector's own delay in samples (`audio::detector_bias`).
pub fn score_markers(
    spec: &AudioSpec,
    bias: f64,
    sound: &ClipAudio,
    frames: &BTreeMap<i64, i64>,
    clip_frames: u32,
    fps: u32,
) -> Vec<Marker> {
    let (Some((&lo, _)), Some((&hi, _))) = (frames.first_key_value(), frames.last_key_value())
    else {
        return Vec::new();
    };
    let per_marker = spec.marker_period_ms * f64::from(fps) / 1000.0;
    // The track has its markers from the start of each loop.
    let per_loop = (f64::from(clip_frames) / per_marker).round().max(1.0) as u64;
    let nf = spec.marker_freqs_hz.len() as u64;
    let (a_start, a_end) = (sound.start_us as f64, sound.end_us());
    let mut markers: Vec<Marker> = Vec::new();
    let first = (lo.max(0) as f64 / per_marker).ceil() as u64;
    let last = (hi.max(0) as f64 / per_marker).floor() as u64;
    for k in first..=last {
        let m = (k as f64 * per_marker).round() as i64;
        let Some(&t) = frames.get(&m) else { continue };
        let t = t as f64;
        if t - MARKER_EDGE_US >= a_start && t + MARKER_EDGE_US <= a_end {
            markers.push(Marker {
                k,
                video_us: t,
                audio_us: None,
            });
        }
    }
    // Each onset goes to the marker of its frequency nearest in time
    // (within half the period of a frequency); a marker keeps its nearest.
    let half_us = spec.marker_period_ms * nf as f64 * 1000.0 / 2.0;
    let mut best: Vec<Option<f64>> = vec![None; markers.len()];
    for (fi, s) in audio::marker_onsets(spec, &sound.left) {
        let heard = sound.time_us((s as f64 - bias).max(0.0));
        let cand = markers
            .iter()
            .enumerate()
            .filter(|(_, mk)| (mk.k % per_loop) % nf == fi as u64)
            .map(|(i, mk)| (i, heard - mk.video_us))
            .filter(|(_, d)| d.abs() < half_us)
            .min_by(|a, b| a.1.abs().total_cmp(&b.1.abs()));
        if let Some((i, d)) = cand {
            if best[i].is_none_or(|b| d.abs() < b.abs()) {
                best[i] = Some(d);
            }
        }
    }
    for (mk, d) in markers.iter_mut().zip(best) {
        mk.audio_us = d.map(|d| mk.video_us + d);
    }
    markers
}

// ---- the loopback row -------------------------------------------------------------

/// The player's bursts: 1 kHz, 100 ms on, every 1000 ms.
pub const TONE_HZ: f64 = 1000.0;
pub const TONE_ON_MS: u32 = 100;
pub const TONE_PERIOD_MS: u32 = 1000;
/// The player's level (0.3 of full scale, tone_player.cpp).
const TONE_LEVEL: f64 = 0.3 * 32767.0;
const TONE_WIN: usize = 96;
const TONE_HOP: usize = 48;
/// Under this window level (-30 dBFS) the sound has no bursts at all.
const TONE_MIN_LEVEL: f64 = 1000.0;

/// Onsets (samples) of tone bursts at `freq`: a run of at least 20 ms over
/// half the level of the loudest window, at least 200 ms after the run
/// before it.
pub fn tone_onsets(x: &[i16], freq: f64) -> Vec<usize> {
    if x.len() < TONE_WIN {
        return Vec::new();
    }
    let hops = (x.len() - TONE_WIN) / TONE_HOP + 1;
    let levels: Vec<f64> = (0..hops)
        .map(|h| audio::tone_amplitude(&x[h * TONE_HOP..h * TONE_HOP + TONE_WIN], freq))
        .collect();
    let peak = levels.iter().copied().fold(0.0, f64::max);
    if peak < TONE_MIN_LEVEL {
        return Vec::new();
    }
    let threshold = 0.5 * peak;
    let hop_ms = TONE_HOP as f64 * 1000.0 / SAMPLE_RATE as f64;
    let (min_run, quiet) = ((20.0 / hop_ms) as usize, (200.0 / hop_ms) as usize);
    let mut out = Vec::new();
    let mut run: Option<usize> = None;
    let mut last_end: Option<usize> = None;
    // One window under the threshold after the last ends a run there.
    for (h, &level) in levels.iter().chain(std::iter::once(&0.0)).enumerate() {
        let on = level >= threshold;
        match (on, run) {
            (true, None) => run = Some(h),
            (false, Some(r)) => {
                if h - r >= min_run && last_end.is_none_or(|e| r - e >= quiet) {
                    out.push(r * TONE_HOP);
                }
                last_end = Some(h);
                run = None;
            }
            _ => {}
        }
    }
    out
}

/// The detector's own delay on a burst like the player's, in samples
/// (negative: it finds the onset early).
pub fn tone_bias() -> f64 {
    let onset = 9_600 + 17;
    let on = TONE_ON_MS as usize * SAMPLE_RATE / 1000;
    let mut x = vec![0i16; onset + on + 9_600];
    let step = 2.0 * std::f64::consts::PI * TONE_HZ / SAMPLE_RATE as f64;
    for j in 0..on {
        x[onset + j] = (TONE_LEVEL * (step * j as f64).sin()).round() as i16;
    }
    tone_onsets(&x, TONE_HZ)
        .first()
        .map_or(0.0, |&s| s as f64 - onset as f64)
}

/// One burst that the player played inside the clip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Burst {
    /// Its row in the player's CSV.
    pub row: usize,
    /// When the device played its first sample (steady clock, µs).
    pub onset_us: i64,
    /// Clip time of that moment: the clip time of the frame the source
    /// captured then, plus the time since that frame's capture.
    pub expected_us: f64,
    /// Clip time of its onset in the clip's sound; None: not found.
    pub heard_us: Option<f64>,
}

impl Burst {
    pub fn offset_ms(&self) -> Option<f64> {
        self.heard_us.map(|h| (h - self.expected_us) / 1000.0)
    }
}

/// The bursts of the player's CSV inside the clip, each with the nearest
/// burst in the clip's sound (within 500 ms).
pub fn score_loopback(
    onsets_us: &[i64],
    clock: &SourceClock,
    frames: &BTreeMap<i64, i64>,
    sound: &ClipAudio,
    bias: f64,
) -> Vec<Burst> {
    let heard: Vec<f64> = tone_onsets(&sound.left, TONE_HZ)
        .iter()
        .map(|&s| sound.time_us((s as f64 - bias).max(0.0)))
        .collect();
    let mut out = Vec::new();
    for (row, &s) in onsets_us.iter().enumerate() {
        let Some(n) = u64::try_from(s).ok().and_then(|t| frame_at(clock, t)) else {
            continue;
        };
        let Some(&t) = frames.get(&(n as i64)) else {
            continue;
        };
        let expected = t as f64 + (s - due_us(clock, n) as i64) as f64;
        if expected < sound.start_us as f64 + MARKER_EDGE_US
            || expected + f64::from(TONE_ON_MS) * 1000.0 > sound.end_us()
        {
            continue;
        }
        let near = heard
            .iter()
            .map(|&h| h - expected)
            .filter(|d| d.abs() < 500_000.0)
            .min_by(|a, b| a.abs().total_cmp(&b.abs()));
        out.push(Burst {
            row,
            onset_us: s,
            expected_us: expected,
            heard_us: near.map(|d| expected + d),
        });
    }
    out
}

/// The onset column of the player's CSV (`burst,onset_frame,onset_steady_us`).
pub fn read_player_onsets(csv: &Path) -> Result<Vec<i64>, String> {
    let text = std::fs::read_to_string(csv).map_err(|e| format!("{}: {e}", csv.display()))?;
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let Some(v) = line.split(',').nth(2) else {
            continue;
        };
        out.push(
            v.trim()
                .parse::<i64>()
                .map_err(|e| format!("{}: '{line}': {e}", csv.display()))?,
        );
    }
    Ok(out)
}

/// The pid of the player's first line ("pid 1234, 48000 Hz, ...").
pub fn parse_player_pid(line: &str) -> Option<u32> {
    line.trim()
        .strip_prefix("pid ")?
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// The tone player, running.
pub struct TonePlayer {
    child: std::process::Child,
    pub pid: u32,
    rest: Option<std::thread::JoinHandle<()>>,
}

impl TonePlayer {
    /// Start `exe` for `seconds` and read its pid from its first line.
    pub fn start(exe: &Path, seconds: f64, csv: &Path) -> Result<Self, String> {
        use std::io::BufRead;
        let mut child = std::process::Command::new(exe)
            .args([
                "--seconds",
                &format!("{seconds}"),
                "--period-ms",
                &TONE_PERIOD_MS.to_string(),
                "--on-ms",
                &TONE_ON_MS.to_string(),
                "--freq",
                &format!("{TONE_HZ}"),
                "--csv",
                &csv.to_string_lossy(),
            ])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("{}: {e}", exe.display()))?;
        let stdout = child.stdout.take().ok_or("the tone player has no stdout")?;
        // The first line on a thread: a player that hangs before it does
        // not hang the gate. The thread then drains the rest.
        let (tx, rx) = mpsc::channel();
        let rest = std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let _ = tx.send(line);
            let mut sink = String::new();
            let _ = std::io::Read::read_to_string(&mut reader, &mut sink);
        });
        let first = rx.recv_timeout(Duration::from_secs(10)).unwrap_or_default();
        let Some(pid) = parse_player_pid(&first) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "the tone player did not start (first line '{}')",
                first.trim()
            ));
        };
        if pid != child.id() {
            log::warn!(
                "the tone player says pid {pid}, the process is {}",
                child.id()
            );
        }
        Ok(Self {
            child,
            pid,
            rest: Some(rest),
        })
    }

    /// The process handle, for the load guard.
    pub fn handle(&self) -> *mut c_void {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            self.child.as_raw_handle()
        }
        #[cfg(not(windows))]
        {
            std::ptr::null_mut()
        }
    }

    /// Wait for the player to finish (it writes its CSV at the end); kill it
    /// after `timeout`.
    pub fn finish(mut self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    if let Some(t) = self.rest.take() {
                        let _ = t.join();
                    }
                    return if status.success() {
                        Ok(())
                    } else {
                        Err(format!("the tone player exited with {status}"))
                    };
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(None) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return Err("the tone player did not finish in time".into());
                }
                Err(e) => return Err(format!("the tone player: {e}")),
            }
        }
    }
}

impl Drop for TonePlayer {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

// ---- gates ------------------------------------------------------------------------

fn gate(metric: &str, min: Option<f64>, max: Option<f64>, note: &str) -> Gate {
    Gate {
        metric: metric.to_owned(),
        min,
        max,
        enforced: true,
        note: note.to_owned(),
    }
}

/// The absolute gates of a reference clip (plan 10.3). `--write-baseline`
/// writes them into gates.json; a test keeps the committed file equal.
pub fn structural_gates(fps: u32) -> Vec<Gate> {
    vec![
        gate(
            "duration_s",
            Some(30.0),
            Some(31.0),
            "plan 3.4: a 30 s clip from the key frame at or before its start is 30 to 31 s long",
        ),
        gate(
            "start_lead_frames",
            Some(0.0),
            Some(f64::from(fps)),
            "plan 10.3: the first frame is the frame captured 30 s before the press, or up to one GOP (1 s) earlier",
        ),
        gate(
            "frames_missing",
            None,
            Some(0.0),
            "plan 10.3: no missing frame index from the first frame to the last",
        ),
        gate(
            "frames_out_of_order",
            None,
            Some(0.0),
            "plan 10.3: every frame index in order",
        ),
        gate(
            "frames_unreadable",
            None,
            Some(0.0),
            "every frame code reads",
        ),
        gate(
            "frames_undecoded",
            None,
            Some(0.0),
            "every video unit of the clip decodes to a picture",
        ),
        gate(
            "av_markers_missing",
            None,
            Some(0.0),
            "the sound has every game audio marker of the clip's frames",
        ),
        gate(
            "av_offset_abs_max_ms",
            None,
            Some(20.0),
            "plan 10.3: game audio burst to video frame offset within +-20 ms for every marker",
        ),
    ]
}

/// The gates of the loopback row (plan 10.3, real loopback source).
pub fn loopback_gates() -> Vec<Gate> {
    vec![
        gate(
            "duration_s",
            Some(30.0),
            Some(31.0),
            "plan 3.4: a 30 s clip from the key frame at or before its start is 30 to 31 s long",
        ),
        gate(
            "loopback_bursts_missing",
            None,
            Some(0.0),
            "the sound has every burst that the player played inside the clip",
        ),
        gate(
            "loopback_offset_abs_max_ms",
            None,
            Some(20.0),
            "plan 10.3: a real process loopback, burst onset to the frame captured then within +-20 ms",
        ),
    ]
}

/// The quality floors: (metric, fixed margin, share of the value). A floor
/// is the first run's value minus the largest of the fixed margin, the share
/// and 1.1 x the spread of the runs, the rule of
/// `tools/stream-gate/make_gates.py`.
pub const FLOOR_RULES: &[(&str, f64, f64)] = &[
    ("vmaf_mean", 2.0, 0.0),
    ("psnr_y_mean", 1.0, 0.0),
    ("ssim_y_mean", 0.01, 0.0),
];

/// The floor of one metric from its values over the runs (the first is the
/// baseline). None: no value, or a floor at or under 0.
pub fn floor_gate(metric: &str, fixed: f64, share: f64, values: &[f64]) -> Option<Gate> {
    let b = *values.first()?;
    let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let spread = hi - lo;
    let margin = fixed.max(share * b.abs()).max(1.1 * spread);
    // Down to 4 decimals: SSIM is under 1, so 2 decimals would widen its
    // margin by up to 0.01. The 1e-6 keeps a float error from taking one
    // more step down.
    let min = ((b - margin) * 1e4 + 1e-6).floor() / 1e4;
    (min > 0.0).then(|| {
        gate(
            metric,
            Some(min),
            None,
            &format!(
                "run 1 {b:.4} minus {margin:.4} (spread {spread:.4} over {} runs; margin = max(fixed {fixed}, {:.0} % of the value, 1.1 x spread))",
                values.len(),
                share * 100.0
            ),
        )
    })
}

/// Free bytes for this user on the volume of `path`.
pub fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn GetDiskFreeSpaceExW(
                dir: *const u16,
                available: *mut u64,
                total: *mut u64,
                free: *mut u64,
            ) -> i32;
        }
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        // SAFETY: a NUL-terminated path and valid out pointers.
        let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
        (ok != 0).then_some(avail)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> SourceClock {
        SourceClock {
            start_us: 5_000_000,
            clip_frames: 1200,
            fps: 60,
        }
    }

    fn code(index: u32) -> Option<FrameCode> {
        Some(FrameCode {
            index,
            clock_ms: index * 1000 / 60,
        })
    }

    /// The frames of a clip from absolute frame `first`, `n` long.
    fn clip_frames(first: u64, n: u64) -> Vec<Frame> {
        let c = clock();
        (first..first + n)
            .map(|a| Frame {
                time_us: (due_us(&c, a) - due_us(&c, first)) as i64,
                code: code((a % 1200) as u32),
            })
            .collect()
    }

    #[test]
    fn a_time_maps_to_the_frame_captured_then() {
        let c = clock();
        assert_eq!(frame_at(&c, c.start_us - 1), None);
        assert_eq!(frame_at(&c, c.start_us), Some(0));
        // Frame 1 is due at 16666 µs: the integer formula, not 16667.
        assert_eq!(frame_at(&c, c.start_us + 16_665), Some(0));
        assert_eq!(frame_at(&c, c.start_us + 16_666), Some(1));
        for n in [59u64, 60, 1199, 1200, 3_599, 123_457] {
            assert_eq!(frame_at(&c, due_us(&c, n)), Some(n));
            assert_eq!(frame_at(&c, due_us(&c, n + 1) - 1), Some(n));
        }
    }

    #[test]
    fn a_loop_index_unwraps_to_the_nearest_pass() {
        assert_eq!(unwrap_nearest(300, 2_400 + 290, 1200), 2_700);
        assert_eq!(unwrap_nearest(1_199, 1_230, 1200), 1_199);
        assert_eq!(unwrap_nearest(5, 1_190, 1200), 1_205);
        assert_eq!(unwrap_nearest(10, 0, 1200), 10);
    }

    #[test]
    fn a_clean_clip_across_a_loop_has_its_frames_in_order() {
        // 30 s from frame 1_140: over the loop end at 1_200.
        let frames = clip_frames(1_140, 1_800);
        let o = frame_order(&frames, 1_140 - 30, 1200, 60);
        assert_eq!((o.unreadable, o.missing, o.out_of_order), (0, 0, 0));
        assert_eq!(o.abs.first(), Some(&Some(1_140)));
        assert_eq!(o.abs.last(), Some(&Some(2_939)));
    }

    #[test]
    fn a_skipped_frame_is_missing_and_a_repeat_is_out_of_order() {
        let mut frames = clip_frames(600, 300);
        // Frames 700 to 702 never made it: the next unit is 703.
        frames.drain(100..103);
        let o = frame_order(&frames, 600, 1200, 60);
        assert_eq!((o.missing, o.out_of_order), (3, 0));

        let mut frames = clip_frames(600, 300);
        frames[50].code = code(649);
        let o = frame_order(&frames, 600, 1200, 60);
        assert_eq!(o.out_of_order, 1);

        let mut frames = clip_frames(600, 300);
        frames[10].code = None;
        let o = frame_order(&frames, 600, 1200, 60);
        assert_eq!((o.unreadable, o.missing, o.out_of_order), (1, 0, 0));
    }

    fn spec() -> AudioSpec {
        let dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks/baselines/stream");
        audio::load(&dir.join("audio.json")).expect("audio.json loads")
    }

    /// A clip of 30 s from absolute frame `first` and its sound, the sound
    /// late by `late_ms`.
    fn av_clip(first: u64, late_ms: f64) -> (AudioSpec, f64, ClipAudio, BTreeMap<i64, i64>) {
        let spec = spec();
        let corpus =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../libmello/tests/fixtures/voice");
        let loop_samples = 20 * SAMPLE_RATE;
        let reference = audio::build(&spec, &corpus, loop_samples).expect("reference");
        let bias = audio::detector_bias(&spec, &reference);
        let c = clock();
        let frames: BTreeMap<i64, i64> = (first..first + 1_800)
            .map(|a| (a as i64, (due_us(&c, a) - due_us(&c, first)) as i64))
            .collect();
        // Sample 0 of the sound is the capture of the first frame.
        let from = (due_us(&c, first) - c.start_us) as f64 * SAMPLE_RATE as f64 / 1e6;
        let late = late_ms * SAMPLE_RATE as f64 / 1000.0;
        let left: Vec<i16> = (0..30 * SAMPLE_RATE)
            .map(|j| {
                let src = from + j as f64 - late;
                if src < 0.0 {
                    0
                } else {
                    reference.stereo[2 * ((src as usize) % loop_samples)]
                }
            })
            .collect();
        (spec, bias, ClipAudio { start_us: 0, left }, frames)
    }

    #[test]
    fn markers_on_time_have_no_offset_across_a_loop() {
        let (spec, bias, sound, frames) = av_clip(1_140, 0.0);
        let m = score_markers(&spec, bias, &sound, &frames, 1200, 60);
        assert!(m.len() >= 118, "{} markers", m.len());
        for mk in &m {
            let off = mk.offset_ms().expect("found");
            assert!(off.abs() < 1.5, "marker {} at {off} ms", mk.k);
        }
    }

    #[test]
    fn a_late_sound_reads_as_its_offset() {
        let (spec, bias, sound, frames) = av_clip(1_500, 25.0);
        let m = score_markers(&spec, bias, &sound, &frames, 1200, 60);
        assert!(!m.is_empty());
        for mk in &m {
            let off = mk.offset_ms().expect("found");
            assert!((off - 25.0).abs() < 1.5, "marker {} at {off} ms", mk.k);
        }
    }

    #[test]
    fn a_hole_in_the_sound_is_a_missing_marker() {
        let (spec, bias, mut sound, frames) = av_clip(1_500, 0.0);
        // 600 ms of silence from 10 s: three markers fall in it.
        sound.left[10 * SAMPLE_RATE..10 * SAMPLE_RATE + 28_800].fill(0);
        let m = score_markers(&spec, bias, &sound, &frames, 1200, 60);
        let missing = m.iter().filter(|mk| mk.audio_us.is_none()).count();
        assert_eq!(missing, 3, "markers at 10.0, 10.25 and 10.5 s");
    }

    #[test]
    fn audio_buffers_go_to_their_time() {
        let a = place_audio(&[
            (0, vec![1; 1024]),
            (21_333, vec![2; 1024]),
            (100_000, vec![3; 10]),
        ]);
        assert_eq!(a.left.len(), 4_800 + 10);
        assert_eq!(a.left[1023], 1);
        assert_eq!(a.left[1024], 2);
        assert_eq!(a.left[2048], 0);
        assert_eq!(a.left[4_800], 3);
    }

    /// The player's bursts as the clip has them: frames from absolute 600,
    /// the sound from the capture of frame 600, the bursts `late_ms` late.
    fn loopback_clip(late_ms: f64) -> (Vec<i64>, SourceClock, BTreeMap<i64, i64>, ClipAudio) {
        let c = clock();
        let first = 600u64;
        let frames: BTreeMap<i64, i64> = (first..first + 1_800)
            .map(|a| (a as i64, (due_us(&c, a) - due_us(&c, first)) as i64))
            .collect();
        let t0 = due_us(&c, first) as i64;
        let mut left = vec![0i16; 30 * SAMPLE_RATE];
        let mut onsets = Vec::new();
        let step = 2.0 * std::f64::consts::PI * TONE_HZ / SAMPLE_RATE as f64;
        for b in 0..29 {
            // Onsets on the steady clock, not on frame times.
            let onset = t0 + 700_000 + b * 1_000_000 + 3_217;
            onsets.push(onset);
            let at = ((onset - t0) as f64 / 1e6 + late_ms / 1000.0) * SAMPLE_RATE as f64;
            for j in 0..TONE_ON_MS as usize * 48 {
                if let Some(v) = left.get_mut(at as usize + j) {
                    *v = (TONE_LEVEL * (step * j as f64).sin()) as i16;
                }
            }
        }
        (onsets, c, frames, ClipAudio { start_us: 0, left })
    }

    #[test]
    fn loopback_bursts_on_time_have_no_offset() {
        let (onsets, c, frames, sound) = loopback_clip(0.0);
        let b = score_loopback(&onsets, &c, &frames, &sound, tone_bias());
        assert_eq!(b.len(), 29);
        for x in &b {
            let off = x.offset_ms().expect("heard");
            assert!(off.abs() < 1.0, "burst {} at {off} ms", x.row);
        }
        let (onsets, c, frames, sound) = loopback_clip(12.0);
        let b = score_loopback(&onsets, &c, &frames, &sound, tone_bias());
        for x in &b {
            let off = x.offset_ms().expect("heard");
            assert!((off - 12.0).abs() < 1.0, "burst {} at {off} ms", x.row);
        }
    }

    #[test]
    fn the_player_pid_and_onsets_parse() {
        assert_eq!(
            parse_player_pid(
                "pid 4321, 48000 Hz, 2 channels, a 100 ms burst every 1000 ms for 36.0 s\n"
            ),
            Some(4321)
        );
        assert_eq!(parse_player_pid("usage: mello_tone_player"), None);
        let dir = std::env::temp_dir().join(format!("replay-onsets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let csv = dir.join("onsets.csv");
        std::fs::write(
            &csv,
            "burst,onset_frame,onset_steady_us\n0,48000,123456789\n1,96000,124456789\n",
        )
        .expect("csv");
        assert_eq!(
            read_player_onsets(&csv).expect("reads"),
            vec![123_456_789, 124_456_789]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_floor_is_the_value_less_the_largest_margin() {
        let g = floor_gate("vmaf_mean", 2.0, 0.0, &[95.5, 95.0, 96.0]).expect("floor");
        assert_eq!(g.min, Some(93.5));
        // A spread over the fixed margin sets it: 1.1 x 3 = 3.3.
        let g = floor_gate("vmaf_mean", 2.0, 0.0, &[90.0, 87.0, 89.0]).expect("floor");
        assert_eq!(g.min, Some(86.7));
        let g = floor_gate("ssim_y_mean", 0.01, 0.0, &[0.973456]).expect("floor");
        assert_eq!(g.min, Some(0.9634));
        assert!(floor_gate("vmaf_mean", 2.0, 0.0, &[]).is_none());
    }

    /// Every committed floor is `floor_gate` on the run values that
    /// `--write-baseline` kept in `_spread`, and every kept value has its
    /// floor.
    #[test]
    fn the_committed_floors_follow_the_rule() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../benchmarks/baselines/replay/gates.json");
        let root: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("gates.json")).expect("json");
        let gates = crate::profile::load_gates(&path).expect("gates.json");
        let spread = root.get("_spread").and_then(|s| s.as_object());
        for (lane, rows) in spread.into_iter().flatten() {
            for (row, metrics) in rows.as_object().expect("rows") {
                let list = &gates.lanes[lane.as_str()][row.as_str()];
                for (metric, values) in metrics.as_object().expect("metrics") {
                    let values: Vec<f64> = values
                        .as_array()
                        .expect("values")
                        .iter()
                        .filter_map(|v| v.as_f64())
                        .collect();
                    let &(_, fixed, share) = FLOOR_RULES
                        .iter()
                        .find(|r| r.0 == metric)
                        .expect("a floor rule");
                    let want = floor_gate(metric, fixed, share, &values).expect("floor");
                    let got = list
                        .iter()
                        .find(|g| &g.metric == metric)
                        .expect("floor gate");
                    assert_eq!(got.min, want.min, "{lane} {row} {metric}");
                }
            }
        }
    }

    #[test]
    fn the_committed_gates_carry_the_structural_gates() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../benchmarks/baselines/replay/gates.json");
        let gates = crate::profile::load_gates(&path).expect("gates.json");
        for l in LANES {
            let per = gates.lanes.get(l.name).expect("lane");
            let (_, fps, _) = l.quality.encoding();
            for (name, _) in PRESSES {
                let list = per.get(*name).expect("press");
                for want in structural_gates(fps) {
                    let got = list.iter().find(|g| g.metric == want.metric).expect("gate");
                    assert_eq!(
                        (got.min, got.max),
                        (want.min, want.max),
                        "{} {name} {}",
                        l.name,
                        want.metric
                    );
                    assert!(got.enforced);
                }
            }
            // The streamed playback row: its gates are the code's, and no
            // other gate.
            let list = per.get(crate::playback::ROW).expect("playback row");
            let want = crate::playback::gates();
            assert_eq!(list.len(), want.len(), "{} playback", l.name);
            for want in want {
                let got = list.iter().find(|g| g.metric == want.metric).expect("gate");
                assert_eq!(
                    (got.min, got.max, got.enforced),
                    (want.min, want.max, want.enforced),
                    "{} playback {}",
                    l.name,
                    want.metric
                );
            }
        }
        let hw = &gates.lanes["hw"][LOOPBACK];
        for want in loopback_gates() {
            let got = hw.iter().find(|g| g.metric == want.metric).expect("gate");
            assert_eq!((got.min, got.max), (want.min, want.max));
        }
    }
}
