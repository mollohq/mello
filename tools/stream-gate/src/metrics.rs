//! Delivery and latency metrics of one viewer, from what it presented.
//!
//! Definitions.
//!
//! | Metric | Definition |
//! |---|---|
//! | new frame | a present whose burned-in frame (index, loop) differs from the last readable present; an unreadable present counts as new |
//! | duplicated | a present that shows the same frame as the present before it |
//! | dropped | clip frames between the first and the last presented frame that never reached the screen |
//! | delivered fps | new frames per second, first to last new frame |
//! | freeze | a gap over `freeze_gap_ms` between new frames, after the first frame, including a gap that runs to the end of the run; freeze time is the whole gap |
//! | time to first frame | first present minus the join start (the viewer's "Watch") |
//! | frame interval p95 | 95th percentile of the gaps between new frames |
//! | glass-to-glass | present time minus the capture time of the frame, from the burned-in clock and the reference source's start time, one value per new frame |

use crate::code::FrameCode;

/// One `mello_stream_present_frame` that delivered a frame.
#[derive(Clone, Copy, Debug)]
pub struct Present {
    /// Present time, µs on `mello_clock_now_us`.
    pub t_us: u64,
    pub code: Option<FrameCode>,
}

/// The reference source's clock, to turn a burned-in clock into a capture
/// time.
#[derive(Clone, Copy, Debug)]
pub struct SourceClock {
    pub start_us: u64,
    pub clip_frames: u32,
    pub fps: u32,
}

impl SourceClock {
    fn clip_us(&self) -> u64 {
        u64::from(self.clip_frames) * 1_000_000 / u64::from(self.fps.max(1))
    }

    /// Capture time of a frame shown at `t_us`: the latest pass over the clip
    /// whose capture time is not after `t_us`. Returns (absolute frame number,
    /// capture µs).
    pub fn capture_of(&self, code: FrameCode, t_us: u64) -> (u64, u64) {
        let clip = self.clip_us().max(1);
        let base = self.start_us + u64::from(code.clock_ms) * 1000;
        let loops = if t_us > base { (t_us - base) / clip } else { 0 };
        let abs = loops * u64::from(self.clip_frames) + u64::from(code.index);
        (abs, base + loops * clip)
    }
}

/// Delivery metrics of one viewer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Delivery {
    pub presents: u64,
    pub new_frames: u64,
    pub duplicated: u64,
    pub dropped: u64,
    pub unreadable: u64,
    pub fps: Option<f64>,
    pub freeze_count: u64,
    pub freeze_ms: f64,
    pub freeze_pct: Option<f64>,
    pub ttff_ms: Option<f64>,
    pub interval_p95_ms: Option<f64>,
    pub g2g_p50_ms: Option<f64>,
    pub g2g_p95_ms: Option<f64>,
    pub g2g_max_ms: Option<f64>,
    /// First present of each new readable frame: (index in `presents`, absolute frame).
    pub scored: Vec<(usize, u64)>,
}

