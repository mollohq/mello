//! Game audio of the reference clip and its scores (streaming-quality stage
//! 7, `benchmarks/baselines/stream/audio.json`).
//!
//! The track. One clip loop long, the same on both channels:
//!
//! | Part | What | Why |
//! |---|---|---|
//! | Speech | voice gate corpus clips, low-passed at 8 kHz | PESQ wideband (16 kHz) scores it |
//! | Markers | a 20 ms tone burst every 250 ms of clip time, at one of 8 frequencies from 10 to 15.25 kHz (marker k uses frequency k mod 8) | the audio twin of the burned-in video code: marker k is captured with video frame k x 15 |
//! | Pilot | a continuous 17.25 kHz tone | continuity: a gap in it is a gap in the audio |
//!
//! Markers and pilot are above the 8 kHz that PESQ wideband reads, so the
//! speech score does not see them.
//!
//! The scores. The viewer's output is pulled 10 ms at a time on its own
//! clock, with each pull's time.
//!
//! | Metric | Definition |
//! |---|---|
//! | A/V offset of marker k | the time its burst onset is heard minus the present time of video frame k x 15 (positive: the audio is late) |
//! | `av_within_40ms_pct` | markers heard with an offset within +-40 ms, in % of the markers whose video frame the viewer showed (a marker not heard counts as outside) |
//! | audio gap | the pilot more than 12 dB under its median for 20 ms or more, after the first audio |
//! | `audio_mos` | PESQ wideband of the speech, mean over 4 s segments, aligned by the markers |

use crate::metrics::{Present, SourceClock};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SAMPLE_RATE: usize = 48_000;
/// Samples per pull of the viewer's output (10 ms).
pub const PULL: usize = 480;

/// One gate of `audio.json`.
#[derive(Clone, Debug)]
pub struct AudioGate {
    pub profile: String,
    pub metric: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub note: String,
}

/// `audio.json`.
#[derive(Clone, Debug)]
pub struct AudioSpec {
    pub version: u64,
    pub speech: Vec<String>,
    pub marker_period_ms: f64,
    pub marker_ms: f64,
    pub marker_freqs_hz: Vec<f64>,
    pub marker_dbfs: f64,
    pub ramp_ms: f64,
    pub pilot_hz: f64,
    pub pilot_dbfs: f64,
    pub speech_cutoff_hz: f64,
    pub gap_min_ms: f64,
    pub gap_drop_db: f64,
    pub av_window_ms: f64,
    pub segment_s: f64,
    /// The voice gate's MOS on its `mobile` profile, the reference of the
    /// game-audio MOS gate.
    pub voice_mobile_mos: f64,
    pub voice_mobile_mos_source: String,
    pub gates: Vec<AudioGate>,
    /// FNV-1a 64 of the canonical file (descriptions and `_` keys out).
    pub fingerprint: String,
}

fn num(v: &Value, key: &str) -> Result<f64, String> {
    v.get(key)
        .and_then(Value::as_f64)
        .ok_or(format!("audio.json: missing number '{key}'"))
}

