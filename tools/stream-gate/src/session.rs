//! The stream path of one profile run: the host and the viewers, in this
//! process, through the shim and the real SFU.
//!
//! Host. The client's order (mello-core `client/streaming.rs`): the native
//! host starts first (`host::start_host`, here on the reference source), then
//! the SFU connect, join and transport wait, then `create_stream_session`
//! with the `SfuSink`. A host thread polls the SFU events every 16 ms and
//! passes viewer joins and leaves to the sink, and pings every 2 s, as
//! `poll_sfu_host_membership_events` does.
//!
//! Viewer. The client's viewer (streaming-quality stage 3): the viewer
//! pipeline is attached to the viewer peer (`mello_stream_viewer_attach_peer`)
//! and libmello runs it on its own threads: the receive timeline releases
//! each frame, the decode thread decodes it, the present thread shows it at
//! its render time. The 16 ms loop here does what the client's stream tick
//! does: the receive-target controller (`ViewerCongestionController`), the
//! ping and the SFU events. One difference: the client registers a native
//! frame callback (the GPU texture for the compositor); the harness registers
//! the CPU frame callback and reads every presented frame back, so it can
//! read the burned-in code and score the pixels. The readback runs on the
//! present thread after the present time is taken.

use crate::code::{self, FrameCode};
use mello_core::stream::congestion::{CongestionSample, ViewerCongestionController};
use mello_core::stream::host::{self, StartStreamResponse};
use mello_core::stream::sink::PacketSink;
use mello_core::stream::sink_sfu::SfuSink;
use mello_core::stream::{Codec, StreamConfig};
use mello_core::transport::{SfuConnection, SfuEvent, StreamPeerRole};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The tick of the client's stream loop.
pub const TICK: Duration = Duration::from_millis(16);

pub fn clock_us() -> u64 {
    // SAFETY: no arguments, reads the steady clock.
    unsafe { mello_sys::mello_clock_now_us() }
}

/// The SFU's test-mode WebSocket join (01-SFU.md section 5.1).
/// `SfuConnection::connect` appends `?token=<token>`, so the test parameters
/// ride in the token.
pub fn test_token(password: &str, user_id: &str) -> String {
    format!("gate&test_password={password}&test_type=stream&test_user_id={user_id}&test_username={user_id}")
}

/// One native context per role: the host and each viewer run as separate
/// machines would, with their own devices.
pub struct Ctx(pub *mut mello_sys::MelloContext);
unsafe impl Send for Ctx {}
unsafe impl Sync for Ctx {}

impl Ctx {
    pub fn new() -> Result<Self, String> {
        // SAFETY: plain constructor.
        let c = unsafe { mello_sys::mello_init() };
        if c.is_null() {
            return Err("mello_init failed".into());
        }
        Ok(Self(c))
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        // SAFETY: created by mello_init, destroyed once.
        unsafe { mello_sys::mello_destroy(self.0) };
    }
}

/// Host samples every 100 ms.
#[derive(Clone, Debug, Default)]
pub struct HostSample {
    pub t_us: u64,
    pub encoder_kbps: u32,
    pub pacing_target_kbps: u64,
    pub gcc_target_kbps: u64,
    pub keyframes: u32,
    /// The stage 4 sender's pacing rate, and the encode size and frame rate
    /// of the quality controller's rung.
    pub pacing_rate_kbps: u64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Sender counters (cumulative) for the congestion timeline: wire
    /// bytes, retransmit bytes, PLIs, local IDR requests, access units the
    /// bounded queue dropped, and the queue depth now.
    pub wire_bytes: u64,
    pub rtx_bytes: u64,
    pub plis: u64,
    pub local_idrs: u64,
    pub aus_dropped: u64,
    pub queued_aus: u64,
    /// The encoder target the sender gives Rust (goog_cc less repairs).
    pub enc_target_kbps: u64,
    /// The IDRs the sender has sent, and the enqueue-to-wire time of the
    /// last one (the gate reads the exit time of each IDR from the steps).
    pub idrs_sent: u64,
    pub last_idr_send_us: u64,
}

