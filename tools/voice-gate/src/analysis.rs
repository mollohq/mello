//! Structural metrics from the reference and the received output:
//! playout delay over time (windowed cross-correlation), recovery after an
//! event, and speech dropouts.

use crate::dsp;
use crate::profile::AnalysisSpec;
use crate::run::{ClipSpan, SAMPLE_RATE};

/// Decimation factor for the delay search: 48 kHz to 4 kHz.
const DECIM: usize = 12;
const RATE4: f64 = (SAMPLE_RATE / DECIM) as f64;

/// One delay measurement.
#[derive(Clone, Debug)]
pub struct LagPoint {
    /// Reference (sender) time of the window centre, seconds.
    pub t_s: f64,
    /// Output position minus reference position, ms.
    pub lag_ms: f64,
    /// Mouth-to-ear delay: playout time minus capture time on the receiver
    /// clock, ms. Equal to `lag_ms` when the clocks do not drift.
    pub latency_ms: f64,
    /// Normalized cross-correlation at the peak.
    pub ncc: f64,
}

/// Delay curve. Windows where the reference is not speech, or where no
/// clear match exists in the output, give no point.
pub fn lag_curve(
    reference: &[i16],
    output: &[i16],
    clock_ratio: f64,
    a: &AnalysisSpec,
) -> Vec<LagPoint> {
    let taps = dsp::lowpass_taps(1800.0 / SAMPLE_RATE as f64, 145);
    let r4 = dsp::decimate(reference, DECIM, &taps);
    let o4 = dsp::decimate(output, DECIM, &taps);
    let win = (a.window_ms * RATE4 / 1000.0) as usize;
    let hop = (a.hop_ms * RATE4 / 1000.0) as usize;
    let max_lag = (a.max_lag_ms * RATE4 / 1000.0) as usize;
    let n_fft = (win + max_lag).next_power_of_two();

    let mut points = Vec::new();
    let mut start = 0usize;
    let mut sr = vec![0.0; n_fft];
    let mut si = vec![0.0; n_fft];
    let mut or = vec![0.0; n_fft];
    let mut oi = vec![0.0; n_fft];
    while start + win <= r4.len() {
        let full = &reference[start * DECIM..(start + win) * DECIM];
        if dsp::dbfs(dsp::rms_i16(full)) < a.active_dbfs || start >= o4.len() {
            start += hop;
            continue;
        }
        let seg_end = (start + win + max_lag).min(o4.len());
        if seg_end < start + win {
            start += hop;
            continue;
        }
        let seg = &o4[start..seg_end];
        let rw = &r4[start..start + win];

        sr.iter_mut().for_each(|v| *v = 0.0);
        si.iter_mut().for_each(|v| *v = 0.0);
        or.iter_mut().for_each(|v| *v = 0.0);
        oi.iter_mut().for_each(|v| *v = 0.0);
        sr[..win].copy_from_slice(rw);
        or[..seg.len()].copy_from_slice(seg);
        dsp::fft(&mut sr, &mut si, false);
        dsp::fft(&mut or, &mut oi, false);
        // corr = IFFT(O * conj(R)); index = lag. No wrap for lag <= len(seg) - win.
        for k in 0..n_fft {
            let (a_r, a_i) = (or[k], oi[k]);
            let (b_r, b_i) = (sr[k], -si[k]);
            or[k] = a_r * b_r - a_i * b_i;
            oi[k] = a_r * b_i + a_i * b_r;
        }
        dsp::fft(&mut or, &mut oi, true);
        let lags = seg.len() - win;
        let mut best = 0usize;
        let mut best_v = f64::MIN;
        for (lag, &v) in or.iter().enumerate().take(lags + 1) {
            if v > best_v {
                best_v = v;
                best = lag;
            }
        }
        let corr = best_v / n_fft as f64;
        let er: f64 = rw.iter().map(|x| x * x).sum();
        let eo: f64 = seg[best..best + win].iter().map(|x| x * x).sum();
        let ncc = if er > 0.0 && eo > 0.0 {
            corr / (er * eo).sqrt()
        } else {
            0.0
        };
        if ncc >= a.min_ncc {
            let centre_ref = (start + win / 2) * DECIM;
            let lag_ms = best as f64 * 1000.0 / RATE4;
            // Content at reference sample k was captured at receiver time
            // k * clock_ratio. latency = playout - capture.
            let latency_ms =
                lag_ms + (1.0 - clock_ratio) * centre_ref as f64 * 1000.0 / SAMPLE_RATE as f64;
            points.push(LagPoint {
                t_s: centre_ref as f64 / SAMPLE_RATE as f64,
                lag_ms,
                latency_ms,
                ncc,
            });
        }
        start += hop;
    }
    points
}

