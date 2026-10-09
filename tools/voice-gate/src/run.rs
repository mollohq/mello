//! Drive the real libmello voice path.
//!
//! Two contexts, both on the device-free test backend
//! (`MELLO_AUDIO_BACKEND=test`): a sender that captures and encodes, and a
//! receiver that buffers, decodes, conceals and mixes. Two contexts, not
//! one, so the receiver's mixed output never reaches the sender's echo
//! canceller as far-end audio.
//!
//! Sender. [`encode`] injects the reference in 10 ms chunks with
//! `mello_voice_inject_capture`. A packet sink (`mello_voice_set_packet_sink`)
//! takes each frame right after Opus encode, as on the SFU send path in
//! mello-core. The sender path reads no clock, so its packets are a pure
//! function of the captured samples and of the encoder settings over time.
//! The harness therefore encodes the longest reference once and every
//! profile replays a prefix of that packet trace while the settings stay at
//! their start values. The sender DSP (about 5 ms per 20 ms frame) runs once
//! instead of once per profile.
//!
//! Feedback (stage 5, [`run_adaptive`] and rtcp.rs). The SFU's receiver
//! reports reach the sender's encoder adaptation, which changes the bitrate
//! and the expected loss. A run replays the shared trace and gives the
//! reports to a probe context with the same controller. At the first
//! changed setting it starts again with a live sender: a context that
//! encodes each chunk at its capture time, between the network and receiver
//! steps, with every report applied at its arrival time.
//!
//! Send timing ([`SendModel`]). A packet that leaves the encoder after chunk
//! `c` is available at sender time `10 c` ms. The SFU path of mello-core
//! sends it then, from the capture thread (spec 10 section 4.5):
//! [`SendModel::Direct`], the default. Before that path, `VoiceManager::tick`
//! drained `mello_voice_get_packet` every 20 ms on the command loop:
//! [`SendModel::Tick`] replays that with ticks at sender time `20 k + 10` ms,
//! for comparison (`--send-tick`). Sender time maps to receiver time by the
//! clock ratio of the profile (drift).
//!
//! Receiver clock. [`run_receiver`] drives libmello's playout clock with
//! `mello_test_set_clock_ms` and steps it 1 ms at a time. NetEQ times its
//! playout by the 10 ms pulls and stamps arrivals with that clock, so a run
//! is deterministic (equal input, equal output) and faster than real time.
//! Real-time pacing would make the 10 minute drift profile alone take 10
//! minutes and would depend on the scheduler of the machine. In each 1 ms
//! step:
//! 0. give the sender the receiver reports that arrive now,
//! 1. capture the due chunks (live sender) and send every packet whose send
//!    tick is due,
//! 2. deliver every packet the shim has due as a complete RTP packet
//!    (`mello_voice_feed_rtp`), as the SFU path does since stage 3,
//!    The SFU's receiver model sees each delivered packet, and sends a
//!    receiver report on its 1 s ticker,
//! 3. every 10 ms, pull 10 ms of mixed output the way the playback device
//!    thread does (`mello_voice_test_pull_output`).

use crate::profile::{CorpusSpec, Profile, SenderSpec};
use crate::rng::Rng;
use crate::rtcp::RtcpModel;
use crate::shim::{Shim, ShimCounters};
use crate::wav;
use std::collections::VecDeque;
use std::ffi::CString;
use std::path::Path;
use std::time::Instant;

pub const SAMPLE_RATE: usize = 48_000;
/// Capture chunk and playback pull size: 10 ms at 48 kHz.
pub const CHUNK: usize = 480;
/// The receiver clock starts here, not at 0: the jitter buffer treats a
/// zero arrival time as "no arrival yet".
const CLOCK_ORIGIN_MS: i64 = 1_000_000;
const PEER_ID: &str = "peer-1";

/// Select the device-free audio backend for every context created after
/// this call. Call before any other thread starts.
pub fn select_test_backend() {
    std::env::set_var("MELLO_AUDIO_BACKEND", "test");
}

/// One corpus clip.
pub struct Clip {
    pub name: String,
    pub samples: Vec<i16>,
}