pub fn load(path: &Path) -> Result<AudioSpec, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let root: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    let m = root.get("markers").ok_or("audio.json: missing 'markers'")?;
    let p = root.get("pilot").ok_or("audio.json: missing 'pilot'")?;
    let a = root
        .get("analysis")
        .ok_or("audio.json: missing 'analysis'")?;
    let v = root
        .get("voice_mobile_mos")
        .ok_or("audio.json: missing 'voice_mobile_mos'")?;
    let voice_mobile_mos = num(v, "mos")?;
    let mut gates = Vec::new();
    for (profile, list) in root
        .get("gates")
        .and_then(Value::as_object)
        .ok_or("audio.json: missing 'gates'")?
    {
        for (metric, g) in list.as_object().ok_or("audio.json: gates per profile")? {
            // "below_voice_mobile_mos": at least the voice gate's mobile
            // MOS minus this.
            let min = match g.get("below_voice_mobile_mos").and_then(Value::as_f64) {
                Some(d) => Some(voice_mobile_mos - d),
                None => g.get("min").and_then(Value::as_f64),
            };
            gates.push(AudioGate {
                profile: profile.clone(),
                metric: metric.clone(),
                min,
                max: g.get("max").and_then(Value::as_f64),
                note: g
                    .get("note")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            });
        }
    }
    let freqs: Vec<f64> = m
        .get("freqs_hz")
        .and_then(Value::as_array)
        .ok_or("audio.json: missing markers.freqs_hz")?
        .iter()
        .filter_map(Value::as_f64)
        .collect();
    if freqs.is_empty() {
        return Err("audio.json: markers.freqs_hz is empty".into());
    }
    Ok(AudioSpec {
        version: root
            .get("audio_version")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        speech: root
            .get("speech")
            .and_then(Value::as_array)
            .ok_or("audio.json: missing 'speech'")?
            .iter()
            .filter_map(|s| s.as_str().map(str::to_owned))
            .collect(),
        marker_period_ms: num(m, "period_ms")?,
        marker_ms: num(m, "burst_ms")?,
        marker_freqs_hz: freqs,
        marker_dbfs: num(m, "level_dbfs")?,
        ramp_ms: num(m, "ramp_ms")?,
        pilot_hz: num(p, "freq_hz")?,
        pilot_dbfs: num(p, "level_dbfs")?,
        speech_cutoff_hz: num(a, "speech_cutoff_hz")?,
        gap_min_ms: num(a, "gap_min_ms")?,
        gap_drop_db: num(a, "gap_drop_db")?,
        av_window_ms: num(a, "av_window_ms")?,
        segment_s: num(a, "pesq_segment_s")?,
        voice_mobile_mos,
        voice_mobile_mos_source: v
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        gates,
        fingerprint: crate::profile::fingerprint(&root),
    })
}

/// The reference track of one clip loop.
pub struct ReferenceAudio {
    /// What the host plays: interleaved stereo, L = R.
    pub stereo: Vec<i16>,
    /// The speech alone (mono, low-passed), for PESQ.
    pub speech: Vec<i16>,
    /// Samples per channel in one loop.
    pub loop_samples: usize,
}

fn dbfs_amplitude(db: f64) -> f64 {
    32767.0 * 10f64.powf(db / 20.0)
}

/// Sample of marker `k`'s onset within the loop.
pub fn marker_sample(spec: &AudioSpec, k: u64) -> u64 {
    (k as f64 * spec.marker_period_ms * SAMPLE_RATE as f64 / 1000.0).round() as u64
}