pub fn percentile(v: &[f64], p: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let rank = (p / 100.0) * (s.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    Some(s[lo] + (s[hi] - s[lo]) * (rank - lo as f64))
}

/// Delivery metrics over presents from `join_us` to `end_us`.
pub fn delivery(
    presents: &[Present],
    clock: &SourceClock,
    join_us: u64,
    end_us: u64,
    freeze_gap_ms: f64,
) -> Delivery {
    let mut d = Delivery {
        presents: presents.len() as u64,
        ..Default::default()
    };
    let Some(first) = presents.first() else {
        // Nothing on screen for the whole run: one freeze of the whole run.
        d.freeze_count = 1;
        d.freeze_ms = end_us.saturating_sub(join_us) as f64 / 1000.0;
        d.freeze_pct = Some(100.0);
        return d;
    };
    d.ttff_ms = Some(first.t_us.saturating_sub(join_us) as f64 / 1000.0);

    let mut new_times: Vec<u64> = Vec::new();
    let mut g2g: Vec<f64> = Vec::new();
    let mut last_abs: Option<u64> = None;
    let mut seen: std::collections::BTreeSet<u64> = Default::default();
    for (i, p) in presents.iter().enumerate() {
        match p.code {
            None => {
                d.unreadable += 1;
                new_times.push(p.t_us);
                last_abs = None;
            }
            Some(code) => {
                let (abs, cap) = clock.capture_of(code, p.t_us);
                if last_abs == Some(abs) {
                    d.duplicated += 1;
                    continue;
                }
                last_abs = Some(abs);
                new_times.push(p.t_us);
                if seen.insert(abs) {
                    d.scored.push((i, abs));
                    g2g.push(p.t_us.saturating_sub(cap) as f64 / 1000.0);
                }
            }
        }
    }
    d.new_frames = new_times.len() as u64;
    if let (Some(lo), Some(hi)) = (seen.first(), seen.last()) {
        d.dropped = (hi - lo + 1).saturating_sub(seen.len() as u64);
    }
    let (t0, t1) = (new_times[0], *new_times.last().unwrap_or(&new_times[0]));
    if t1 > t0 {
        d.fps = Some((new_times.len() - 1) as f64 / ((t1 - t0) as f64 / 1e6));
    }
    let mut gaps: Vec<f64> = new_times
        .windows(2)
        .map(|w| (w[1] - w[0]) as f64 / 1000.0)
        .collect();
    // A freeze that lasts to the end of the run.
    gaps.push(end_us.saturating_sub(t1) as f64 / 1000.0);
    for &g in &gaps {
        if g > freeze_gap_ms {
            d.freeze_count += 1;
            d.freeze_ms += g;
        }
    }
    let shown = end_us.saturating_sub(t0) as f64 / 1000.0;
    if shown > 0.0 {
        d.freeze_pct = Some(100.0 * d.freeze_ms / shown);
    }
    gaps.pop();
    d.interval_p95_ms = percentile(&gaps, 95.0);
    d.g2g_p50_ms = percentile(&g2g, 50.0);
    d.g2g_p95_ms = percentile(&g2g, 95.0);
    d.g2g_max_ms = g2g.iter().cloned().reduce(f64::max);
    d
}

/// Time from `bins` (bytes per 100 ms since the arm time) first being at or
/// under `cap_kbps` over a 500 ms window, from `start_s`. None when it never
/// is before `end_s`.
pub fn reaction_s(bins: &[u64], start_s: f64, end_s: f64, cap_kbps: f64) -> Option<f64> {
    let first = (start_s * 10.0).round() as usize;
    let last = (end_s * 10.0).round() as usize;
    for i in first..last.min(bins.len()) {
        let lo = (i + 1).saturating_sub(5).max(first);
        if i + 1 - lo < 5 {
            continue;
        }
        let bytes: u64 = bins[lo..=i].iter().sum();
        let kbps = bytes as f64 * 8.0 / 500.0;
        if kbps <= cap_kbps {
            return Some((i + 1) as f64 / 10.0 - start_s);
        }
    }
    None
}

/// Mean rate in kbps of `bins` over [from_s, to_s).
pub fn mean_kbps(bins: &[u64], from_s: f64, to_s: f64) -> Option<f64> {
    let a = (from_s * 10.0).round() as usize;
    let b = ((to_s * 10.0).round() as usize).min(bins.len());
    if b <= a {
        return None;
    }
    let bytes: u64 = bins[a..b].iter().sum();
    Some(bytes as f64 * 8.0 / ((b - a) as f64 * 100.0))
}

/// Per metric, the median of the runs that have a value (the mean of the
/// middle two for an even count). A metric no run has stays None.
pub fn median_metrics(runs: &[crate::report::Metrics]) -> crate::report::Metrics {
    let mut keys: std::collections::BTreeSet<&String> = Default::default();
    for r in runs {
        keys.extend(r.keys());
    }
    let mut out = crate::report::Metrics::new();
    for k in keys {
        let vals: Vec<f64> = runs
            .iter()
            .filter_map(|r| r.get(k).cloned().flatten())
            .collect();
        out.insert(k.clone(), percentile(&vals, 50.0));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> SourceClock {
        SourceClock {
            start_us: 1_000_000,
            clip_frames: 1200,
            fps: 60,
        }
    }

    fn at(index: u32, delay_us: u64) -> Present {
        let c = clock();
        let ms = ((u64::from(index) * 1000 + 30) / 60) as u32;
        Present {
            t_us: c.start_us + u64::from(ms) * 1000 + delay_us,
            code: Some(FrameCode {
                index,
                clock_ms: ms,
            }),
        }
    }

    #[test]
    fn a_clean_run_has_no_freeze_and_its_latency() {
        let p: Vec<Present> = (0..600).map(|i| at(i, 70_000)).collect();
        let end = p.last().map(|x| x.t_us).unwrap_or(0) + 16_000;
        let d = delivery(&p, &clock(), 1_000_000, end, 150.0);
        assert_eq!(d.new_frames, 600);
        assert_eq!((d.duplicated, d.dropped, d.freeze_count), (0, 0, 0));
        assert!((d.fps.unwrap_or(0.0) - 60.0).abs() < 0.5);
        assert!((d.g2g_p50_ms.unwrap_or(0.0) - 70.0).abs() < 0.01);
        assert_eq!(d.ttff_ms, Some(70.0));
    }

    #[test]
    fn a_gap_is_one_freeze_and_its_frames_are_dropped() {
        let mut p: Vec<Present> = (0..100).map(|i| at(i, 50_000)).collect();
        // Frames 100..129 never arrive: 500 ms without a new frame.
        p.extend((130..200).map(|i| at(i, 50_000)));
        // A second present of frame 199.
        p.push(at(199, 60_000));
        let end = p.last().map(|x| x.t_us).unwrap_or(0) + 10_000;
        let d = delivery(&p, &clock(), 1_000_000, end, 150.0);
        assert_eq!(d.freeze_count, 1);
        assert!((d.freeze_ms - 516.7).abs() < 1.0, "{}", d.freeze_ms);
        assert_eq!(d.dropped, 30);
        assert_eq!(d.duplicated, 1);
    }

    #[test]
    fn the_capture_time_follows_the_clip_loops() {
        let c = clock();
        let code = FrameCode {
            index: 3,
            clock_ms: 50,
        };
        // Shown 80 ms after its capture in the second pass (20 s later).
        let t = c.start_us + 20_000_000 + 50_000 + 80_000;
        let (abs, cap) = c.capture_of(code, t);
        assert_eq!(abs, 1203);
        assert_eq!(t - cap, 80_000);
    }

    #[test]
    fn nothing_presented_is_one_freeze_of_the_whole_run() {
        let d = delivery(&[], &clock(), 0, 5_000_000, 150.0);
        assert_eq!((d.freeze_count, d.freeze_ms), (1, 5000.0));
        assert_eq!(d.ttff_ms, None);
    }

    #[test]
    fn the_median_of_three_runs_takes_each_metric_alone() {
        let run = |a: Option<f64>, b: Option<f64>| {
            let mut m = crate::report::Metrics::new();
            m.insert("freeze_ms".into(), a);
            m.insert("vmaf_mean".into(), b);
            m
        };
        let med = median_metrics(&[
            run(Some(9000.0), Some(90.0)),
            run(Some(100.0), None),
            run(Some(300.0), Some(80.0)),
        ]);
        assert_eq!(med["freeze_ms"], Some(300.0));
        assert_eq!(med["vmaf_mean"], Some(85.0));
        let none = median_metrics(&[run(None, None)]);
        assert_eq!(none["freeze_ms"], None);
    }

    #[test]
    fn reaction_finds_the_first_window_under_the_cap() {
        // 5 Mbps (62500 B per bin) until 7 s, then 2.5 Mbps.
        let bins: Vec<u64> = (0..100)
            .map(|i| if i < 70 { 62_500 } else { 31_250 })
            .collect();
        let r = reaction_s(&bins, 5.0, 9.0, 3000.0).expect("reacts");
        assert!((r - 2.5).abs() < 0.11, "{r}");
        assert_eq!(reaction_s(&bins, 0.0, 5.0, 3000.0), None);
        assert_eq!(mean_kbps(&bins, 8.0, 9.0), Some(2500.0));
    }
}