/// Where one clip sits in the reference signal.
#[derive(Clone, Debug)]
pub struct ClipSpan {
    pub name: String,
    pub start: usize,
    pub len: usize,
}

/// Load the corpus clips (48 kHz mono 16-bit).
pub fn load_corpus(root: &Path, spec: &CorpusSpec) -> Result<Vec<Clip>, String> {
    let mut clips = Vec::new();
    for name in &spec.clips {
        let w = wav::read(&root.join(&spec.dir).join(name))?;
        if w.sample_rate as usize != SAMPLE_RATE {
            return Err(format!("{name}: need 48 kHz, got {}", w.sample_rate));
        }
        clips.push(Clip {
            name: name.clone(),
            samples: w.samples,
        });
    }
    Ok(clips)
}

/// The sender's capture signal for a run of `duration_s`: gap, clip, gap,
/// clip, ... in corpus order, looped. Low-level seeded white noise runs
/// under all of it, because a real microphone never delivers digital zero.
/// Only clips that fit completely are placed. A shorter run uses a prefix
/// of a longer reference: the noise and the clip positions are the same.
pub fn build_reference(
    clips: &[Clip],
    spec: &CorpusSpec,
    duration_s: f64,
) -> (Vec<i16>, Vec<ClipSpan>) {
    let total = reference_len(duration_s);
    let gap = spec.gap_ms as usize * SAMPLE_RATE / 1000;
    let mut signal = vec![0.0f64; total];
    let mut spans = Vec::new();
    let mut pos = 0usize;
    'fill: loop {
        for clip in clips {
            let start = pos + gap;
            if start + clip.samples.len() > total {
                break 'fill;
            }
            for (i, &s) in clip.samples.iter().enumerate() {
                signal[start + i] = f64::from(s);
            }
            spans.push(ClipSpan {
                name: clip.name.clone(),
                start,
                len: clip.samples.len(),
            });
            pos = start + clip.samples.len();
        }
        if clips.is_empty() {
            break;
        }
    }
    let noise_rms = 32768.0 * 10f64.powf(spec.noise_dbfs / 20.0);
    let mut rng = Rng::new(spec.noise_seed);
    let reference = signal
        .iter()
        .map(|&s| {
            (s + rng.gaussian() * noise_rms)
                .round()
                .clamp(-32768.0, 32767.0) as i16
        })
        .collect();
    (reference, spans)
}

/// Receiver state sampled during the run (NetEQ, spec 10 section 5.4).
#[derive(Clone, Debug)]
pub struct StatSample {
    pub t_ms: i64,
    /// NetEQ filtered buffer delay plus the output ring.
    pub pipeline_delay_ms: f32,
    /// NetEQ target delay.
    pub jitter_target_ms: f32,
    /// NetEQ packet and sync buffer plus the output ring.
    pub playout_buffer_ms: f32,
    /// NetEQ filtered buffer delay.
    pub filtered_delay_ms: f32,
    /// Minimum delay the talk-spurt controller holds for the pre-roll.
    pub spurt_hold_ms: f32,
}

/// The receiver's NetEQ buffer at one 10 ms device pull (burst recovery,
/// stage 3b; analysis::buffer_curve).
#[derive(Clone, Copy, Debug)]
pub struct BufferSample {
    /// Receiver time of the pull, ms.
    pub t_ms: i64,
    /// NetEQ packet and sync buffer plus the output ring after the pull, ms:
    /// the audio that waits to play, which accelerate removes.
    pub buffer_ms: f32,
    /// NetEQ's target delay after the pull, ms.
    pub target_ms: f32,
    /// The pull played decoded audio: NetEQ concealed less than the whole
    /// pull. False in a talk pause (expand, comfort noise, muted state) and
    /// in an outage.
    pub playing: bool,
}

/// One encoded frame as the packet sink received it.
#[derive(Clone, Debug)]
pub struct SentPacket {
    /// Chunks injected when the packet left the encoder.
    pub chunks: usize,
    /// The sender's 32-bit packet counter.
    pub sequence: u32,
    /// Media time in 48 kHz samples (the sender's RTP timestamp offset).
    pub timestamp: u32,
    pub payload: Vec<u8>,
}