/// Summary of a delay curve.
#[derive(Clone, Debug, Default)]
pub struct DelaySummary {
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub max: Option<f64>,
    /// Median latency over the first corpus loop.
    pub start: Option<f64>,
    /// Median latency over the last complete corpus loop (None with one loop).
    pub end: Option<f64>,
    /// end - start: the same speech, later in the run.
    pub growth: Option<f64>,
    /// Median over clips of (latency in the last third of the clip - latency
    /// in the first third). Shows delay that climbs inside one talk spurt.
    pub clip_growth: Option<f64>,
    pub points: usize,
}

fn median(v: &[f64]) -> Option<f64> {
    dsp::percentile(v, 50.0)
}

fn latencies_between(points: &[LagPoint], from_s: f64, to_s: f64) -> Vec<f64> {
    points
        .iter()
        .filter(|p| p.t_s >= from_s && p.t_s < to_s)
        .map(|p| p.latency_ms)
        .collect()
}

/// Summarize the delay curve. `clips` are the clip spans of the run in
/// order; `clips_per_loop` clips make one pass over the corpus. Start and
/// end compare the same speech (first loop against last complete loop), so
/// a difference is delay the receiver built up, not a property of a clip.
pub fn summarize(points: &[LagPoint], clips: &[ClipSpan], clips_per_loop: usize) -> DelaySummary {
    let lat: Vec<f64> = points.iter().map(|p| p.latency_ms).collect();
    let sr = SAMPLE_RATE as f64;
    let loop_span = |l: usize| -> Option<(f64, f64)> {
        let first = clips.get(l * clips_per_loop)?;
        let last = clips.get((l + 1) * clips_per_loop - 1)?;
        Some((first.start as f64 / sr, (last.start + last.len) as f64 / sr))
    };
    let loops = if clips_per_loop == 0 {
        0
    } else {
        clips.len() / clips_per_loop
    };
    let start = loop_span(0).and_then(|(a, b)| median(&latencies_between(points, a, b)));
    let end = if loops >= 2 {
        loop_span(loops - 1).and_then(|(a, b)| median(&latencies_between(points, a, b)))
    } else {
        None
    };
    let mut per_clip = Vec::new();
    for c in clips {
        let a = c.start as f64 / sr;
        let third = c.len as f64 / sr / 3.0;
        let head = median(&latencies_between(points, a, a + third));
        let tail = median(&latencies_between(points, a + 2.0 * third, a + 3.0 * third));
        if let (Some(h), Some(t)) = (head, tail) {
            per_clip.push(t - h);
        }
    }
    DelaySummary {
        p50: median(&lat),
        p95: dsp::percentile(&lat, 95.0),
        max: lat
            .iter()
            .cloned()
            .fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v)))),
        start,
        end,
        growth: match (start, end) {
            (Some(s), Some(e)) => Some(e - s),
            _ => None,
        },
        clip_growth: median(&per_clip),
        points: points.len(),
    }
}

/// Recovery after an event (jitter burst or outage) that spans
/// [event_start_s, event_end_s] of reference time.
#[derive(Clone, Debug)]
pub struct Recovery {
    /// Median latency in the 10 s before the event.
    pub before_ms: Option<f64>,
    /// Highest latency from the event start to the end of the run.
    pub peak_ms: Option<f64>,
    /// Seconds from the event end until the latency is back within the
    /// tolerance of `before_ms` and stays there for the hold time. None when
    /// that never happens in the run.
    pub recovery_s: Option<f64>,
}

pub fn recovery(
    points: &[LagPoint],
    event_start_s: f64,
    event_end_s: f64,
    a: &AnalysisSpec,
) -> Recovery {
    let before: Vec<f64> = points
        .iter()
        .filter(|p| p.t_s >= event_start_s - 10.0 && p.t_s < event_start_s)
        .map(|p| p.latency_ms)
        .collect();
    let before_ms = median(&before);
    let peak_ms = points
        .iter()
        .filter(|p| p.t_s >= event_start_s)
        .map(|p| p.latency_ms)
        .fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))));
    let mut recovery_s = None;
    if let Some(b) = before_ms {
        let limit = b + a.recovery_tolerance_ms;
        let hold = a.recovery_hold_ms / 1000.0;
        let after: Vec<&LagPoint> = points.iter().filter(|p| p.t_s >= event_end_s).collect();
        for (i, p) in after.iter().enumerate() {
            let window: Vec<&&LagPoint> = after[i..]
                .iter()
                .take_while(|q| q.t_s <= p.t_s + hold)
                .collect();
            let covers = window
                .last()
                .map(|q| q.t_s - p.t_s >= hold * 0.5)
                .unwrap_or(false);
            if covers && window.iter().all(|q| q.latency_ms <= limit) {
                recovery_s = Some((p.t_s - event_end_s).max(0.0));
                break;
            }
        }
    }
    Recovery {
        before_ms,
        peak_ms,
        recovery_s,
    }
}