/// The track for a loop of `loop_samples`, from the corpus WAVs in
/// `corpus_dir`. Each speech clip fills an equal share of the loop (cut,
/// or silence after it).
pub fn build(
    spec: &AudioSpec,
    corpus_dir: &Path,
    loop_samples: usize,
) -> Result<ReferenceAudio, String> {
    let mut speech = vec![0i16; loop_samples];
    if !spec.speech.is_empty() {
        let share = loop_samples / spec.speech.len();
        let taps = voice_gate::dsp::lowpass_taps(spec.speech_cutoff_hz / SAMPLE_RATE as f64, 127);
        for (i, name) in spec.speech.iter().enumerate() {
            let wav = voice_gate::wav::read(&corpus_dir.join(name))?;
            if wav.sample_rate as usize != SAMPLE_RATE {
                return Err(format!("{name}: {} Hz, expected 48 kHz", wav.sample_rate));
            }
            // Low-pass in place (decimate by 1 keeps every sample).
            let filtered = voice_gate::dsp::decimate(&wav.samples, 1, &taps);
            let n = filtered.len().min(share);
            for (j, v) in filtered.iter().take(n).enumerate() {
                speech[i * share + j] = v.round().clamp(-32768.0, 32767.0) as i16;
            }
        }
    }
    let marker_amp = dbfs_amplitude(spec.marker_dbfs);
    let pilot_amp = dbfs_amplitude(spec.pilot_dbfs);
    let burst = (spec.marker_ms * SAMPLE_RATE as f64 / 1000.0) as usize;
    let ramp = ((spec.ramp_ms * SAMPLE_RATE as f64 / 1000.0) as usize).max(1);
    let mut mix: Vec<f64> = speech.iter().map(|&s| f64::from(s)).collect();
    let tau = 2.0 * std::f64::consts::PI;
    for (n, v) in mix.iter_mut().enumerate() {
        *v += pilot_amp * (tau * spec.pilot_hz * n as f64 / SAMPLE_RATE as f64).sin();
    }
    let markers =
        (loop_samples as f64 / (spec.marker_period_ms * SAMPLE_RATE as f64 / 1000.0)) as u64;
    let nf = spec.marker_freqs_hz.len() as u64;
    for k in 0..markers {
        let f = spec.marker_freqs_hz[(k % nf) as usize];
        let start = marker_sample(spec, k) as usize;
        for j in 0..burst {
            let n = start + j;
            if n >= loop_samples {
                break;
            }
            let env = if j < ramp {
                0.5 - 0.5 * (std::f64::consts::PI * j as f64 / ramp as f64).cos()
            } else if j + ramp > burst {
                0.5 - 0.5 * (std::f64::consts::PI * (burst - j) as f64 / ramp as f64).cos()
            } else {
                1.0
            };
            mix[n] += env * marker_amp * (tau * f * j as f64 / SAMPLE_RATE as f64).sin();
        }
    }
    let mut stereo = Vec::with_capacity(loop_samples * 2);
    for v in &mix {
        let s = v.round().clamp(-32768.0, 32767.0) as i16;
        stereo.push(s);
        stereo.push(s);
    }
    Ok(ReferenceAudio {
        stereo,
        speech,
        loop_samples,
    })
}

