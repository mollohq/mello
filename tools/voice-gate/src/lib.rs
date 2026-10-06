//! Voice quality gate.
//!
//! Pushes a speech corpus through the real libmello voice path (capture
//! inject, DSP, Opus encode, an impairment shim that behaves like the SFU
//! network, jitter buffer, decode, concealment, mix) and measures the
//! result. `scripts/voice-gate.sh` runs every profile and prints the delta
//! against `benchmarks/baselines/voice/baseline-2.9.json`.
//!
//! See the "Voice quality gate" section of `TESTING.md` for the method.

pub mod analysis;
pub mod dsp;
pub mod profile;
pub mod report;
pub mod rng;
pub mod run;
pub mod score;
pub mod shim;
pub mod wav;

use analysis::LagPoint;
use profile::{AnalysisSpec, Profile};
use report::Metrics;
use run::{RunResult, SAMPLE_RATE};
use serde_json::{json, Value};

/// Structural metrics of one run, plus the delay curve.
pub struct Measured {
    pub metrics: Metrics,
    pub points: Vec<LagPoint>,
    pub extra: Value,
}

fn n(v: u64) -> Option<f64> {
    Some(v as f64)
}

/// Compute every structural metric of a run. MOS is added by the caller.
/// `clips_per_loop` is the number of corpus clips (one pass over the corpus).
pub fn measure(
    profile: &Profile,
    run: &RunResult,
    a: &AnalysisSpec,
    clips_per_loop: usize,
) -> Measured {
    let points = analysis::lag_curve(&run.reference, &run.output, run.clock_ratio, a);
    let delay = analysis::summarize(&points, &run.clips, clips_per_loop);
    let drops = analysis::dropouts(&run.reference, &run.output, &points, a);

    // The event whose recovery the gate checks: the first jitter episode,
    // else the outage.
    let event = profile
        .network
        .episodes
        .first()
        .map(|e| (e.start_s, e.start_s + e.dur_s))
        .or(profile.network.outage.map(|(s, d)| (s, s + d)));
    let rec = event.map(|(s, e)| analysis::recovery(&points, s, e, a));

    let rx = &run.rx;
    let conceal = u64::from(rx.rx_conceal_missing_plc)
        + u64::from(rx.rx_conceal_gap_fec)
        + u64::from(rx.rx_conceal_gap_plc);
    let lost = run.shim.lost()
        + u64::from(rx.rx_jitter_dropped_late)
        + u64::from(rx.rx_jitter_dropped_overflow);

    let buf: Vec<f64> = run
        .stats
        .iter()
        .map(|s| f64::from(s.playout_buffer_ms))
        .collect();
    let target: Vec<f64> = run
        .stats
        .iter()
        .filter(|s| s.jitter_target_ms > 0.0)
        .map(|s| f64::from(s.jitter_target_ms))
        .collect();

    let wrap_dropout = run.wrap_sender_sample.map(|w| {
        let from = w.saturating_sub(SAMPLE_RATE);
        drops.ms_between(from, w + 3 * SAMPLE_RATE)
    });

    let mut m = Metrics::new();
    m.insert("mos".into(), None);
    m.insert("mos_min".into(), None);
    m.insert("latency_p50_ms".into(), delay.p50);
    m.insert("latency_p95_ms".into(), delay.p95);
    m.insert("latency_start_ms".into(), delay.start);
    m.insert("latency_end_ms".into(), delay.end);
    m.insert("latency_growth_ms".into(), delay.growth);
    m.insert("latency_clip_growth_ms".into(), delay.clip_growth);
    m.insert("latency_max_ms".into(), delay.max);
    if let Some(r) = &rec {
        m.insert("event_before_ms".into(), r.before_ms);
        m.insert("event_peak_ms".into(), r.peak_ms);
        m.insert("recovery_s".into(), r.recovery_s);
    }
    m.insert(
        "conceal_per_lost".into(),
        if lost > 0 {
            Some(conceal as f64 / lost as f64)
        } else {
            None
        },
    );
    m.insert("lost_frames".into(), n(lost));
    m.insert("shim_lost".into(), n(run.shim.lost()));
    m.insert("conceal_frames".into(), n(conceal));
    m.insert(
        "conceal_missing_plc".into(),
        n(u64::from(rx.rx_conceal_missing_plc)),
    );
    m.insert(
        "conceal_gap_fec".into(),
        n(u64::from(rx.rx_conceal_gap_fec)),
    );
    m.insert(
        "conceal_gap_plc".into(),
        n(u64::from(rx.rx_conceal_gap_plc)),
    );
    m.insert(
        "fill_plc_frames".into(),
        n(u64::from(rx.rx_conceal_fill_plc)),
    );
    m.insert("underruns".into(), Some(f64::from(rx.underrun_count)));
    m.insert("decode_errors".into(), n(u64::from(rx.rx_decode_errors)));
    m.insert("packets_sent".into(), n(run.shim.sent));
    m.insert("packets_fed".into(), n(run.packets_fed));
    m.insert(
        "rx_dropped_late".into(),
        n(u64::from(rx.rx_jitter_dropped_late)),
    );
    m.insert(
        "rx_dropped_overflow".into(),
        n(u64::from(rx.rx_jitter_dropped_overflow)),
    );
    m.insert("rx_jitter_resets".into(), n(u64::from(rx.rx_jitter_resets)));
    m.insert("dropout_ms".into(), Some(drops.total_ms()));
    if run.wrap_sender_sample.is_some() {
        m.insert("wrap_dropout_ms".into(), wrap_dropout);
    }
    m.insert("playout_buffer_p50_ms".into(), dsp::percentile(&buf, 50.0));
    m.insert("playout_buffer_max_ms".into(), dsp::percentile(&buf, 100.0));
    m.insert(
        "jitter_target_p50_ms".into(),
        dsp::percentile(&target, 50.0),
    );

    let extra = json!({
        "delay_points": delay.points,
        "output_level_offset_db": drops.level_offset_db,
        "wrap_at_s": run.wrap_sender_sample.map(|w| w as f64 / SAMPLE_RATE as f64),
        "event_s": event.map(|(s, e)| json!([s, e])),
        "shim": {
            "sent": run.shim.sent,
            "lost_random": run.shim.lost_random,
            "lost_burst": run.shim.lost_burst,
            "lost_outage": run.shim.lost_outage,
            "reordered": run.shim.reordered,
            "delivered": run.shim.delivered,
        },
        "receiver_rtp_recv_total": run.rx.rtp_recv_total,
        "jitter_missing": run.rx.rx_jitter_missing,
        "frames_decoded": run.rx.rx_frames_decoded,
        "wall_s": (run.wall_s * 10.0).round() / 10.0,
    });
    Measured {
        metrics: m,
        points,
        extra,
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    /// Wiring check for every `cargo test --workspace`: a short clean run
    /// through the real libmello path, device-free. Proves that audio comes
    /// out of the receiver and lines up with the input. It does not score.
    #[test]
    fn clean_run_produces_aligned_speech() {
        run::select_test_backend();
        let gf = profile::load(&root().join("benchmarks/baselines/voice/profiles.json"))
            .expect("profiles.json");
        let clips = run::load_corpus(&root(), &gf.corpus).expect("corpus");
        let mut p = gf
            .profiles
            .iter()
            .find(|p| p.name == "clean")
            .expect("clean profile")
            .clone();
        p.duration_s = 14.0;
        let (reference, spans) = run::build_reference(&clips, &gf.corpus, p.duration_s);
        let trace = run::encode(&reference, &p.sender).expect("encode");
        let r = run::run_receiver(&p, &reference, &spans, &trace, 1000).expect("run");
        let m = measure(&p, &r, &gf.analysis, gf.corpus.clips.len());
        let get = |k: &str| m.metrics.get(k).cloned().flatten();

        assert!(r.shim.sent > 100, "sender produced {} packets", r.shim.sent);
        assert_eq!(
            r.packets_fed, r.shim.sent,
            "clean network delivers every packet"
        );
        assert_eq!(get("decode_errors"), Some(0.0));
        let p50 = get("latency_p50_ms").expect("no delay measured: output does not match input");
        assert!((20.0..400.0).contains(&p50), "delay p50 {p50} ms");
        assert!(m.points.iter().all(|pt| pt.ncc > 0.5));
        assert!(get("dropout_ms").unwrap_or(f64::MAX) < 500.0);

        // The baseline delta only means something if a run is repeatable:
        // the same trace through the same profile gives the same output.
        let again = run::run_receiver(&p, &reference, &spans, &trace, 1000).expect("second run");
        assert!(
            again.output == r.output,
            "receiver output is not deterministic"
        );
    }
}