pub struct Host {
    pub conn: Arc<SfuConnection>,
    pub sink: Arc<SfuSink>,
    pub session: mello_core::stream::manager::StreamSession,
    native: SendPtr,
    pub width: u32,
    pub height: u32,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<Vec<HostSample>>>,
}

#[derive(Clone, Copy)]
struct SendPtr(*mut mello_sys::MelloStreamHost);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

pub struct HostEnd {
    pub samples: Vec<HostSample>,
    pub reference: mello_sys::MelloReferenceInfo,
    pub stats: mello_sys::MelloStreamStats,
    pub video: mello_sys::MelloRtpVideoStats,
}

impl Host {
    /// Start the reference host and publish it to the SFU session.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        rt: &tokio::runtime::Runtime,
        ctx: &Ctx,
        ws: &str,
        password: &str,
        session_id: &str,
        bitrate_kbps: u32,
        fps: u32,
        audio: bool,
        before_connect: impl FnOnce(),
    ) -> Result<Self, String> {
        let source = mello_sys::MelloCaptureSource {
            mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_REFERENCE,
            monitor_index: 0,
            hwnd: std::ptr::null_mut(),
            pid: 0,
            allow_hook: false,
        };
        let config = mello_sys::MelloStreamConfig {
            width: 0,
            height: 0,
            fps,
            bitrate_kbps,
        };
        // SAFETY: ctx is live; the source and config outlive the call.
        let (native, video_rx, audio_rx, teardown) =
            unsafe { host::start_host(ctx.0, &source, &config) }
                .map_err(|e| format!("start_host: {e}"))?;
        let (mut w, mut h) = (0u32, 0u32);
        // SAFETY: native is the live host.
        unsafe { mello_sys::mello_stream_get_host_resolution(native, &mut w, &mut h) };
        // Stage 7: the clip's game audio, in the client's order (right after
        // the native start). Only with a reference audio track: without one
        // the host would open the loopback device.
        if audio {
            // SAFETY: native is the live host.
            let r = unsafe { mello_sys::mello_stream_start_audio(native) };
            if r != mello_sys::MelloResult_MELLO_OK {
                return Err(format!("mello_stream_start_audio: {r}"));
            }
        }

        before_connect();
        let user = format!("gate-host-{session_id}");
        let conn = rt.block_on(async {
            let mut conn = SfuConnection::connect(ws, &test_token(password, &user))
                .await
                .map_err(|e| format!("host connect: {e}"))?;
            // SAFETY: ctx is live.
            let peer =
                unsafe { SfuConnection::create_stream_peer(ctx.0, StreamPeerRole::Host, &[]) }
                    .map_err(|e| format!("host peer: {e}"))?;
            conn.join_stream(peer, session_id, "host")
                .await
                .map_err(|e| format!("host join: {e}"))?;
            conn.wait_for_datachannel_open()
                .await
                .map_err(|e| format!("host transport: {e}"))?;
            Ok::<_, String>(Arc::new(conn))
        })?;

        let sink = Arc::new(SfuSink::new(Arc::clone(&conn)));
        let resp = StartStreamResponse {
            session_id: Some(session_id.to_owned()),
            stream_id: None,
            mode: "sfu".to_owned(),
            sfu_endpoint: Some(ws.to_owned()),
            sfu_token: None,
            capture: None,
        };
        let preset = if w >= 1920 {
            mello_core::stream::QualityPreset::Ultra
        } else {
            mello_core::stream::QualityPreset::Medium
        };
        let cfg = StreamConfig {
            width: w.max(1),
            height: h.max(1),
            fps,
            bitrate_kbps,
            capture_desc: "reference clip".to_owned(),
            ..StreamConfig::from_preset(preset, Codec::H264)
        };
        let sink_dyn: Arc<dyn PacketSink> = Arc::clone(&sink) as _;
        let session = rt
            .block_on(async {
                host::create_stream_session(
                    ctx.0, native, &resp, cfg, video_rx, audio_rx, teardown, sink_dyn, None,
                )
            })
            .map_err(|e| format!("create_stream_session: {e}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (conn, sink, stop) = (Arc::clone(&conn), Arc::clone(&sink), Arc::clone(&stop));
            let handle = rt.handle().clone();
            let native = SendPtr(native);
            std::thread::Builder::new()
                .name("gate-host".into())
                .spawn(move || host_loop(handle, conn, sink, native, stop))
                .map_err(|e| e.to_string())?
        };
        Ok(Self {
            conn,
            sink,
            session,
            native: SendPtr(native),
            width: w,
            height: h,
            stop,
            thread: Some(thread),
        })
    }

    /// Stop the host, leave the session, and return what it did.
    pub fn stop(mut self, rt: &tokio::runtime::Runtime) -> HostEnd {
        self.stop.store(true, Ordering::Relaxed);
        let samples = self
            .thread
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_default();
        // SAFETY: the host is live until the session stops below.
        let mut reference: mello_sys::MelloReferenceInfo = unsafe { std::mem::zeroed() };
        let mut stats: mello_sys::MelloStreamStats = unsafe { std::mem::zeroed() };
        unsafe {
            mello_sys::mello_stream_reference_info(self.native.0, &mut reference);
            mello_sys::mello_stream_get_stats(self.native.0, &mut stats);
        }
        let video = self
            .conn
            .video_stats()
            .unwrap_or_else(|_| unsafe { std::mem::zeroed() });
        self.session.stop();
        rt.block_on(async {
            self.conn.leave().await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        HostEnd {
            samples,
            reference,
            stats,
            video,
        }
    }

    /// The reference source's clock (valid once the host runs).
    pub fn reference(&self) -> mello_sys::MelloReferenceInfo {
        let mut r: mello_sys::MelloReferenceInfo = unsafe { std::mem::zeroed() };
        // SAFETY: the host is live while self is.
        unsafe { mello_sys::mello_stream_reference_info(self.native.0, &mut r) };
        r
    }
}

fn host_loop(
    rt: tokio::runtime::Handle,
    conn: Arc<SfuConnection>,
    sink: Arc<SfuSink>,
    native: SendPtr,
    stop: Arc<AtomicBool>,
) -> Vec<HostSample> {
    let mut samples = Vec::new();
    let mut tick = 0u64;
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        tick += 1;
        if tick.is_multiple_of(125) {
            conn.send_ping();
        }
        for ev in conn.poll_events() {
            match ev {
                SfuEvent::MemberJoined { user_id, role } if role == "viewer" => {
                    rt.block_on(sink.on_viewer_joined(&user_id));
                }
                SfuEvent::MemberLeft { user_id, .. } => rt.block_on(sink.on_viewer_left(&user_id)),
                SfuEvent::Disconnected { reason } => {
                    log::warn!("gate host: SFU disconnected: {reason}")
                }
                _ => {}
            }
        }
        if tick.is_multiple_of(6) {
            let mut st: mello_sys::MelloStreamStats = unsafe { std::mem::zeroed() };
            // SAFETY: the host lives until stop() joins this thread.
            unsafe { mello_sys::mello_stream_get_stats(native.0, &mut st) };
            let v = conn
                .video_stats()
                .unwrap_or_else(|_| unsafe { std::mem::zeroed() });
            let (mut w, mut h) = (0u32, 0u32);
            // SAFETY: as above.
            unsafe { mello_sys::mello_stream_get_host_resolution(native.0, &mut w, &mut h) };
            samples.push(HostSample {
                t_us: clock_us(),
                encoder_kbps: st.bitrate_kbps,
                pacing_target_kbps: v.tx_pacing_target_bps / 1000,
                gcc_target_kbps: v.tx_gcc_target_bps / 1000,
                keyframes: st.keyframes_sent,
                pacing_rate_kbps: v.tx_pacing_rate_bps / 1000,
                width: w,
                height: h,
                fps: st.fps_actual,
                wire_bytes: v.tx_rtp_wire_bytes_sent,
                rtx_bytes: v.tx_rtx_bytes_sent,
                plis: v.tx_pli_requests,
                local_idrs: v.tx_local_idr_requests,
                aus_dropped: v.tx_access_units_dropped,
                queued_aus: v.tx_queued_access_units,
                enc_target_kbps: v.tx_encoder_target_bps / 1000,
                idrs_sent: v.tx_idr_access_units_sent,
                last_idr_send_us: v.tx_last_idr_send_us,
            });
        }
        next += TICK;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    samples
}

/// One presented frame as the viewer saw it.
#[derive(Clone, Copy, Debug)]
pub struct PresentRec {
    pub t_us: u64,
    pub code: Option<FrameCode>,
    /// Slot of its luma plane in the distorted file (written for a readable
    /// code that differs from the present before it).
    pub slot: Option<u64>,
}

/// Where the frame callback puts each presented frame. The callback runs on
/// libmello's present thread, so it only copies the pixels and hands them to
/// a worker thread; the worker reads the code, converts to luma and writes
/// the file. `ts` is the present thread's present time.
/// One presented frame on its way to the worker: present time and RGBA.
/// (present time, RGBA pixels, width, height).
type FrameMsg = (u64, Vec<u8>, u32, u32);

struct FrameSink {
    tx: Mutex<Option<std::sync::mpsc::Sender<FrameMsg>>>,
    pool: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Frames at another size than the clip (a resolution rung): scaled to
    /// the clip size before the code is read.
    scaled: AtomicU64,
    /// Longest callback, us: the harness's own cost inside the present.
    callback_max_us: AtomicU64,
    width: u32,
    height: u32,
}

unsafe extern "C" fn on_frame(
    user: *mut std::ffi::c_void,
    rgba: *const u8,
    w: u32,
    h: u32,
    ts: u64,
) {
    // SAFETY: user is the FrameSink the viewer owns until it stops.
    let sink = unsafe { &*(user as *const FrameSink) };
    if rgba.is_null() || w == 0 || h == 0 {
        return;
    }
    if w != sink.width || h != sink.height {
        sink.scaled.fetch_add(1, Ordering::Relaxed);
    }
    let started = Instant::now();
    let n = (w as usize) * (h as usize) * 4;
    // SAFETY: libmello hands w * h RGBA pixels for the duration of the call.
    let px = unsafe { std::slice::from_raw_parts(rgba, n) };
    let mut buf = sink
        .pool
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pop()
        .unwrap_or_default();
    buf.clear();
    buf.extend_from_slice(px);
    if let Some(tx) = sink.tx.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        let _ = tx.send((ts, buf, w, h));
    }
    sink.callback_max_us
        .fetch_max(started.elapsed().as_micros() as u64, Ordering::Relaxed);
}

/// The worker of a FrameSink: one present at a time, in order.
fn frame_worker(
    rx: std::sync::mpsc::Receiver<FrameMsg>,
    pool: Arc<Mutex<Vec<Vec<u8>>>>,
    file: std::fs::File,
    width: u32,
    height: u32,
) -> Vec<PresentRec> {
    let mut writer = std::io::BufWriter::with_capacity(16 << 20, file);
    let mut luma = vec![0u8; (width * height) as usize];
    let mut presents = Vec::with_capacity(4096);
    let mut last: Option<FrameCode> = None;
    let mut slots = 0u64;
    let mut scaled = Vec::new();
    for (ts, frame, fw, fh) in rx {
        // A frame at another size (the host stepped its resolution) is
        // scaled to the clip size: the code and the scores read the clip
        // size.
        let px: &[u8] = if fw == width && fh == height {
            &frame
        } else {
            scale_rgba(&frame, fw, fh, width, height, &mut scaled);
            &scaled
        };
        let code = code::read_rgba(px, width as usize);
        let mut slot = None;
        if code.is_some() && code != last {
            for (i, y) in luma.iter_mut().enumerate() {
                *y = code::luma_709(px[i * 4], px[i * 4 + 1], px[i * 4 + 2]);
            }
            if writer.write_all(&luma).is_ok() {
                slot = Some(slots);
                slots += 1;
            }
        }
        last = code;
        presents.push(PresentRec {
            t_us: ts,
            code,
            slot,
        });
        pool.lock().unwrap_or_else(|p| p.into_inner()).push(frame);
    }
    let _ = writer.flush();
    presents
}

/// Bilinear scale of a `sw` x `sh` RGBA frame to `dw` x `dh` into `out`.
fn scale_rgba(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32, out: &mut Vec<u8>) {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    out.resize(dw * dh * 4, 0);
    for y in 0..dh {
        let fy = ((y as f32 + 0.5) * sh as f32 / dh as f32 - 0.5).max(0.0);
        let y0 = (fy as usize).min(sh - 1);
        let y1 = (y0 + 1).min(sh - 1);
        let wy = fy - y0 as f32;
        for x in 0..dw {
            let fx = ((x as f32 + 0.5) * sw as f32 / dw as f32 - 0.5).max(0.0);
            let x0 = (fx as usize).min(sw - 1);
            let x1 = (x0 + 1).min(sw - 1);
            let wx = fx - x0 as f32;
            for c in 0..4 {
                let p = |xx: usize, yy: usize| f32::from(src[(yy * sw + xx) * 4 + c]);
                let top = p(x0, y0) * (1.0 - wx) + p(x1, y0) * wx;
                let bottom = p(x0, y1) * (1.0 - wx) + p(x1, y1) * wx;
                out[(y * dw + x) * 4 + c] = (top * (1.0 - wy) + bottom * wy + 0.5) as u8;
            }
        }
    }
}

/// What a viewer recorded.
pub struct ViewerRecord {
    pub role: String,
    pub join_us: u64,
    pub end_us: u64,
    pub presents: Vec<PresentRec>,
    pub luma_path: PathBuf,
    pub error: Option<String>,
    pub video: Option<mello_sys::MelloRtpVideoStats>,
    pub backlog_drops: u64,
    pub feed_failures: u64,
    pub aus_fed: u64,
    pub aus_idr: u64,
    /// Frames at another size than the clip, scaled before the code was read.
    pub scaled_frames: u64,
    /// Longest tick of the viewer loop, and ticks over 30 ms: the loop's own
    /// stalls (the receive session flushes after a 3-AU backlog).
    pub tick_max_ms: f64,
    pub ticks_over_30ms: u64,
    pub present_max_ms: f64,
    pub callback_max_ms: f64,
    /// The viewer's receive timeline at the end (stage 3).
    pub timeline: Option<TimelineRecord>,
    /// Stage 7: the game audio the viewer's output played, pulled 10 ms at
    /// a time as the device thread does, and its stats at the end.
    pub audio: Option<crate::audio::Capture>,
    pub audio_stats: Option<AudioRecord>,
    /// The viewer's A/V sync state every 100 ms (stage 7, `sync.csv`).
    pub sync_samples: Vec<SyncSample>,
}

/// One sample of the viewer's A/V sync state.
#[derive(Clone, Copy, Debug, Default)]
pub struct SyncSample {
    pub t_us: u64,
    pub av_offset_ms: f32,
    pub video_min_ms: f32,
    pub audio_min_ms: f32,
    pub audio_delay_ms: f32,
    pub video_target_ms: f32,
    pub video_current_ms: f32,
}

/// The viewer's game audio and A/V sync at the end of a run (stage 7).
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioRecord {
    pub delay_ms: f32,
    pub av_offset_ms: f32,
    pub av_video_min_delay_ms: f32,
    pub av_audio_min_delay_ms: f32,
    pub packets: u64,
    pub expand_ms: u64,
    pub fec_used: u64,
    pub sync_extensions: u64,
    pub sync_changes: u64,
}

/// The viewer pipeline's stage 3 counters at the end of a run.
#[derive(Clone, Copy, Debug, Default)]
pub struct TimelineRecord {
    pub target_delay_ms: f32,
    pub jitter_ms: f32,
    pub decode_ms_p95: f32,
    pub frames_released: u64,
    pub frames_late: u64,
    pub frames_dropped_late: u64,
    pub frames_decoded: u64,
    pub frames_presented: u64,
    pub keyframe_requests: u64,
    pub loss_events: u64,
}

pub struct ViewerSpec {
    pub role: String,
    pub ws: String,
    pub password: String,
    pub session_id: String,
    pub width: u32,
    pub height: u32,
    pub receive_kbps: u32,
    pub end_us: u64,
    pub out_dir: PathBuf,
    /// Stage 7: pull the context's audio output every 10 ms and keep it.
    pub capture_audio: bool,
}

/// Pull `ctx`'s mixed output 10 ms at a time on a fixed schedule, as a
/// device thread does, until `stop`. Late pulls catch up (the device clock
/// does not wait).
fn audio_puller(ctx: Arc<Ctx>, stop: Arc<AtomicBool>) -> crate::audio::Capture {
    let mut cap = crate::audio::Capture::default();
    let mut buf = [0i16; crate::audio::PULL];
    let period = Duration::from_micros(10_000);
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
            continue;
        }
        let t = clock_us();
        // SAFETY: ctx is live while the viewer runs; the test backend.
        let n = unsafe {
            mello_sys::mello_voice_test_pull_output(
                ctx.0,
                buf.as_mut_ptr(),
                crate::audio::PULL as i32,
            )
        };
        if n < 0 {
            log::warn!("audio pull: the context has no test backend");
            break;
        }
        cap.pull_us.push(t);
        cap.samples.extend_from_slice(&buf);
        next += period;
    }
    cap
}