/// The sender's output for one reference signal.
pub struct SenderTrace {
    pub packets: Vec<SentPacket>,
    pub packets_encoded: u32,
    pub wall_s: f64,
}

/// Everything one receiver run produced.
pub struct RunResult {
    pub reference: Vec<i16>,
    pub output: Vec<i16>,
    pub clips: Vec<ClipSpan>,
    /// Receiver seconds per sender second (1 / (1 + ppm * 1e-6)).
    pub clock_ratio: f64,
    pub shim: ShimCounters,
    pub wrap_sender_sample: Option<usize>,
    pub rx: mello_sys::MelloDebugStats,
    pub packets_fed: u64,
    pub stats: Vec<StatSample>,
    /// The NetEQ buffer at every 10 ms pull.
    pub buffer: Vec<BufferSample>,
    /// The send-leg feedback loop; None for a run without feedback.
    pub adapt: Option<Adaptation>,
    /// The packets of a live sender, in send order; None for a replay.
    pub live_trace: Option<Vec<SentPacket>>,
    pub wall_s: f64,
}

struct Context(*mut mello_sys::MelloContext);

impl Context {
    fn new() -> Result<Self, String> {
        // SAFETY: plain FFI constructor; null is checked.
        let p = unsafe { mello_sys::mello_init() };
        if p.is_null() {
            return Err("mello_init failed".into());
        }
        Ok(Self(p))
    }

    fn stats(&self) -> mello_sys::MelloDebugStats {
        // SAFETY: zeroed is a valid MelloDebugStats (plain C struct); the
        // context pointer is live for the lifetime of self.
        unsafe {
            let mut s: mello_sys::MelloDebugStats = std::mem::zeroed();
            mello_sys::mello_get_debug_stats(self.0, &mut s);
            s
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: the pointer came from mello_init and is destroyed once.
        unsafe { mello_sys::mello_destroy(self.0) };
    }
}

/// Restores the steady clock when the run ends, also on panic.
struct ClockGuard;

impl Drop for ClockGuard {
    fn drop(&mut self) {
        // SAFETY: process-wide setter, no pointers.
        unsafe { mello_sys::mello_test_set_clock_ms(-1) };
    }
}

fn set_clock(now_ms: i64) {
    // SAFETY: process-wide setter, no pointers.
    unsafe { mello_sys::mello_test_set_clock_ms(CLOCK_ORIGIN_MS + now_ms) };
}

/// When the sender puts an encoded packet on the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendModel {
    /// At encode time, from the capture thread (the SFU path since stage 2).
    Direct,
    /// On the 20 ms voice tick of the command loop (the SFU path before
    /// stage 2, and the P2P path).
    Tick,
}

impl SendModel {
    pub fn describe(self) -> &'static str {
        match self {
            SendModel::Direct => {
                "packets leave at encode time (sender time 10c ms), like the SFU packet sink on the capture thread"
            }
            SendModel::Tick => {
                "packets leave on a 20 ms tick at sender time 20k+10 ms, like VoiceManager::tick before stage 2"
            }
        }
    }
}

/// Send tick of the voice loop before stage 2: every 20 ms, drain all.
const SEND_TICK_MS: usize = 20;
/// Phase of the send tick against the capture frames (sender ms).
const SEND_TICK_PHASE_MS: usize = 10;

/// Packets the sink collects while the reference is injected.
struct SinkTrace {
    /// Chunks injected so far, including the one being injected now.
    chunks: usize,
    packets: Vec<SentPacket>,
}

/// Packet sink of the sender context: the Opus payload, the media time and
/// the sequence, as the SFU send path gets them. The shim builds the RTP
/// packet from them.
///
/// # Safety
/// `user_data` points to the `SinkTrace` of [`encode`], which outlives the
/// sink; `data` is valid for `size` bytes.
unsafe extern "C" fn collect_packet(
    user_data: *mut std::ffi::c_void,
    data: *const u8,
    size: i32,
    timestamp: u32,
    sequence: u32,
) {
    if user_data.is_null() || data.is_null() || size <= 0 {
        return;
    }
    let trace = &mut *(user_data as *mut SinkTrace);
    let payload = std::slice::from_raw_parts(data, size as usize);
    trace.packets.push(SentPacket {
        chunks: trace.chunks,
        sequence,
        timestamp,
        payload: payload.to_vec(),
    });
}

