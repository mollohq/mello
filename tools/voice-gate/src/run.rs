//! Drive the real libmello voice path.
//!
//! Two contexts, both on the device-free test backend
//! (`MELLO_AUDIO_BACKEND=test`): a sender that captures and encodes, and a
//! receiver that buffers, decodes, conceals and mixes. Two contexts, not
//! one, so the receiver's mixed output never reaches the sender's echo
//! canceller as far-end audio.
//!
//! Sender. [`encode`] injects the reference in 10 ms chunks with
//! `mello_voice_inject_capture` and drains `mello_voice_get_packet` after
//! each chunk. The sender path reads no clock and never sees the receiver,
//! so its packets are a pure function of the captured samples. The harness
//! therefore encodes the longest reference once and every profile replays a
//! prefix of that packet trace. Same packets as a live interleaved run, and
//! the sender DSP (about 5 ms per 20 ms frame) runs once instead of once per
//! profile.
//!
//! Send timing. A packet that leaves the encoder after chunk `c` is
//! available at sender time `10 c` ms. Today `VoiceManager::tick` in
//! mello-core drains `mello_voice_get_packet` every 20 ms on the command
//! loop and sends what it finds. The replay does the same, with ticks at
//! sender time `20 k + 10` ms. Sender time maps to receiver time by the
//! clock ratio of the profile (drift).
//!
//! Receiver clock. [`run_receiver`] drives libmello's playout clock with
//! `mello_test_set_clock_ms` and steps it 1 ms at a time. The jitter buffer
//! times its holds against that clock, so a run is deterministic (equal
//! input, equal output) and faster than real time. Real-time pacing would
//! make the 10 minute drift profile alone take 10 minutes and would depend
//! on the scheduler of the machine. In each 1 ms step:
//! 1. send every packet whose send tick is due,
//! 2. deliver every packet the shim has due (`mello_voice_feed_packet`),
//! 3. every 10 ms, pull 10 ms of mixed output the way the playback device
//!    thread does (`mello_voice_test_pull_output`).

use crate::profile::{CorpusSpec, Profile, SenderSpec};
use crate::rng::Rng;
use crate::shim::{Shim, ShimCounters};
use crate::wav;
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

/// Receiver state sampled during the run.
#[derive(Clone, Debug)]
pub struct StatSample {
    pub t_ms: i64,
    pub pipeline_delay_ms: f32,
    pub jitter_target_ms: f32,
    pub playout_buffer_ms: f32,
    pub jitter_buffered: i32,
}

/// The sender's output for one reference signal.
pub struct SenderTrace {
    /// (chunks injected when the packet left the encoder, packet bytes).
    pub packets: Vec<(usize, Vec<u8>)>,
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

/// Send tick of today's mello-core voice loop: every 20 ms, drain all.
const SEND_TICK_MS: usize = 20;
/// Phase of the send tick against the capture frames (sender ms).
const SEND_TICK_PHASE_MS: usize = 10;

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
    let mut packets = Vec::new();
    let mut pkt = vec![0u8; 4000];
    for (i, chunk) in reference.chunks_exact(CHUNK).enumerate() {
        // SAFETY: chunk outlives the call; libmello copies the samples.
        unsafe { mello_sys::mello_voice_inject_capture(tx.0, chunk.as_ptr(), CHUNK as i32) };
        loop {
            // SAFETY: pkt is a writable buffer of the given length.
            let n = unsafe {
                mello_sys::mello_voice_get_packet(tx.0, pkt.as_mut_ptr(), pkt.len() as i32)
            };
            if n <= 0 {
                break;
            }
            packets.push((i + 1, pkt[..n as usize].to_vec()));
        }
    }
    let packets_encoded = tx.stats().packets_encoded;
    // SAFETY: tx.0 is a live context.
    unsafe { mello_sys::mello_voice_stop_capture_inject(tx.0) };
    Ok(SenderTrace {
        packets,
        packets_encoded,
        wall_s: started.elapsed().as_secs_f64(),
    })
}

/// Receiver time (ms, rounded up) at which the send tick sends a packet
/// that left the encoder after `chunks` chunks.
fn send_time_ms(chunks: usize, clock_ratio: f64) -> i64 {
    let ready = chunks * 10;
    let tick = if ready <= SEND_TICK_PHASE_MS {
        SEND_TICK_PHASE_MS
    } else {
        (ready - SEND_TICK_PHASE_MS).div_ceil(SEND_TICK_MS) * SEND_TICK_MS + SEND_TICK_PHASE_MS
    };
    (tick as f64 * clock_ratio).ceil() as i64
}

/// Replay the sender trace for `reference` (a prefix of the encoded
/// reference) through the shim of `profile` into a receiver context.
pub fn run_receiver(
    profile: &Profile,
    reference: &[i16],
    clips: &[ClipSpan],
    trace: &SenderTrace,
    tail_ms: u32,
) -> Result<RunResult, String> {
    let started = Instant::now();
    let _clock = ClockGuard;
    set_clock(0);
    let rx = Context::new()?;
    let peer = CString::new(PEER_ID).expect("static peer id");

    let clock_ratio = 1.0 / (1.0 + profile.drift_ppm * 1e-6);
    let n_chunks = reference.len() / CHUNK;
    let end_ms = (reference.len() as f64 / 48.0 * clock_ratio).ceil() as i64 + i64::from(tail_ms);
    let packets: Vec<&(usize, Vec<u8>)> = trace
        .packets
        .iter()
        .filter(|(c, _)| *c <= n_chunks)
        .collect();

    let mut shim = Shim::new(profile);
    let mut output: Vec<i16> = Vec::with_capacity((end_ms as usize / 10 + 1) * CHUNK);
    let mut pull = vec![0i16; CHUNK];
    let mut next = 0usize;
    let mut packets_fed = 0u64;
    let mut stats = Vec::new();

    for now in 0..=end_ms {
        set_clock(now);

        // 1. Sender send tick.
        while next < packets.len() && send_time_ms(packets[next].0, clock_ratio) <= now {
            let (chunks, bytes) = packets[next];
            shim.send(bytes, now, chunks * CHUNK);
            next += 1;
        }

        // 2. Network.
        for p in shim.due(now) {
            // SAFETY: p and peer outlive the call; libmello copies the data.
            let r = unsafe {
                mello_sys::mello_voice_feed_packet(rx.0, peer.as_ptr(), p.as_ptr(), p.len() as i32)
            };
            if r != mello_sys::MelloResult_MELLO_OK {
                return Err(format!("feed_packet failed: {r}"));
            }
            packets_fed += 1;
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
        }
        if now % 100 == 0 {
            let s = rx.stats();
            stats.push(StatSample {
                t_ms: now,
                pipeline_delay_ms: s.pipeline_delay_ms,
                jitter_target_ms: s.rx_jitter_target_delay_ms,
                playout_buffer_ms: s.rx_playout_buffer_ms,
                jitter_buffered: s.rx_jitter_buffered_packets,
            });
        }
    }

    let rx_stats = rx.stats();
    drop(rx);
    Ok(RunResult {
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
        wall_s: started.elapsed().as_secs_f64(),
    })
}

/// Samples of reference for a profile duration (whole 10 ms chunks).
pub fn reference_len(duration_s: f64) -> usize {
    let total = (duration_s * SAMPLE_RATE as f64) as usize;
    total - total % CHUNK
}