/// Write the stereo track as raw s16le (`mello_test_set_reference_audio`).
pub fn write_raw(path: &Path, reference: &ReferenceAudio) -> Result<(), String> {
    let mut bytes = Vec::with_capacity(reference.stereo.len() * 2);
    for s in &reference.stereo {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Amplitude of a sinusoid at `freq` in `x` (Goertzel over the window).
fn tone_amplitude(x: &[i16], freq: f64) -> f64 {
    let n = x.len();
    if n == 0 {
        return 0.0;
    }
    let w = 2.0 * std::f64::consts::PI * freq / SAMPLE_RATE as f64;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &v in x {
        let s = f64::from(v) + coeff * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    2.0 * power.max(0.0).sqrt() / n as f64
}

/// Window and hop of the marker detector, samples.
const MARKER_WIN: usize = 96;
const MARKER_HOP: usize = 48;

/// Burst onsets in `x`: (frequency index, sample of the onset). A burst is a
/// run of at least half the burst length over the threshold.
pub fn marker_onsets(spec: &AudioSpec, x: &[i16]) -> Vec<(usize, usize)> {
    let threshold = 0.4 * dbfs_amplitude(spec.marker_dbfs);
    let min_run = ((spec.marker_ms * SAMPLE_RATE as f64 / 1000.0) / 2.0 / MARKER_HOP as f64)
        .max(1.0) as usize;
    let mut out = Vec::new();
    if x.len() < MARKER_WIN {
        return out;
    }
    let hops = (x.len() - MARKER_WIN) / MARKER_HOP + 1;
    for (fi, &f) in spec.marker_freqs_hz.iter().enumerate() {
        let mut run_start: Option<usize> = None;
        for h in 0..=hops {
            let on = h < hops && {
                let s = h * MARKER_HOP;
                tone_amplitude(&x[s..s + MARKER_WIN], f) >= threshold
            };
            match (on, run_start) {
                (true, None) => run_start = Some(h),
                (false, Some(r)) => {
                    if h - r >= min_run {
                        out.push((fi, r * MARKER_HOP));
                    }
                    run_start = None;
                }
                _ => {}
            }
        }
    }
    out.sort_by_key(|&(_, s)| s);
    out
}

/// Pilot amplitude per 5 ms window.
fn pilot_levels(spec: &AudioSpec, x: &[i16]) -> Vec<f64> {
    const WIN: usize = 240;
    x.chunks_exact(WIN)
        .map(|c| tone_amplitude(c, spec.pilot_hz))
        .collect()
}

/// Gaps in the pilot after the first audio: (count, total ms, the first
/// audio's sample). A gap is the pilot under its median minus
/// `gap_drop_db` for `gap_min_ms` or more.
pub fn pilot_gaps(spec: &AudioSpec, x: &[i16]) -> (u64, f64, Option<usize>) {
    let (list, first) = pilot_gap_list(spec, x);
    (
        list.len() as u64,
        list.iter().map(|&(_, ms)| ms).sum(),
        first,
    )
}

/// The gaps themselves: (first sample, length ms), and the first audio's
/// sample.
pub fn pilot_gap_list(spec: &AudioSpec, x: &[i16]) -> (Vec<(usize, f64)>, Option<usize>) {
    const WIN_MS: f64 = 5.0;
    let levels = pilot_levels(spec, x);
    let expected = dbfs_amplitude(spec.pilot_dbfs);
    let Some(first) = levels.iter().position(|&l| l >= 0.5 * expected) else {
        return (Vec::new(), None);
    };
    let mut sorted: Vec<f64> = levels[first..].to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = sorted[sorted.len() / 2];
    let floor = median * 10f64.powf(-spec.gap_drop_db / 20.0);
    let min_windows = (spec.gap_min_ms / WIN_MS).ceil() as usize;
    let mut gaps = Vec::new();
    let mut run = 0usize;
    for (i, &l) in levels[first..]
        .iter()
        .chain(std::iter::once(&f64::INFINITY))
        .enumerate()
    {
        if l < floor {
            run += 1;
        } else {
            if run >= min_windows {
                gaps.push(((first + i - run) * 240, run as f64 * WIN_MS));
            }
            run = 0;
        }
    }
    (gaps, Some(first * 240))
}

/// What the viewer's output pulls recorded.
#[derive(Clone, Debug, Default)]
pub struct Capture {
    /// Mono output, PULL samples per pull.
    pub samples: Vec<i16>,
    /// Time of each pull, µs on `mello_clock_now_us`.
    pub pull_us: Vec<u64>,
}

impl Capture {
    /// When sample `n` is heard: its pull's time plus its place in the pull.
    pub fn heard_us(&self, n: usize) -> Option<f64> {
        let t = *self.pull_us.get(n / PULL)?;
        Some(t as f64 + (n % PULL) as f64 * 1e6 / SAMPLE_RATE as f64)
    }
}

/// The A/V and continuity scores of one viewer.
#[derive(Clone, Debug, Default)]
pub struct AvScores {
    pub markers_expected: u64,
    pub markers_matched: u64,
    pub within_pct: Option<f64>,
    pub offset_p50_ms: Option<f64>,
    pub offset_abs_p95_ms: Option<f64>,
    pub offset_min_ms: Option<f64>,
    pub offset_max_ms: Option<f64>,
    pub gaps: u64,
    pub gap_ms: f64,
    /// Each gap: when it was heard (µs), its length (ms).
    pub gap_list: Vec<(f64, f64)>,
    /// (marker k, reference sample, output sample, offset ms) of each match.
    pub matches: Vec<(u64, u64, usize, f64)>,
}

/// Score one viewer. `presents` and `clock` are the video side; the
/// detector bias comes from the same detector on the reference.
pub fn score_av(
    spec: &AudioSpec,
    reference: &ReferenceAudio,
    capture: &Capture,
    presents: &[Present],
    clock: &SourceClock,
    end_us: u64,
) -> AvScores {
    let mut out = AvScores::default();
    let (list, _) = pilot_gap_list(spec, &capture.samples);
    out.gaps = list.len() as u64;
    out.gap_ms = list.iter().map(|&(_, ms)| ms).sum();
    out.gap_list = list
        .iter()
        .filter_map(|&(n, ms)| capture.heard_us(n).map(|t| (t, ms)))
        .collect();

    // Detector bias: where it finds the reference's first markers.
    let mono_ref: Vec<i16> = reference.stereo.iter().step_by(2).copied().collect();
    let nf = spec.marker_freqs_hz.len() as u64;
    let bias = {
        let onsets = marker_onsets(spec, &mono_ref);
        let mut d: Vec<f64> = onsets
            .iter()
            .filter_map(|&(fi, s)| {
                let period = marker_sample(spec, 1) as f64;
                let k = (s as f64 / period).round() as u64;
                (k % nf == fi as u64).then(|| s as f64 - marker_sample(spec, k) as f64)
            })
            .collect();
        d.sort_by(|a, b| a.total_cmp(b));
        d.get(d.len() / 2).copied().unwrap_or(0.0)
    };

    // The video: first present time of each absolute frame.
    let fps = f64::from(clock.fps.max(1));
    let mut shown: BTreeMap<u64, u64> = BTreeMap::new();
    for p in presents {
        if let Some(code) = p.code {
            let (abs, _) = clock.capture_of(code, p.t_us);
            shown.entry(abs).or_insert(p.t_us);
        }
    }
    let frames_per_marker = spec.marker_period_ms * fps / 1000.0;
    let video_time = |k: u64| -> Option<f64> {
        let m = (k as f64 * frames_per_marker).round() as u64;
        // The frame itself, or a neighbour within 3 frames.
        let near = shown
            .range(m.saturating_sub(3)..=m + 3)
            .min_by_key(|(&a, _)| a.abs_diff(m))?;
        Some(*near.1 as f64 + (m as f64 - *near.0 as f64) * 1e6 / fps)
    };
    // Expected: markers whose video frame was shown while the output was
    // pulled, at least `av_window_ms` from its start and its end.
    let last_video = shown.keys().next_back().copied().unwrap_or(0);
    let first_video = shown.keys().next().copied().unwrap_or(0);
    let k_lo = (first_video as f64 / frames_per_marker).ceil() as u64;
    let k_hi = (last_video as f64 / frames_per_marker).floor() as u64;
    let heard_end = capture.pull_us.last().copied().unwrap_or(0).min(end_us) as f64;
    let heard_start = capture.pull_us.first().copied().unwrap_or(0) as f64;
    let mut expected: Vec<(u64, f64)> = Vec::new();
    for k in k_lo..=k_hi {
        if let Some(t) = video_time(k) {
            let window = spec.av_window_ms * 1000.0;
            if t - window >= heard_start && t + window <= heard_end {
                expected.push((k, t));
            }
        }
    }
    out.markers_expected = expected.len() as u64;

    // Match each onset to the expected marker of its frequency nearest in
    // time (within half the frequency period, +-1 s).
    let onsets = marker_onsets(spec, &capture.samples);
    let half_period_us = spec.marker_period_ms * nf as f64 * 1000.0 / 2.0;
    let mut best: BTreeMap<u64, (usize, f64)> = BTreeMap::new();
    for &(fi, s) in &onsets {
        let at = (s as f64 - bias).max(0.0) as usize;
        let Some(heard) = capture.heard_us(at) else {
            continue;
        };
        let cand = expected
            .iter()
            .filter(|(k, _)| k % nf == fi as u64)
            .map(|&(k, t)| (k, heard - t))
            .filter(|(_, d)| d.abs() < half_period_us)
            .min_by(|a, b| a.1.abs().total_cmp(&b.1.abs()));
        if let Some((k, d)) = cand {
            let e = best.entry(k).or_insert((at, d));
            if d.abs() < e.1.abs() {
                *e = (at, d);
            }
        }
    }
    let mut offsets = Vec::new();
    let mut within = 0u64;
    for (&k, &(at, d)) in &best {
        let ms = d / 1000.0;
        offsets.push(ms);
        within += u64::from(ms.abs() <= 40.0);
        out.matches.push((k, marker_sample(spec, k), at, ms));
    }
    out.markers_matched = offsets.len() as u64;
    if out.markers_expected > 0 {
        out.within_pct = Some(100.0 * within as f64 / out.markers_expected as f64);
    }
    out.offset_p50_ms = crate::metrics::percentile(&offsets, 50.0);
    let abs: Vec<f64> = offsets.iter().map(|o| o.abs()).collect();
    out.offset_abs_p95_ms = crate::metrics::percentile(&abs, 95.0);
    out.offset_min_ms = offsets.iter().cloned().reduce(f64::min);
    out.offset_max_ms = offsets.iter().cloned().reduce(f64::max);
    out
}

/// PESQ of the speech: 4 s segments of the reference (absolute clip time,
/// over loops) inside the matched markers, aligned by them. Returns (mean
/// MOS, per-segment detail, scorer note).
/// Where and how to score the speech of one run.
pub struct SpeechJob<'a> {
    /// The reference source's start, µs (`mello_stream_reference_info`).
    pub start_us: u64,
    /// The run's directory; the segments go into `pesq/` in it.
    pub dir: &'a Path,
    pub profile: &'a str,
    /// `scripts/voice-gate-score.py`.
    pub script: &'a Path,
}

pub fn score_speech(
    spec: &AudioSpec,
    reference: &ReferenceAudio,
    capture: &Capture,
    av: &AvScores,
    job: &SpeechJob<'_>,
) -> (Option<f64>, Value) {
    let (start_us, dir, profile, script) = (job.start_us, job.dir, job.profile, job.script);
    if av.matches.len() < 2 || capture.pull_us.is_empty() {
        return (
            None,
            json!({ "available": false, "reason": "too few markers" }),
        );
    }
    // One axis for both: samples since the reference source's start.
    let out_origin =
        ((capture.pull_us[0] as f64 - start_us as f64) * SAMPLE_RATE as f64 / 1e6).round() as i64;
    if out_origin < 0 {
        return (
            None,
            json!({ "available": false, "reason": "output before the source start" }),
        );
    }
    let out_origin = out_origin as usize;
    let first_k = av.matches.first().map_or(0, |m| m.0);
    let last_k = av.matches.last().map_or(0, |m| m.0);
    let r_lo = marker_sample(spec, first_k) as usize;
    let r_hi = marker_sample(spec, last_k) as usize;
    let len = out_origin + capture.samples.len();
    let mut reference_abs = vec![0i16; len.max(r_hi + 1)];
    for (n, v) in reference_abs.iter_mut().enumerate() {
        *v = reference.speech[n % reference.loop_samples];
    }
    let mut output_abs = vec![0i16; len];
    output_abs[out_origin..].copy_from_slice(&capture.samples);
    // Lag points from the markers (the marker index is absolute over loops).
    let points: Vec<voice_gate::analysis::LagPoint> = av
        .matches
        .iter()
        .map(|&(k, _, at, _)| {
            let r = marker_sample(spec, k) as f64;
            let o = (out_origin + at) as f64;
            voice_gate::analysis::LagPoint {
                t_s: r / SAMPLE_RATE as f64,
                lag_ms: (o - r) * 1000.0 / SAMPLE_RATE as f64,
                latency_ms: (o - r) * 1000.0 / SAMPLE_RATE as f64,
                ncc: 1.0,
            }
        })
        .collect();
    let seg = (spec.segment_s * SAMPLE_RATE as f64) as usize;
    let mut clips = Vec::new();
    let mut start = r_lo.div_ceil(seg) * seg;
    while start + seg <= r_hi {
        clips.push(voice_gate::run::ClipSpan {
            name: format!("t{}s", start / SAMPLE_RATE),
            start,
            len: seg,
        });
        start += seg;
    }
    if clips.is_empty() {
        return (
            None,
            json!({ "available": false, "reason": "no whole segment heard" }),
        );
    }
    let segdir = dir.join("pesq");
    let segments = match voice_gate::score::write_segments(
        &segdir,
        profile,
        &reference_abs,
        &output_abs,
        &clips,
        &points,
    ) {
        Ok(s) => s,
        Err(e) => return (None, json!({ "available": false, "reason": e })),
    };
    let scores = voice_gate::score::score(script, &segdir, &segments);
    let mos: Vec<f64> = scores.results.iter().filter_map(|(_, m)| *m).collect();
    let mean = (!mos.is_empty()).then(|| mos.iter().sum::<f64>() / mos.len() as f64);
    (
        if scores.available { mean } else { None },
        json!({
            "available": scores.available,
            "scorer": scores.scorer,
            "detail": scores.detail,
            "segments": scores.results.iter().map(|(id, m)| json!({ "id": id, "mos": m })).collect::<Vec<_>>(),
        }),
    )
}

/// The audio gates of the runs (`audio.json` gates). A gate whose metric the
/// run has not got fails.
pub fn evaluate(
    spec: &AudioSpec,
    results: &BTreeMap<String, crate::report::Metrics>,
) -> Vec<crate::report::GateResult> {
    let mut out = Vec::new();
    for g in &spec.gates {
        let Some(m) = results.get(&g.profile) else {
            continue;
        };
        let v = m.get(&g.metric).cloned().flatten();
        let min = g.min;
        let max = g.max;
        let mut rule = String::new();
        if let Some(x) = min {
            rule.push_str(&format!(">= {}", crate::report::fmt_num(Some(x))));
        }
        if let Some(x) = max {
            if !rule.is_empty() {
                rule.push_str(", ");
            }
            rule.push_str(&format!("<= {}", crate::report::fmt_num(Some(x))));
        }
        let pass = v.is_some_and(|x| {
            min.is_none_or(|b| x >= b - 1e-9) && max.is_none_or(|b| x <= b + 1e-9)
        });
        out.push(crate::report::GateResult {
            profile: g.profile.clone(),
            metric: g.metric.clone(),
            value: v,
            rule,
            enforced: true,
            pass,
            note: g.note.clone(),
        });
    }
    out
}

/// The game-audio file the host plays: `<dir>/clip-audio-v<version>-<loop>.pcm`.
pub fn track_path(dir: &Path, spec: &AudioSpec, loop_samples: usize) -> PathBuf {
    dir.join(format!("clip-audio-v{}-{}.pcm", spec.version, loop_samples))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code::FrameCode;

    fn spec() -> AudioSpec {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../benchmarks/baselines/stream");
        load(&dir.join("audio.json")).expect("audio.json loads")
    }

    fn corpus() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../libmello/tests/fixtures/voice")
    }

    /// The reference, heard `delay_ms` after its capture on a viewer whose
    /// video shows each frame `video_ms` after its capture, with `holes` of
    /// silence (start ms, length ms).
    fn simulate(
        delay_ms: f64,
        video_ms: f64,
        holes: &[(f64, f64)],
    ) -> (
        AudioSpec,
        ReferenceAudio,
        Capture,
        Vec<Present>,
        SourceClock,
    ) {
        let spec = spec();
        let loop_samples = 20 * SAMPLE_RATE;
        let reference = build(&spec, &corpus(), loop_samples).expect("reference");
        let start_us = 1_000_000_000u64;
        let clock = SourceClock {
            start_us,
            clip_frames: 1200,
            fps: 60,
        };
        // 12 s of output from 2 s of clip time on.
        let mono: Vec<i16> = reference.stereo.iter().step_by(2).copied().collect();
        let delay = (delay_ms * 48.0) as i64;
        let from = 2 * SAMPLE_RATE as i64;
        let pulls = 1200;
        let mut cap = Capture::default();
        for p in 0..pulls {
            let t0 = from + (p * PULL) as i64;
            cap.pull_us
                .push(start_us + (t0 as u64) * 1_000_000 / SAMPLE_RATE as u64);
            for j in 0..PULL as i64 {
                let src = t0 + j - delay;
                let ms = (t0 + j) as f64 / 48.0;
                let hole = holes.iter().any(|&(s, l)| ms >= s && ms < s + l);
                cap.samples.push(if src < 0 || hole {
                    0
                } else {
                    mono[(src as usize) % loop_samples]
                });
            }
        }
        let mut presents = Vec::new();
        for f in 0..1200u32 {
            let cap_us = start_us + u64::from(f) * 1_000_000 / 60;
            presents.push(Present {
                t_us: cap_us + (video_ms * 1000.0) as u64,
                code: Some(FrameCode {
                    index: f,
                    clock_ms: f * 1000 / 60,
                }),
            });
        }
        (spec, reference, cap, presents, clock)
    }

    #[test]
    fn the_reference_has_its_markers_where_they_belong() {
        let spec = spec();
        let reference = build(&spec, &corpus(), 20 * SAMPLE_RATE).expect("reference");
        let mono: Vec<i16> = reference.stereo.iter().step_by(2).copied().collect();
        let onsets = marker_onsets(&spec, &mono);
        assert_eq!(onsets.len(), 80, "one marker every 250 ms over 20 s");
        for (i, &(fi, s)) in onsets.iter().enumerate() {
            assert_eq!(fi, i % 8);
            let want = marker_sample(&spec, i as u64) as i64;
            assert!(
                (s as i64 - want).abs() <= 96,
                "marker {i} at {s}, want {want}"
            );
        }
        assert_eq!(pilot_gaps(&spec, &mono).0, 0);
    }

    #[test]
    fn a_late_audio_reads_as_its_offset() {
        let (spec, reference, cap, presents, clock) = simulate(130.0, 80.0, &[]);
        let end = cap.pull_us.last().copied().unwrap_or(0);
        let av = score_av(&spec, &reference, &cap, &presents, &clock, end);
        assert!(av.markers_expected > 30);
        assert_eq!(av.markers_matched, av.markers_expected);
        let p50 = av.offset_p50_ms.expect("offset");
        assert!((p50 - 50.0).abs() < 2.0, "offset {p50} ms, want 50");
        assert_eq!(av.within_pct, Some(0.0));
        assert_eq!(av.gaps, 0);

        let (spec, reference, cap, presents, clock) = simulate(90.0, 80.0, &[]);
        let av = score_av(&spec, &reference, &cap, &presents, &clock, end);
        assert!((av.offset_p50_ms.unwrap_or(99.0) - 10.0).abs() < 2.0);
        assert_eq!(av.within_pct, Some(100.0));
    }

    #[test]
    fn a_hole_in_the_audio_is_a_gap_and_its_markers_are_missing() {
        let (spec, reference, cap, presents, clock) = simulate(60.0, 60.0, &[(6000.0, 300.0)]);
        let end = cap.pull_us.last().copied().unwrap_or(0);
        let av = score_av(&spec, &reference, &cap, &presents, &clock, end);
        assert_eq!(av.gaps, 1);
        assert!((av.gap_ms - 300.0).abs() <= 10.0, "gap {} ms", av.gap_ms);
        assert!(av.markers_matched < av.markers_expected);
        assert!(av.within_pct.unwrap_or(100.0) < 100.0);
    }

    #[test]
    fn a_short_dip_is_no_gap() {
        let (spec, _, cap, _, _) = simulate(60.0, 60.0, &[(6000.0, 10.0)]);
        assert_eq!(pilot_gaps(&spec, &cap.samples).0, 0);
    }
}