/// Encode `reference` with a sender context set up as `sender`. See the
/// module notes.
pub fn encode(reference: &[i16], sender: &SenderSpec) -> Result<SenderTrace, String> {
    let started = Instant::now();
    let tx = Context::new()?;
    // SAFETY: tx.0 is a live context.
    unsafe {
        mello_sys::mello_voice_set_push_to_talk(tx.0, sender.push_to_talk);
        mello_sys::mello_voice_set_noise_suppression(tx.0, sender.noise_suppression);
    }
    // SAFETY: tx.0 is a live context.
    let r = unsafe { mello_sys::mello_voice_start_capture_inject(tx.0) };
    if r != mello_sys::MelloResult_MELLO_OK {
        return Err(format!("start_capture_inject failed: {r}"));
    }
    let mut trace = Box::new(SinkTrace {
        chunks: 0,
        packets: Vec::new(),
    });
    let trace_ptr: *mut SinkTrace = &mut *trace;
    // SAFETY: tx.0 is a live context. The sink runs only inside
    // mello_voice_inject_capture on this thread, and it is cleared below
    // before `trace` is read or dropped.
    let r = unsafe {
        mello_sys::mello_voice_set_packet_sink(
            tx.0,
            Some(collect_packet),
            trace_ptr as *mut std::ffi::c_void,
        )
    };
    if r != mello_sys::MelloResult_MELLO_OK {
        return Err(format!("set_packet_sink failed: {r}"));
    }
    for (i, chunk) in reference.chunks_exact(CHUNK).enumerate() {
        // SAFETY: the sink touches the trace only inside the inject call.
        unsafe { (*trace_ptr).chunks = i + 1 };
        // SAFETY: chunk outlives the call; libmello copies the samples.
        unsafe { mello_sys::mello_voice_inject_capture(tx.0, chunk.as_ptr(), CHUNK as i32) };
    }
    // SAFETY: tx.0 is a live context; the clear waits for a running sink.
    unsafe {
        mello_sys::mello_voice_set_packet_sink(tx.0, None, std::ptr::null_mut());
    }
    let packets_encoded = tx.stats().packets_encoded;
    // SAFETY: tx.0 is a live context.
    unsafe { mello_sys::mello_voice_stop_capture_inject(tx.0) };
    let packets = std::mem::take(&mut trace.packets);
    Ok(SenderTrace {
        packets,
        packets_encoded,
        wall_s: started.elapsed().as_secs_f64(),
    })
}

/// Receiver time (ms, rounded up) at which the sender sends a packet that
/// left the encoder after `chunks` chunks.
fn send_time_ms(chunks: usize, clock_ratio: f64, model: SendModel) -> i64 {
    let ready = chunks * 10;
    let sent = match model {
        SendModel::Direct => ready,
        SendModel::Tick if ready <= SEND_TICK_PHASE_MS => SEND_TICK_PHASE_MS,
        SendModel::Tick => {
            (ready - SEND_TICK_PHASE_MS).div_ceil(SEND_TICK_MS) * SEND_TICK_MS + SEND_TICK_PHASE_MS
        }
    };
    (sent as f64 * clock_ratio).ceil() as i64
}

/// The encoder setting of the sender, sampled during a run (spec 10
/// section 4.6).
#[derive(Clone, Copy, Debug)]
pub struct AdaptSample {
    pub t_ms: i64,
    pub bitrate_kbps: i32,
    pub loss_perc: i32,
}

/// One receiver report that reached the sender, and the setting after it.
#[derive(Clone, Copy, Debug)]
pub struct ReportSample {
    /// Receiver time at which the SFU sent it.
    pub sent_ms: i64,
    /// Receiver time at which the sender took it.
    pub arrive_ms: i64,
    pub feedback: mello_sys::MelloSendFeedback,
    pub bitrate_kbps: i32,
    pub loss_perc: i32,
}

