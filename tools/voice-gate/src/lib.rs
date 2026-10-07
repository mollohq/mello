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
pub mod rtcp;
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
    let adaptation = run
        .adapt
        .as_ref()
        .map(|a| adaptation_metrics(a, &run.shim.sent_log, &mut m));

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
        "adaptation": adaptation,
        "wall_s": (run.wall_s * 10.0).round() / 10.0,
    });
    Measured {
        metrics: m,
        points,
        extra,
    }
}

fn ms_to_s(ms: i64) -> f64 {
    ms as f64 / 1000.0
}

/// The first time at which the network lost more than `loss` of the packets
/// sent in the `window_ms` before, over at least `min_packets` packets: the
/// truth that the step-down rule of the encoder adaptation reacts to (spec
/// 10 section 4.6: more than 10 % over the last 2 s, at least 25 packets).
/// The window must span `window_ms` of sending: before that, there is no
/// "last 2 s" of the stream.
pub fn first_lossy_window_ms(
    sent_log: &[(i64, bool)],
    window_ms: i64,
    loss: f64,
    min_packets: usize,
) -> Option<i64> {
    let start = sent_log.first()?.0;
    let mut from = 0usize;
    let mut lost = 0usize;
    for (i, &(t, l)) in sent_log.iter().enumerate() {
        lost += usize::from(l);
        while sent_log[from].0 <= t - window_ms {
            lost -= usize::from(sent_log[from].1);
            from += 1;
        }
        let n = i + 1 - from;
        if t - start >= window_ms && n >= min_packets && lost as f64 > loss * n as f64 {
            return Some(t);
        }
    }
    None
}

/// The step-down rule of libmello's SendAdaptation (send_adaptation.hpp).
const DOWN_WINDOW_MS: i64 = 2000;
const DOWN_LOSS: f64 = 0.10;
const DOWN_MIN_PACKETS: usize = 25;

