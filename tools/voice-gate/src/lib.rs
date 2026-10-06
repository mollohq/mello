//! Voice quality gate.
//!
//! Pushes a speech corpus through the real libmello voice path (capture
//! inject, DSP, Opus encode, an impairment shim that behaves like the SFU
//! network, NetEQ, mix) and measures the result. `scripts/voice-gate.sh` runs every profile and prints the delta
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

/// NetEQ concealment of a run in 20 ms frames: every concealed sample
/// (voice expand and its fade to comfort noise) plus the frames repaired
/// from in-band FEC.
fn neteq_conceal_frames(rx: &mello_sys::MelloDebugStats) -> f64 {
    let fec = rx
        .rx_fec_packets_received
        .saturating_sub(rx.rx_fec_packets_discarded);
    rx.rx_concealed_samples as f64 / 960.0 + fec as f64
}

/// Compute every structural metric of a run. MOS is added by the caller.
/// `clips_per_loop` is the number of corpus clips (one pass over the corpus).
/// `lossless` is the same run on the same network without loss (see
/// [`Network::without_loss`](profile::Network::without_loss)); it gives the
/// concealment that is not caused by loss.
pub fn measure(
    profile: &Profile,
    run: &RunResult,
    lossless: Option<&RunResult>,
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
    // Concealment for losses, NetEQ era. NetEQ conceals by time: also while
    // the sender's gate is closed and a talk pause plays out, which is not
    // loss. So the concealment for losses is this run's concealment (expand,
    // its fade to comfort noise, and FEC repairs) minus the concealment of
    // the same packets on the same network without loss. The old counters
    // (PLC on Missing, FEC on a gap, PLC on a decode error) have no NetEQ
    // equivalent and print n/a.
    let fec_frames = rx
        .rx_fec_packets_received
        .saturating_sub(rx.rx_fec_packets_discarded);
    let voice_concealed = rx
        .rx_concealed_samples
        .saturating_sub(rx.rx_silent_concealed_samples);
    let conceal_frames = lossless.map_or(0.0, |l| {
        (neteq_conceal_frames(rx) - neteq_conceal_frames(&l.rx)).max(0.0)
    });
    // Lost frames that need concealment: network loss and receiver drops,
    // without lost Opus DTX frames (2 bytes or less). Through those NetEQ
    // keeps playing comfort noise, so there is nothing to conceal.
    let lost = run.shim.lost() - run.shim.lost_dtx + rx.rx_packets_discarded;

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
        if lost > 0 && lossless.is_some() {
            Some(conceal_frames / lost as f64)
        } else {
            None
        },
    );
    m.insert("lost_frames".into(), n(lost));
    m.insert("shim_lost".into(), n(run.shim.lost()));
    m.insert("lost_dtx_frames".into(), n(run.shim.lost_dtx));
    m.insert(
        "conceal_frames".into(),
        lossless.is_some().then_some(conceal_frames),
    );
    // No NetEQ equivalent: printed as n/a against the baseline.
    for retired in [
        "conceal_missing_plc",
        "conceal_gap_fec",
        "conceal_gap_plc",
        "fill_plc_frames",
        "rx_dropped_overflow",
    ] {
        m.insert(retired.into(), None);
    }
    m.insert("fec_frames".into(), n(fec_frames));
    m.insert(
        "voice_concealed_ms".into(),
        Some(voice_concealed as f64 / 48.0),
    );
    m.insert("concealment_events".into(), n(rx.rx_concealment_events));
    m.insert(
        "accelerate_ms".into(),
        Some(rx.rx_removed_samples_for_acceleration as f64 / 48.0),
    );
    m.insert(
        "decelerate_ms".into(),
        Some(rx.rx_inserted_samples_for_deceleration as f64 / 48.0),
    );
    m.insert("interruptions".into(), n(rx.rx_interruption_count));
    m.insert("talk_spurts".into(), n(rx.rx_talk_spurts));
    m.insert("underruns".into(), Some(f64::from(rx.underrun_count)));
    m.insert("late_underruns".into(), n(u64::from(rx.rx_late_underruns)));
    m.insert(
        "decode_errors".into(),
        n(u64::from(rx.rx_decode_errors) + rx.rx_insert_errors),
    );
    m.insert("packets_sent".into(), n(run.shim.sent));
    m.insert("packets_fed".into(), n(run.packets_fed));
    m.insert("rx_dropped_late".into(), n(rx.rx_packets_discarded));
    m.insert(
        "rx_jitter_resets".into(),
        n(rx.rx_stream_resets + rx.rx_buffer_flushes),
    );
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
    let filtered: Vec<f64> = run
        .stats
        .iter()
        .filter(|s| s.filtered_delay_ms > 0.0)
        .map(|s| f64::from(s.filtered_delay_ms))
        .collect();
    m.insert(
        "neteq_delay_p50_ms".into(),
        dsp::percentile(&filtered, 50.0),
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
            "lost_dtx": run.shim.lost_dtx,
        },
        "receiver_rtp_recv_total": run.rx.rtp_recv_total,
        "neteq": {
            "total_samples_received": run.rx.rx_total_samples_received,
            "concealed_samples": run.rx.rx_concealed_samples,
            "silent_concealed_samples": run.rx.rx_silent_concealed_samples,
            "concealment_events": run.rx.rx_concealment_events,
            "fec_packets_received": run.rx.rx_fec_packets_received,
            "fec_packets_discarded": run.rx.rx_fec_packets_discarded,
            "packets_discarded": run.rx.rx_packets_discarded,
            "buffer_flushes": run.rx.rx_buffer_flushes,
            "stream_resets": run.rx.rx_stream_resets,
            "packets_inserted": run.rx.rx_packets_inserted,
            "insert_errors": run.rx.rx_insert_errors,
            "packets_rejected": run.rx.rx_packets_rejected,
            "interruption_count": run.rx.rx_interruption_count,
            "interruption_duration_ms": run.rx.rx_interruption_duration_ms,
        },
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
        let r = run::run_receiver(&p, &reference, &spans, &trace, 1000, run::SendModel::Direct)
            .expect("run");
        let m = measure(&p, &r, None, &gf.analysis, gf.corpus.clips.len());
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
        let again = run::run_receiver(&p, &reference, &spans, &trace, 1000, run::SendModel::Direct)
            .expect("second run");
        assert!(
            again.output == r.output,
            "receiver output is not deterministic"
        );
    }
}