/// The send-leg feedback loop of one run (rtcp.rs).
#[derive(Clone, Debug, Default)]
pub struct Adaptation {
    /// The run encoded the reference again with the feedback applied: the
    /// controller changed a setting of the shared trace's encoder.
    pub live_sender: bool,
    /// Receiver time of the first changed setting in the replay of the
    /// shared trace (the reason for the live run).
    pub diverged_ms: Option<i64>,
    /// Receiver time of the first sent packet.
    pub first_send_ms: Option<i64>,
    /// Receiver time of the first step below the top rung.
    pub first_drop_ms: Option<i64>,
    pub top_kbps: i32,
    pub series: Vec<AdaptSample>,
    pub reports: Vec<ReportSample>,
    pub rung_changes: u32,
}

/// Where the packets of a run come from.
enum Feed<'a> {
    /// A recorded trace: the shared one, or the trace of a live run.
    Trace {
        packets: Vec<&'a SentPacket>,
        next: usize,
    },
    /// A sender context that encodes the reference during the run, so the
    /// receiver reports reach its encoder.
    Live(Box<LiveSender<'a>>),
}

/// A sender context that encodes during the run. See [`encode`] for the
/// sink; here the chunks go in at their capture time, interleaved with the
/// network and the receiver.
struct LiveSender<'a> {
    tx: Context,
    reference: &'a [i16],
    /// Chunks injected so far.
    injected: usize,
    trace: Box<SinkTrace>,
    /// Packets taken from the sink that wait for their send time.
    pending: VecDeque<SentPacket>,
    /// Every packet, for the lossless twin run.
    sent: Vec<SentPacket>,
}

impl<'a> LiveSender<'a> {
    fn new(reference: &'a [i16], sender: &SenderSpec) -> Result<Self, String> {
        let tx = Context::new()?;
        // SAFETY: tx.0 is a live context.
        unsafe {
            mello_sys::mello_voice_set_push_to_talk(tx.0, sender.push_to_talk);
            mello_sys::mello_voice_set_noise_suppression(tx.0, sender.noise_suppression);
        }
        // SAFETY: tx.0 is a live context.
        let r = unsafe { mello_sys::mello_voice_start_capture_inject(tx.0) };
        if r != mello_sys::MelloResult_MELLO_OK {
            return Err(format!("start_capture_inject failed: {r}"));
        }
        let mut trace = Box::new(SinkTrace {
            chunks: 0,
            packets: Vec::new(),
        });
        let trace_ptr: *mut SinkTrace = &mut *trace;
        // SAFETY: tx.0 is a live context. The sink runs only inside
        // mello_voice_inject_capture on this thread; Drop clears it before
        // the box goes. The box does not move its contents.
        let r = unsafe {
            mello_sys::mello_voice_set_packet_sink(
                tx.0,
                Some(collect_packet),
                trace_ptr as *mut std::ffi::c_void,
            )
        };
        if r != mello_sys::MelloResult_MELLO_OK {
            return Err(format!("set_packet_sink failed: {r}"));
        }
        Ok(Self {
            tx,
            reference,
            injected: 0,
            trace,
            pending: VecDeque::new(),
            sent: Vec::new(),
        })
    }

    /// Inject every chunk captured at or before receiver time `now`.
    fn capture_until(&mut self, now: i64, clock_ratio: f64) {
        let n_chunks = self.reference.len() / CHUNK;
        while self.injected < n_chunks
            && send_time_ms(self.injected + 1, clock_ratio, SendModel::Direct) <= now
        {
            let i = self.injected;
            self.trace.chunks = i + 1;
            let chunk = &self.reference[i * CHUNK..(i + 1) * CHUNK];
            // SAFETY: chunk outlives the call; libmello copies the samples.
            // The sink writes into self.trace on this thread.
            unsafe {
                mello_sys::mello_voice_inject_capture(self.tx.0, chunk.as_ptr(), CHUNK as i32)
            };
            self.injected += 1;
            self.pending.extend(self.trace.packets.drain(..));
        }
    }
}

impl Drop for LiveSender<'_> {
    fn drop(&mut self) {
        // SAFETY: tx.0 is live; the clear waits for a running sink, after
        // which no call touches self.trace.
        unsafe {
            mello_sys::mello_voice_set_packet_sink(self.tx.0, None, std::ptr::null_mut());
            mello_sys::mello_voice_stop_capture_inject(self.tx.0);
        }
    }
}