/// Output lag (samples) at reference position `k`: the lag of the nearest
/// delay point. None when the curve is empty.
pub fn lag_at(points: &[LagPoint], k: usize) -> Option<usize> {
    let t = k as f64 / SAMPLE_RATE as f64;
    points
        .iter()
        .min_by(|a, b| {
            (a.t_s - t)
                .abs()
                .partial_cmp(&(b.t_s - t).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|p| (p.lag_ms * SAMPLE_RATE as f64 / 1000.0).round() as usize)
}

/// Speech frames (20 ms) that did not come out intact. A reference frame
/// with speech is a dropout when the output at the measured lag is
/// - more than `dropout_db` below the reference, after the overall level
///   difference (AGC) is taken out, or
/// - not the same audio: the best normalized correlation within +-1 ms of
///   the lag is below `dropout_ncc` (concealment or wrong audio).
pub struct Dropouts {
    /// Reference sample positions of the dropout frames.
    pub frames: Vec<usize>,
    pub frame_ms: f64,
    /// Output level relative to reference over speech frames, dB.
    pub level_offset_db: Option<f64>,
}

impl Dropouts {
    pub fn total_ms(&self) -> f64 {
        self.frames.len() as f64 * self.frame_ms
    }

    /// Dropout ms with reference position in [from, to).
    pub fn ms_between(&self, from: usize, to: usize) -> f64 {
        self.frames.iter().filter(|&&k| k >= from && k < to).count() as f64 * self.frame_ms
    }
}

/// Best normalized correlation of `r` against `out` at offsets
/// `centre - 48 ..= centre + 48` (step 4 samples, +-1 ms).
fn best_frame_ncc(r: &[i16], out: &[i16], centre: usize) -> f64 {
    let er: f64 = r.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
    let mut best = -1.0f64;
    let mut d = -48isize;
    while d <= 48 {
        let o = centre as isize + d;
        d += 4;
        if o < 0 || o as usize + r.len() > out.len() {
            continue;
        }
        let seg = &out[o as usize..o as usize + r.len()];
        let mut dot = 0.0;
        let mut eo = 0.0;
        for (&a, &b) in r.iter().zip(seg) {
            dot += f64::from(a) * f64::from(b);
            eo += f64::from(b) * f64::from(b);
        }
        if er > 0.0 && eo > 0.0 {
            best = best.max(dot / (er * eo).sqrt());
        }
    }
    best
}

pub fn dropouts(
    reference: &[i16],
    output: &[i16],
    points: &[LagPoint],
    a: &AnalysisSpec,
) -> Dropouts {
    const FRAME: usize = 960;
    // (reference position, reference rms, output rms, output ncc)
    let mut frames_seen: Vec<(usize, f64, f64, f64)> = Vec::new();
    let mut k = 0usize;
    while k + FRAME <= reference.len() {
        let r = &reference[k..k + FRAME];
        let rr = dsp::rms_i16(r);
        if dsp::dbfs(rr) >= a.active_dbfs {
            // The lag can step between two delay points (a talk spurt
            // start, a concealment). Try the lag on each side of the frame
            // and keep the better match.
            let t = (k + FRAME / 2) as f64 / SAMPLE_RATE as f64;
            let after = points.partition_point(|p| p.t_s < t);
            let mut best: Option<(f64, f64)> = None;
            for p in points[after.saturating_sub(1)..(after + 1).min(points.len())].iter() {
                let o = k + (p.lag_ms * SAMPLE_RATE as f64 / 1000.0).round() as usize;
                let cand = if o + FRAME <= output.len() {
                    (
                        dsp::rms_i16(&output[o..o + FRAME]),
                        best_frame_ncc(r, output, o),
                    )
                } else {
                    (0.0, 0.0)
                };
                if best.is_none_or(|b| cand.1 > b.1) {
                    best = Some(cand);
                }
            }
            if let Some((orms, ncc)) = best {
                frames_seen.push((k, rr, orms, ncc));
            }
        }
        k += FRAME;
    }
    let ratios: Vec<f64> = frames_seen
        .iter()
        .filter(|p| p.2 > 0.0)
        .map(|p| p.2 / p.1)
        .collect();
    let gain = dsp::percentile(&ratios, 50.0);
    let mut frames = Vec::new();
    if let Some(g) = gain {
        let floor = g * 10f64.powf(a.dropout_db / 20.0);
        for (k, rr, orms, ncc) in &frames_seen {
            if *orms < rr * floor || *ncc < a.dropout_ncc {
                frames.push(*k);
            }
        }
    }
    Dropouts {
        frames,
        frame_ms: 20.0,
        level_offset_db: gain.map(|g| 20.0 * g.log10()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> AnalysisSpec {
        AnalysisSpec {
            window_ms: 1000.0,
            hop_ms: 250.0,
            max_lag_ms: 1000.0,
            min_ncc: 0.5,
            active_dbfs: -50.0,
            recovery_tolerance_ms: 20.0,
            recovery_hold_ms: 1000.0,
            dropout_db: -20.0,
            dropout_ncc: 0.2,
        }
    }

    /// Deterministic noise-like test signal with speech-like level.
    fn signal(n: usize) -> Vec<i16> {
        let mut r = crate::rng::Rng::new(3);
        (0..n).map(|_| (r.gaussian() * 3000.0) as i16).collect()
    }

    #[test]
    fn finds_a_known_delay() {
        let x = signal(48_000 * 6);
        let delay = 48 * 137; // 137 ms
        let mut y = vec![0i16; delay];
        y.extend(x.iter().map(|v| v / 2));
        let pts = lag_curve(&x, &y, 1.0, &spec());
        assert!(!pts.is_empty());
        for p in &pts {
            assert!((p.lag_ms - 137.0).abs() <= 0.5, "lag {}", p.lag_ms);
            assert!(p.ncc > 0.9);
        }
        let d = dropouts(&x, &y, &pts, &spec());
        assert_eq!(d.total_ms(), 0.0);
        assert!((d.level_offset_db.expect("gain") + 6.02).abs() < 0.2);
    }

    #[test]
    fn sees_growth_and_dropouts() {
        let x = signal(48_000 * 8);
        // 40 ms delay for the first 4 s, then 120 ms with an 80 ms gap of
        // silence in the output.
        let mut y = vec![0i16; 48 * 40];
        y.extend_from_slice(&x[..48_000 * 4]);
        y.extend(std::iter::repeat_n(0, 48 * 80));
        y.extend_from_slice(&x[48_000 * 4..]);
        let a = spec();
        let pts = lag_curve(&x, &y, 1.0, &a);
        // Two "loops" of one 3.5 s clip each, and one clip across the step.
        let clip = |s: f64, l: f64| ClipSpan {
            name: "c".into(),
            start: (s * 48_000.0) as usize,
            len: (l * 48_000.0) as usize,
        };
        let s = summarize(&pts, &[clip(0.0, 3.5), clip(4.5, 3.5)], 1);
        assert!((s.start.expect("start") - 40.0).abs() < 1.0);
        assert!((s.end.expect("end") - 120.0).abs() < 1.0);
        assert!((s.growth.expect("growth") - 80.0).abs() < 2.0);
        let across = summarize(&pts, &[clip(1.0, 6.0)], 1);
        assert!((across.clip_growth.expect("clip growth") - 80.0).abs() < 2.0);
        assert_eq!(across.end, None);
        let d = dropouts(&x, &y, &pts, &a);
        assert!(d.total_ms() <= 100.0);
    }

    #[test]
    fn recovery_measures_return_to_baseline() {
        let a = spec();
        let mut pts = Vec::new();
        for i in 0..200 {
            let t = i as f64 * 0.25;
            let lat = if (20.0..25.0).contains(&t) {
                200.0
            } else if (25.0..28.0).contains(&t) {
                100.0
            } else {
                60.0
            };
            pts.push(LagPoint {
                t_s: t,
                lag_ms: lat,
                latency_ms: lat,
                ncc: 1.0,
            });
        }
        let r = recovery(&pts, 20.0, 25.0, &a);
        assert_eq!(r.before_ms, Some(60.0));
        assert_eq!(r.peak_ms, Some(200.0));
        assert_eq!(r.recovery_s, Some(3.0));
    }
}
