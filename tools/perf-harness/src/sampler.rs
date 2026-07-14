use std::time::{Duration, Instant};

use mello_core::stats::proc_rusage;

/// One raw sample. `footprint_mb` is a gauge; `cpu_ns` and `wakeups` are
/// cumulative-since-process-start counters — rates are derived across the
/// window in [`summarize`].
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub at: Instant,
    pub footprint_mb: f32,
    pub cpu_ns: u64,
    pub wakeups: u64,
}

#[derive(Debug, Clone, Default)]
pub struct SampleSummary {
    pub samples: usize,
    pub footprint_mb_min: f32,
    pub footprint_mb_p50: f32,
    pub footprint_mb_p95: f32,
    pub footprint_mb_max: f32,
    /// Average wakeups/sec over the sample window (the macOS energy metric).
    pub wakeups_per_s: f32,
    /// Average CPU as % of one core over the sample window.
    pub cpu_pct: f32,
}

/// Sample macOS `phys_footprint` + cumulative wakeups/CPU for `pid`.
pub fn sample_process(pid: u32) -> Option<Sample> {
    let r = proc_rusage(pid)?;
    Some(Sample {
        at: Instant::now(),
        footprint_mb: r.phys_footprint_bytes as f32 / (1024.0 * 1024.0),
        cpu_ns: r.user_time_ns + r.system_time_ns,
        wakeups: r.pkg_idle_wakeups + r.interrupt_wakeups,
    })
}

pub fn summarize(samples: &[Sample]) -> SampleSummary {
    if samples.is_empty() {
        return SampleSummary::default();
    }

    let mut fp: Vec<f32> = samples.iter().map(|s| s.footprint_mb).collect();
    fp.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    // Rates need a span: derive from first/last cumulative counters.
    let mut wakeups_per_s = 0.0;
    let mut cpu_pct = 0.0;
    if samples.len() >= 2 {
        let first = &samples[0];
        let last = &samples[samples.len() - 1];
        let elapsed_s = last.at.duration_since(first.at).as_secs_f64();
        if elapsed_s > 0.0 {
            let d_wakeups = last.wakeups.saturating_sub(first.wakeups) as f64;
            wakeups_per_s = (d_wakeups / elapsed_s) as f32;
            let d_cpu_ns = last.cpu_ns.saturating_sub(first.cpu_ns) as f64;
            cpu_pct = ((d_cpu_ns / 1_000_000_000.0) / elapsed_s * 100.0) as f32;
        }
    }

    SampleSummary {
        samples: samples.len(),
        footprint_mb_min: fp[0],
        footprint_mb_p50: percentile(&fp, 50.0),
        footprint_mb_p95: percentile(&fp, 95.0),
        footprint_mb_max: *fp.last().unwrap_or(&0.0),
        wakeups_per_s,
        cpu_pct,
    }
}

fn percentile(sorted: &[f32], pct: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f32 - 1.0) * (pct / 100.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

pub fn collect_for(pid: u32, duration: Duration, interval: Duration) -> Vec<Sample> {
    let deadline = Instant::now() + duration;
    let mut out = Vec::new();
    while Instant::now() < deadline {
        if let Some(s) = sample_process(pid) {
            out.push(s);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(interval.min(remaining));
    }
    out
}