/// How a run ended.
enum Drive {
    Done(Box<RunResult>),
    /// Replay with feedback: the controller changed a setting at this
    /// receiver time, so the shared trace is no longer the sender's output.
    Diverged(i64),
}

fn rtp_seq_ts(p: &[u8]) -> Option<(u16, u32)> {
    (p.len() >= 12).then(|| {
        (
            u16::from_be_bytes([p[2], p[3]]),
            u32::from_be_bytes([p[4], p[5], p[6], p[7]]),
        )
    })
}

/// OPUS_SET_PACKET_LOSS_PERC before any report (libmello kLossPercFloor).
const LOSS_PERC_FLOOR: i32 = 5;

/// Which context gets the receiver reports.
#[derive(Clone, Copy)]
enum FeedbackTo {
    /// No feedback loop.
    None,
    /// A probe context: the shared trace is replayed, and the run stops at
    /// the first changed setting.
    Probe,
    /// The live sender's own context.
    LiveSender,
}

/// The core loop: send, network, receiver, and the receiver reports back to
/// the sender. See the module notes.
fn drive(
    profile: &Profile,
    reference: &[i16],
    clips: &[ClipSpan],
    mut feed: Feed<'_>,
    tail_ms: u32,
    model: SendModel,
    feedback: FeedbackTo,
) -> Result<Drive, String> {
    let started = Instant::now();
    let _clock = ClockGuard;
    set_clock(0);
    let rx = Context::new()?;
    let peer = CString::new(PEER_ID).expect("static peer id");
    let probe = match feedback {
        FeedbackTo::Probe => Some(Context::new()?),
        _ => None,
    };
    // The context that the reports go to, borrowed for the whole run.
    let feedback_ctx = |feed: &Feed<'_>| -> Option<*mut mello_sys::MelloContext> {
        match (feedback, feed) {
            (FeedbackTo::Probe, _) => probe.as_ref().map(|p| p.0),
            (FeedbackTo::LiveSender, Feed::Live(live)) => Some(live.tx.0),
            _ => None,
        }
    };

    let clock_ratio = 1.0 / (1.0 + profile.drift_ppm * 1e-6);
    let end_ms = (reference.len() as f64 / 48.0 * clock_ratio).ceil() as i64 + i64::from(tail_ms);

    let mut shim = Shim::new(profile);
    let fb_ctx = feedback_ctx(&feed);
    let mut rtcp = fb_ctx.map(|_| RtcpModel::new(profile.seed));
    let mut adapt = fb_ctx.map(|ctx| Adaptation {
        live_sender: matches!(feedback, FeedbackTo::LiveSender),
        top_kbps: context_stats(ctx).tx_max_bitrate_kbps,
        ..Default::default()
    });
    let mut output: Vec<i16> = Vec::with_capacity((end_ms as usize / 10 + 1) * CHUNK);
    let mut pull = vec![0i16; CHUNK];
    let mut packets_fed = 0u64;
    let mut stats = Vec::new();
    let mut buffer = Vec::with_capacity(end_ms as usize / 10 + 1);
    let mut concealed_before = 0u64;

    for now in 0..=end_ms {
        set_clock(now);

        // 0. Receiver reports that reach the sender now.
        if let (Some(rtcp), Some(ctx), Some(adapt)) = (rtcp.as_mut(), fb_ctx, adapt.as_mut()) {
            for r in rtcp.arrived(now) {
                // SAFETY: ctx is live for the run; the feedback is a plain
                // struct that libmello copies.
                unsafe { mello_sys::mello_voice_report_send_feedback(ctx, &r.feedback) };
                let st = context_stats(ctx);
                adapt.reports.push(ReportSample {
                    sent_ms: r.sent_ms,
                    arrive_ms: now,
                    feedback: r.feedback,
                    bitrate_kbps: st.tx_bitrate_kbps,
                    loss_perc: st.tx_packet_loss_perc,
                });
                if st.tx_bitrate_kbps < adapt.top_kbps && adapt.first_drop_ms.is_none() {
                    adapt.first_drop_ms = Some(now);
                }
                if matches!(feedback, FeedbackTo::Probe)
                    && (st.tx_bitrate_kbps != adapt.top_kbps
                        || st.tx_packet_loss_perc != LOSS_PERC_FLOOR)
                {
                    return Ok(Drive::Diverged(now));
                }
            }
        }

        // 1. Sender.
        let mut to_send: Vec<SentPacket> = Vec::new();
        match &mut feed {
            Feed::Trace { packets, next } => {
                while *next < packets.len()
                    && send_time_ms(packets[*next].chunks, clock_ratio, model) <= now
                {
                    to_send.push(packets[*next].clone());
                    *next += 1;
                }
            }
            Feed::Live(live) => {
                live.capture_until(now, clock_ratio);
                while live
                    .pending
                    .front()
                    .is_some_and(|p| send_time_ms(p.chunks, clock_ratio, model) <= now)
                {
                    if let Some(p) = live.pending.pop_front() {
                        to_send.push(p);
                    }
                }
            }
        }
        for pkt in to_send {
            if !pkt.payload.is_empty() {
                if let (Some(rtcp), Some(adapt)) = (rtcp.as_mut(), adapt.as_mut()) {
                    let seq = shim.rtp_sequence(pkt.sequence);
                    rtcp.on_rtp_sent(now, seq, &mut shim);
                    adapt.first_send_ms.get_or_insert(now);
                }
            }
            shim.send(&pkt, now, pkt.chunks * CHUNK);
            if let Feed::Live(live) = &mut feed {
                live.sent.push(pkt);
            }
        }

        // 2. Network. Receive time -1: libmello stamps it with the clock.
        for p in shim.due(now) {
            if let (Some(rtcp), Some((seq, ts))) = (rtcp.as_mut(), rtp_seq_ts(&p)) {
                rtcp.on_rtp_received(now, seq, ts);
            }
            // SAFETY: p and peer outlive the call; libmello copies the data.
            let r = unsafe {
                mello_sys::mello_voice_feed_rtp(rx.0, peer.as_ptr(), p.as_ptr(), p.len() as i32, -1)
            };
            if r != mello_sys::MelloResult_MELLO_OK {
                return Err(format!("feed_rtp failed: {r}"));
            }
            packets_fed += 1;
        }
        if let Some(rtcp) = rtcp.as_mut() {
            rtcp.step(now, &shim);
        }

        // 3. Receiver device period.
        if now % 10 == 0 {
            // SAFETY: pull is a writable buffer of CHUNK samples.
            let got = unsafe {
                mello_sys::mello_voice_test_pull_output(rx.0, pull.as_mut_ptr(), CHUNK as i32)
            };
            if got < 0 {
                return Err("mello_voice_test_pull_output: test backend not active (MELLO_AUDIO_BACKEND=test)".into());
            }
            output.extend_from_slice(&pull);
            let s = rx.stats();
            let concealed = s.rx_concealed_samples;
            buffer.push(BufferSample {
                t_ms: now,
                buffer_ms: s.rx_buffer_ms + s.rx_output_ring_ms,
                target_ms: s.rx_target_delay_ms,
                // A merge can correct the count down; that pull played audio.
                playing: concealed < concealed_before + CHUNK as u64,
            });
            concealed_before = concealed;
        }
        if now % 100 == 0 {
            let s = rx.stats();
            stats.push(StatSample {
                t_ms: now,
                pipeline_delay_ms: s.pipeline_delay_ms,
                jitter_target_ms: s.rx_target_delay_ms,
                playout_buffer_ms: s.rx_buffer_ms + s.rx_output_ring_ms,
                filtered_delay_ms: s.rx_filtered_delay_ms,
                spurt_hold_ms: s.rx_spurt_hold_ms,
            });
            if let (Some(ctx), Some(adapt)) = (fb_ctx, adapt.as_mut()) {
                let st = context_stats(ctx);
                adapt.series.push(AdaptSample {
                    t_ms: now,
                    bitrate_kbps: st.tx_bitrate_kbps,
                    loss_perc: st.tx_packet_loss_perc,
                });
            }
        }
    }

    if let (Some(ctx), Some(adapt)) = (fb_ctx, adapt.as_mut()) {
        adapt.rung_changes = context_stats(ctx).tx_rung_changes;
    }
    let live_trace = match feed {
        Feed::Live(mut live) => Some(std::mem::take(&mut live.sent)),
        Feed::Trace { .. } => None,
    };
    let rx_stats = rx.stats();
    drop(rx);
    Ok(Drive::Done(Box::new(RunResult {
        reference: reference.to_vec(),
        output,
        clips: clips
            .iter()
            .filter(|c| c.start + c.len <= reference.len())
            .cloned()
            .collect(),
        clock_ratio,
        wrap_sender_sample: shim.wrap_sender_sample,
        shim: shim.counters,
        rx: rx_stats,
        packets_fed,
        stats,
        buffer,
        adapt,
        live_trace,
        wall_s: started.elapsed().as_secs_f64(),
    })))
}