/// Metrics of the send-leg feedback loop (rtcp.rs, spec 10 section 4.6),
/// and its time series for the result file.
fn adaptation_metrics(a: &run::Adaptation, sent_log: &[(i64, bool)], m: &mut Metrics) -> Value {
    let lossy_from = first_lossy_window_ms(sent_log, DOWN_WINDOW_MS, DOWN_LOSS, DOWN_MIN_PACKETS);
    let from = a.first_send_ms.unwrap_or(0);
    let active: Vec<&run::AdaptSample> = a.series.iter().filter(|s| s.t_ms >= from).collect();
    let bitrate: Vec<f64> = active.iter().map(|s| f64::from(s.bitrate_kbps)).collect();
    let loss_perc: Vec<f64> = active.iter().map(|s| f64::from(s.loss_perc)).collect();
    let mean = |v: &[f64]| (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
    let max = |v: &[f64]| v.iter().cloned().reduce(f64::max);
    let min = |v: &[f64]| v.iter().cloned().reduce(f64::min);
    let expected: u64 = a
        .reports
        .iter()
        .map(|r| u64::from(r.feedback.interval_expected))
        .sum();
    let lost: u64 = a
        .reports
        .iter()
        .map(|r| u64::from(r.feedback.interval_lost))
        .sum();
    let rtts: Vec<f64> = a
        .reports
        .iter()
        .filter(|r| r.feedback.rtt_ms >= 0.0)
        .map(|r| f64::from(r.feedback.rtt_ms))
        .collect();
    m.insert(
        "bitrate_drop_s".into(),
        match (a.first_send_ms, a.first_drop_ms) {
            (Some(s), Some(d)) => Some(ms_to_s(d - s)),
            _ => None,
        },
    );
    // Reaction: from the first 2 s in which the network lost more than 10 %
    // of the sent packets to the encoder's first step down. Receiver report
    // cadence (1 s), network delay and the controller window are in it.
    m.insert(
        "bitrate_reaction_s".into(),
        match (lossy_from, a.first_drop_ms) {
            (Some(l), Some(d)) => Some(ms_to_s(d - l)),
            _ => None,
        },
    );
    m.insert("lossy_window_s".into(), lossy_from.map(ms_to_s));
    m.insert("bitrate_min_kbps".into(), min(&bitrate));
    m.insert("bitrate_mean_kbps".into(), mean(&bitrate));
    m.insert("loss_perc_mean".into(), mean(&loss_perc));
    m.insert("loss_perc_max".into(), max(&loss_perc));
    m.insert("rung_changes".into(), Some(f64::from(a.rung_changes)));
    m.insert("rr_reports".into(), Some(a.reports.len() as f64));
    m.insert(
        "rr_loss_pct".into(),
        (expected > 0).then(|| 100.0 * lost as f64 / expected as f64),
    );
    m.insert("rr_rtt_p50_ms".into(), dsp::percentile(&rtts, 50.0));
    m.insert(
        "live_sender".into(),
        Some(if a.live_sender { 1.0 } else { 0.0 }),
    );
    json!({
        "live_sender": a.live_sender,
        "diverged_at_s": a.diverged_ms.map(ms_to_s),
        "first_send_s": a.first_send_ms.map(ms_to_s),
        "first_drop_s": a.first_drop_ms.map(ms_to_s),
        "first_lossy_window_s": lossy_from.map(ms_to_s),
        "top_kbps": a.top_kbps,
        "series": {
            "t_s": a.series.iter().map(|s| ms_to_s(s.t_ms)).collect::<Vec<_>>(),
            "bitrate_kbps": a.series.iter().map(|s| s.bitrate_kbps).collect::<Vec<_>>(),
            "loss_perc": a.series.iter().map(|s| s.loss_perc).collect::<Vec<_>>(),
        },
        "reports": a.reports.iter().map(|r| json!({
            "sent_s": ms_to_s(r.sent_ms),
            "arrive_s": ms_to_s(r.arrive_ms),
            "fraction_lost": (f64::from(r.feedback.fraction_lost) * 1e4).round() / 1e4,
            "cumulative_lost": r.feedback.cumulative_lost,
            "expected": r.feedback.interval_expected,
            "lost": r.feedback.interval_lost,
            "jitter_ms": (f64::from(r.feedback.jitter_ms) * 10.0).round() / 10.0,
            "rtt_ms": (f64::from(r.feedback.rtt_ms) * 10.0).round() / 10.0,
            "bitrate_kbps": r.bitrate_kbps,
            "loss_perc": r.loss_perc,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod adaptation_tests {
    use super::first_lossy_window_ms;

    #[test]
    fn first_lossy_window_slides_over_send_times() {
        // 50 packets a second; 4 of every 20 lost from 3 s on (20 %).
        let log: Vec<(i64, bool)> = (0..300)
            .map(|i| {
                let t = i64::from(i) * 20;
                (t, t >= 3000 && i % 20 < 4)
            })
            .collect();
        let at = first_lossy_window_ms(&log, 2000, 0.10, 25).expect("lossy window");
        // Over 2 s (100 packets) more than 10 lost: the 11th loss after 3 s.
        assert!((3000..4500).contains(&at), "first lossy window at {at}");
        assert_eq!(first_lossy_window_ms(&log[..150], 2000, 0.10, 25), None);
        // Too few packets in the window never count.
        let sparse = vec![(0, true), (1000, true), (1500, false)];
        assert_eq!(first_lossy_window_ms(&sparse, 2000, 0.10, 25), None);
        // Loss in the first 2 s of sending counts only once 2 s have passed.
        let early: Vec<(i64, bool)> = (0..150)
            .map(|i| (1000 + i64::from(i) * 20, i < 10))
            .collect();
        assert_eq!(first_lossy_window_ms(&early, 2000, 0.10, 25), None);
        let early_heavy: Vec<(i64, bool)> = (0..150)
            .map(|i| (1000 + i64::from(i) * 20, i < 30))
            .collect();
        assert_eq!(
            first_lossy_window_ms(&early_heavy, 2000, 0.10, 25),
            Some(3000)
        );
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