/// Run a viewer on its own thread until `spec.end_us`.
pub fn spawn_viewer(
    rt: Arc<tokio::runtime::Runtime>,
    ctx: Arc<Ctx>,
    spec: ViewerSpec,
) -> std::thread::JoinHandle<ViewerRecord> {
    std::thread::Builder::new()
        .name(format!("gate-{}", spec.role))
        .spawn(move || run_viewer(&rt, ctx, spec))
        .expect("spawn viewer thread")
}

fn run_viewer(rt: &tokio::runtime::Runtime, ctx_arc: Arc<Ctx>, spec: ViewerSpec) -> ViewerRecord {
    let ctx: &Ctx = &ctx_arc;
    let join_us = clock_us();
    let luma_path = spec.out_dir.join(format!("{}-luma.y", spec.role));
    let mut rec = ViewerRecord {
        role: spec.role.clone(),
        join_us,
        end_us: spec.end_us,
        presents: Vec::new(),
        luma_path: luma_path.clone(),
        error: None,
        video: None,
        backlog_drops: 0,
        feed_failures: 0,
        aus_fed: 0,
        aus_idr: 0,
        scaled_frames: 0,
        tick_max_ms: 0.0,
        ticks_over_30ms: 0,
        present_max_ms: 0.0,
        callback_max_ms: 0.0,
        timeline: None,
        audio: None,
        audio_stats: None,
        sync_samples: Vec::new(),
    };
    let file = match std::fs::File::create(&luma_path) {
        Ok(f) => f,
        Err(e) => {
            rec.error = Some(format!("{}: {e}", luma_path.display()));
            return rec;
        }
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let pool = Arc::new(Mutex::new(Vec::new()));
    let worker = {
        let pool = Arc::clone(&pool);
        let (w, h) = (spec.width, spec.height);
        std::thread::Builder::new()
            .name(format!("gate-{}-frames", spec.role))
            .spawn(move || frame_worker(rx, pool, file, w, h))
            .expect("spawn frame worker")
    };
    let sink = Box::new(FrameSink {
        tx: Mutex::new(Some(tx)),
        pool,
        scaled: AtomicU64::new(0),
        callback_max_us: AtomicU64::new(0),
        width: spec.width,
        height: spec.height,
    });

    let user = format!("gate-{}-{}", spec.role, spec.session_id);
    let conn = rt.block_on(async {
        let mut conn = SfuConnection::connect(&spec.ws, &test_token(&spec.password, &user))
            .await
            .map_err(|e| format!("{} connect: {e}", spec.role))?;
        // SAFETY: ctx is live.
        let peer = unsafe { SfuConnection::create_stream_peer(ctx.0, StreamPeerRole::Viewer, &[]) }
            .map_err(|e| format!("{} peer: {e}", spec.role))?;
        conn.join_stream(peer, &spec.session_id, "viewer")
            .await
            .map_err(|e| format!("{} join: {e}", spec.role))?;
        conn.wait_for_datachannel_open()
            .await
            .map_err(|e| format!("{} transport: {e}", spec.role))?;
        Ok::<_, String>(conn)
    });
    let conn = match conn {
        Ok(c) => c,
        Err(e) => {
            rec.error = Some(e);
            return rec;
        }
    };

    let config = mello_sys::MelloStreamConfig {
        width: spec.width,
        height: spec.height,
        fps: StreamConfig::default().fps,
        bitrate_kbps: 0,
    };
    let sink_ptr = &*sink as *const FrameSink as *mut std::ffi::c_void;
    // SAFETY: ctx is live; sink outlives the viewer (stopped below first).
    let viewer =
        unsafe { mello_sys::mello_stream_start_viewer(ctx.0, &config, Some(on_frame), sink_ptr) };
    if viewer.is_null() {
        rec.error = Some(format!("{}: mello_stream_start_viewer failed", spec.role));
        rt.block_on(conn.leave());
        return rec;
    }

    // The client's order: the pipeline takes the peer's receive timeline.
    match conn.peer_nonnull() {
        // SAFETY: viewer and peer are live; the viewer stops before `conn` goes.
        Ok(peer) => unsafe {
            if mello_sys::mello_stream_viewer_attach_peer(viewer, peer.as_ptr())
                != mello_sys::MelloResult_MELLO_OK
            {
                rec.error = Some(format!(
                    "{}: mello_stream_viewer_attach_peer failed",
                    spec.role
                ));
            }
        },
        Err(e) => rec.error = Some(format!("{}: no viewer peer: {e}", spec.role)),
    }

    // Stage 7: the viewer's speaker.
    let audio_stop = Arc::new(AtomicBool::new(false));
    let puller = spec.capture_audio.then(|| {
        let (ctx, stop) = (Arc::clone(&ctx_arc), Arc::clone(&audio_stop));
        std::thread::Builder::new()
            .name(format!("gate-{}-audio", spec.role))
            .spawn(move || audio_puller(ctx, stop))
            .expect("spawn audio puller")
    });

    let mut congestion = ViewerCongestionController::new(spec.receive_kbps, Codec::H264);
    let mut tick: u64 = 0;
    let mut next = Instant::now();
    let mut disconnected = false;
    while clock_us() < spec.end_us && !disconnected {
        let tick_start = Instant::now();
        tick += 1;
        if tick.is_multiple_of(125) {
            conn.send_ping();
        }
        if spec.capture_audio && tick.is_multiple_of(6) {
            // SAFETY: viewer is live until the stop below.
            let mut s: mello_sys::MelloStreamStats = unsafe { std::mem::zeroed() };
            unsafe { mello_sys::mello_stream_viewer_get_stats(viewer, &mut s) };
            rec.sync_samples.push(SyncSample {
                t_us: clock_us(),
                av_offset_ms: s.av_offset_ms,
                video_min_ms: s.av_video_min_delay_ms,
                audio_min_ms: s.av_audio_min_delay_ms,
                audio_delay_ms: s.audio_delay_ms,
                video_target_ms: s.target_delay_ms,
                video_current_ms: s.current_delay_ms,
            });
        }
        if conn.is_video_track_open() {
            if let Ok(stats) = conn.video_stats() {
                if let Some(bps) =
                    congestion.tick(CongestionSample::from_native(&stats), Instant::now())
                {
                    if let Err(e) = conn.set_video_receive_target(bps) {
                        log::warn!("{}: receive target: {e}", spec.role);
                    }
                }
            }
        }
        for ev in conn.poll_events() {
            if let SfuEvent::Disconnected { reason } = ev {
                log::warn!("{}: SFU disconnected: {reason}", spec.role);
                rec.error = Some(format!("disconnected: {reason}"));
                disconnected = true;
            }
        }
        let work_ms = tick_start.elapsed().as_secs_f64() * 1000.0;
        rec.tick_max_ms = rec.tick_max_ms.max(work_ms);
        rec.ticks_over_30ms += u64::from(work_ms > 30.0);
        next += TICK;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    rec.video = conn.video_stats().ok();
    {
        // SAFETY: viewer is live until the stop below.
        let mut vstats: mello_sys::MelloStreamStats = unsafe { std::mem::zeroed() };
        unsafe { mello_sys::mello_stream_viewer_get_stats(viewer, &mut vstats) };
        rec.aus_fed = vstats.frames_released;
        rec.feed_failures = vstats.decode_errors;
        rec.timeline = Some(TimelineRecord {
            target_delay_ms: vstats.target_delay_ms,
            jitter_ms: vstats.jitter_ms,
            decode_ms_p95: vstats.decode_ms_p95,
            frames_released: vstats.frames_released,
            frames_late: vstats.frames_late,
            frames_dropped_late: vstats.frames_dropped_late,
            frames_decoded: vstats.frames_decoded,
            frames_presented: vstats.frames_presented,
            keyframe_requests: vstats.keyframe_requests,
            loss_events: vstats.loss_events,
        });
        rec.audio_stats = Some(AudioRecord {
            delay_ms: vstats.audio_delay_ms,
            av_offset_ms: vstats.av_offset_ms,
            av_video_min_delay_ms: vstats.av_video_min_delay_ms,
            av_audio_min_delay_ms: vstats.av_audio_min_delay_ms,
            packets: vstats.audio_packets,
            expand_ms: vstats.audio_expand_ms,
            fec_used: vstats.audio_fec_used,
            sync_extensions: vstats.audio_sync_extensions,
            sync_changes: vstats.av_sync_changes,
        });
    }
    audio_stop.store(true, Ordering::Relaxed);
    if let Some(p) = puller {
        rec.audio = p.join().ok();
    }
    // SAFETY: stop before the sink goes; no callback after it returns.
    unsafe { mello_sys::mello_stream_stop_viewer(viewer) };
    rt.block_on(conn.leave());
    rec.scaled_frames = sink.scaled.load(Ordering::Relaxed);
    rec.callback_max_ms = sink.callback_max_us.load(Ordering::Relaxed) as f64 / 1000.0;
    // Closing the channel ends the worker once it has written every frame.
    sink.tx.lock().unwrap_or_else(|p| p.into_inner()).take();
    rec.presents = worker.join().unwrap_or_default();
    rec
}

/// Read back one luma plane from a viewer's file (for tests and dumps).
pub fn read_luma(path: &Path, slot: u64, w: u32, h: u32) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let n = (w * h) as usize;
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(slot * n as u64))
        .map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Stage 3: after a resolution rung (1080p to 540p) the viewer shows
    // 960x540 frames. The sink dropped every one of them as the wrong size;
    // now it scales them to the clip size and the code still reads.
    #[test]
    fn a_frame_at_half_the_clip_size_still_reads_its_code() {
        let (w, h) = (1920usize, 1080usize);
        let mut y = vec![90u8; w * h];
        let mut u = vec![128u8; w * h / 4];
        let mut v = vec![128u8; w * h / 4];
        let want = code::FrameCode {
            index: 4321,
            clock_ms: 72_015,
        };
        code::paint(want, &mut y, w, &mut u, &mut v, w / 2);
        let full: Vec<u8> = y
            .iter()
            .flat_map(|&l| {
                let g = (((i32::from(l) - 16) * 298 + 128) >> 8).clamp(0, 255) as u8;
                [g, g, g, 255]
            })
            .collect();
        // What the viewer shows after the rung, then what the sink reads.
        let mut half = Vec::new();
        scale_rgba(&full, 1920, 1080, 960, 540, &mut half);
        assert_eq!(half.len(), 960 * 540 * 4);
        let mut back = Vec::new();
        scale_rgba(&half, 960, 540, 1920, 1080, &mut back);
        assert_eq!(code::read_rgba(&back, w), Some(want));
    }
}