fn context_stats(ctx: *mut mello_sys::MelloContext) -> mello_sys::MelloDebugStats {
    // SAFETY: zeroed is a valid MelloDebugStats (plain C struct); the
    // caller keeps the context live.
    unsafe {
        let mut s: mello_sys::MelloDebugStats = std::mem::zeroed();
        mello_sys::mello_get_debug_stats(ctx, &mut s);
        s
    }
}

fn trace_feed<'a>(reference: &[i16], trace: &'a SenderTrace) -> Feed<'a> {
    let n_chunks = reference.len() / CHUNK;
    Feed::Trace {
        packets: trace
            .packets
            .iter()
            .filter(|p| p.chunks <= n_chunks)
            .collect(),
        next: 0,
    }
}

/// Replay the sender trace for `reference` (a prefix of the encoded
/// reference) through the shim of `profile` into a receiver context. No
/// feedback reaches the sender.
pub fn run_receiver(
    profile: &Profile,
    reference: &[i16],
    clips: &[ClipSpan],
    trace: &SenderTrace,
    tail_ms: u32,
    model: SendModel,
) -> Result<RunResult, String> {
    match drive(
        profile,
        reference,
        clips,
        trace_feed(reference, trace),
        tail_ms,
        model,
        FeedbackTo::None,
    )? {
        Drive::Done(r) => Ok(*r),
        Drive::Diverged(_) => Err("a run without feedback cannot diverge".into()),
    }
}

/// Run `profile` with the send-leg feedback loop (rtcp.rs): the receiver
/// reports of the SFU reach the sender's encoder adaptation.
///
/// The sender's packets are a pure function of the captured samples and the
/// encoder settings over time, and the packet count and send times do not
/// depend on the settings (the speech gate decides them before encode). So
/// the run first replays the shared trace and gives the reports to a probe
/// context, which runs the same controller. While the controller keeps the
/// start setting (the top rung, loss perc 5), the shared trace is exactly
/// what the sender sends. At the first changed setting, the run starts
/// again with a live sender that encodes the reference with the reports
/// applied at their arrival time. The result carries the live sender's
/// packets ([`RunResult::live_trace`]) for the lossless twin run.
pub fn run_adaptive(
    profile: &Profile,
    reference: &[i16],
    clips: &[ClipSpan],
    shared: &SenderTrace,
    tail_ms: u32,
    model: SendModel,
) -> Result<RunResult, String> {
    let diverged = match drive(
        profile,
        reference,
        clips,
        trace_feed(reference, shared),
        tail_ms,
        model,
        FeedbackTo::Probe,
    )? {
        Drive::Done(r) => return Ok(*r),
        Drive::Diverged(t) => t,
    };
    let live = Box::new(LiveSender::new(reference, &profile.sender)?);
    match drive(
        profile,
        reference,
        clips,
        Feed::Live(live),
        tail_ms,
        model,
        FeedbackTo::LiveSender,
    )? {
        Drive::Done(mut r) => {
            if let Some(a) = r.adapt.as_mut() {
                a.diverged_ms = Some(diverged);
            }
            Ok(*r)
        }
        Drive::Diverged(_) => Err("a live run does not stop early".into()),
    }
}

/// Samples of reference for a profile duration (whole 10 ms chunks).
pub fn reference_len(duration_s: f64) -> usize {
    let total = (duration_s * SAMPLE_RATE as f64) as usize;
    total - total % CHUNK
}
