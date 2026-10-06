//! Metrics table, baseline delta, gates, and the result files.

use crate::profile::{Gate, GateFile};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Metric key, label, and which direction is better (+1 higher, -1 lower,
/// 0 neither). The order is the print order.
pub const METRICS: &[(&str, &str, i8)] = &[
    ("mos", "MOS-LQO, mean over clips", 1),
    ("mos_min", "MOS-LQO, worst clip", 1),
    ("latency_p50_ms", "Mouth-to-ear delay p50 (ms)", -1),
    ("latency_p95_ms", "Mouth-to-ear delay p95 (ms)", -1),
    (
        "latency_start_ms",
        "Delay over the first corpus loop (ms)",
        -1,
    ),
    ("latency_end_ms", "Delay over the last corpus loop (ms)", -1),
    (
        "latency_growth_ms",
        "Delay growth, last - first loop (ms)",
        -1,
    ),
    (
        "latency_clip_growth_ms",
        "Delay growth inside a clip, median (ms)",
        -1,
    ),
    ("latency_max_ms", "Delay max (ms)", -1),
    ("event_before_ms", "Delay before the event (ms)", -1),
    ("event_peak_ms", "Delay peak from the event on (ms)", -1),
    ("recovery_s", "Back to the pre-event delay after (s)", -1),
    ("conceal_per_lost", "Concealment frames per lost frame", 0),
    ("lost_frames", "Lost frames (network + receiver drops)", 0),
    ("shim_lost", "Packets lost in the network", 0),
    ("conceal_frames", "Concealment frames for losses", 0),
    ("conceal_missing_plc", "  PLC for a lost packet", 0),
    ("conceal_gap_fec", "  FEC for a lost packet", 0),
    ("conceal_gap_plc", "  PLC for an undecodable packet", 0),
    (
        "fill_plc_frames",
        "PLC frames filling an empty playout buffer",
        -1,
    ),
    ("underruns", "Mixer underruns (no audio at all)", -1),
    (
        "late_underruns",
        "Late playout underruns (packets waiting)",
        -1,
    ),
    ("decode_errors", "Decode errors", -1),
    ("packets_sent", "Packets sent", 0),
    ("packets_fed", "Packets fed to the receiver", 0),
    ("rx_dropped_late", "Receiver drops: late", -1),
    (
        "rx_dropped_overflow",
        "Receiver drops: jitter buffer full",
        -1,
    ),
    ("rx_jitter_resets", "Jitter buffer resets", -1),
    ("dropout_ms", "Speech dropout (ms)", -1),
    (
        "wrap_dropout_ms",
        "Speech dropout around the RTP wrap (ms)",
        -1,
    ),
    (
        "playout_buffer_p50_ms",
        "Decoded playout buffer p50 (ms)",
        -1,
    ),
    (
        "playout_buffer_max_ms",
        "Decoded playout buffer max (ms)",
        -1,
    ),
    ("jitter_target_p50_ms", "Jitter target delay p50 (ms)", 0),
];

/// Metrics of one profile run. None = not applicable or not measured.
pub type Metrics = BTreeMap<String, Option<f64>>;

#[derive(Clone, Debug)]
pub struct GateResult {
    pub profile: String,
    pub metric: String,
    pub value: Option<f64>,
    pub rule: String,
    pub enforced: bool,
    pub pass: bool,
    pub note: String,
}

fn fmt_num(v: Option<f64>) -> String {
    match v {
        None => "-".into(),
        Some(x) if x.abs() >= 100.0 || x.fract() == 0.0 => format!("{x:.0}"),
        Some(x) if x.abs() >= 10.0 => format!("{x:.1}"),
        Some(x) => format!("{x:.2}"),
    }
}

fn round4(x: f64) -> f64 {
    (x * 1e4).round() / 1e4
}

fn fmt_delta(cur: Option<f64>, base: Option<f64>) -> String {
    match (cur, base) {
        (Some(c), Some(b)) => {
            // Result files keep 4 decimals; compare at that precision.
            let d = round4(c) - round4(b);
            if d.abs() < 1e-9 {
                "0".into()
            } else if d.abs() >= 10.0 {
                format!("{d:+.0}")
            } else if d.abs() >= 0.01 {
                format!("{d:+.2}")
            } else {
                format!("{d:+.4}")
            }
        }
        _ => "".into(),
    }
}

/// Metric value of a profile in a result or baseline JSON.
pub fn metric_of(doc: &Value, profile: &str, metric: &str) -> Option<f64> {
    doc.get("profiles")?
        .get(profile)?
        .get("metrics")?
        .get(metric)?
        .as_f64()
}

fn check(g: &Gate, profile: &str, v: Option<f64>) -> GateResult {
    let mut rule = String::new();
    if let Some(m) = g.min {
        let _ = write!(rule, ">= {}", fmt_num(Some(m)));
    }
    if let Some(m) = g.max {
        if !rule.is_empty() {
            rule.push_str(", ");
        }
        let _ = write!(rule, "<= {}", fmt_num(Some(m)));
    }
    let pass = match v {
        None => false,
        Some(x) => g.min.is_none_or(|m| x >= m) && g.max.is_none_or(|m| x <= m),
    };
    GateResult {
        profile: profile.to_owned(),
        metric: g.metric.clone(),
        value: v,
        rule,
        enforced: g.enforced,
        pass,
        note: g.note.clone(),
    }
}

/// Evaluate the profile gates and the MOS-drop gate against the baseline.
/// MOS gates are skipped when no scorer ran, and the MOS-drop gate is
/// skipped when the baseline used a different scorer.
pub fn evaluate(
    gf: &GateFile,
    results: &BTreeMap<String, Metrics>,
    scored: bool,
    baseline: Option<&Value>,
    scorer: &str,
) -> (Vec<GateResult>, Vec<String>) {
    let mut out = Vec::new();
    let mut notes = Vec::new();
    for p in &gf.profiles {
        let Some(m) = results.get(&p.name) else {
            continue;
        };
        for g in &p.gates {
            if g.metric.starts_with("mos") && !scored {
                continue;
            }
            out.push(check(g, &p.name, m.get(&g.metric).cloned().flatten()));
        }
    }
    if let Some(base) = baseline {
        let base_scorer = base
            .get("scorer")
            .and_then(|s| s.get("scorer"))
            .and_then(Value::as_str)
            .unwrap_or("none");
        if !scored {
            notes.push("MOS not scored (no scorer): the MOS-drop gate did not run.".into());
        } else if base_scorer != scorer {
            notes.push(format!(
                "Scorer differs from the baseline ('{scorer}' vs '{base_scorer}'): MOS is not compared."
            ));
        } else {
            for (name, m) in results {
                let cur = m.get("mos").cloned().flatten();
                let b = metric_of(base, name, "mos");
                let (Some(c), Some(b)) = (cur, b) else {
                    continue;
                };
                out.push(GateResult {
                    profile: name.clone(),
                    metric: "mos drop vs baseline".into(),
                    value: Some(b - c),
                    rule: format!("<= {}", gf.mos_max_drop),
                    enforced: true,
                    pass: b - c <= gf.mos_max_drop + 1e-9,
                    note: "plans/voice-quality.md stage 7".into(),
                });
            }
        }
    }
    (out, notes)
}

/// Columns of the summary table: (metric, short header).
const SUMMARY: &[(&str, &str)] = &[
    ("mos", "MOS"),
    ("latency_p50_ms", "delay p50"),
    ("latency_p95_ms", "delay p95"),
    ("latency_start_ms", "start"),
    ("latency_end_ms", "end"),
    ("latency_growth_ms", "growth"),
    ("latency_clip_growth_ms", "in-clip growth"),
    ("recovery_s", "recover s"),
    ("conceal_per_lost", "conceal/lost"),
    ("fill_plc_frames", "fill PLC"),
    ("underruns", "underruns"),
    ("late_underruns", "late ur"),
    ("decode_errors", "dec err"),
    ("packets_fed", "fed"),
    ("rx_dropped_late", "rx late"),
    ("rx_jitter_resets", "resets"),
    ("dropout_ms", "dropout ms"),
];

/// Summary table, Markdown. With a baseline each cell is "value (delta)".
pub fn summary_table(
    order: &[String],
    results: &BTreeMap<String, Metrics>,
    baseline: Option<&Value>,
) -> String {
    let mut s = String::new();
    s.push_str("| profile |");
    for (_, h) in SUMMARY {
        let _ = write!(s, " {h} |");
    }
    s.push_str("\n|---|");
    for _ in SUMMARY {
        s.push_str("---:|");
    }
    s.push('\n');
    for name in order {
        let Some(m) = results.get(name) else { continue };
        let _ = write!(s, "| {name} |");
        for (k, _) in SUMMARY {
            let cur = m.get(*k).cloned().flatten();
            let cell = match baseline {
                Some(b) => {
                    let d = fmt_delta(cur, metric_of(b, name, k));
                    if d.is_empty() || d == "0" {
                        fmt_num(cur)
                    } else {
                        format!("{} ({d})", fmt_num(cur))
                    }
                }
                None => fmt_num(cur),
            };
            let _ = write!(s, " {cell} |");
        }
        s.push('\n');
    }
    s
}

/// Every metric of every profile against the baseline.
pub fn detail_table(
    order: &[String],
    results: &BTreeMap<String, Metrics>,
    baseline: Option<&Value>,
) -> String {
    let mut s = String::new();
    for name in order {
        let Some(m) = results.get(name) else { continue };
        let _ = writeln!(s, "\n{name}");
        let _ = writeln!(
            s,
            "  {:<44} {:>10} {:>10} {:>10}",
            "metric", "now", "baseline", "delta"
        );
        for (k, label, _) in METRICS {
            let cur = m.get(*k).cloned().flatten();
            let base = baseline.and_then(|b| metric_of(b, name, k));
            if cur.is_none() && base.is_none() {
                continue;
            }
            let _ = writeln!(
                s,
                "  {:<44} {:>10} {:>10} {:>10}",
                label,
                fmt_num(cur),
                fmt_num(base),
                fmt_delta(cur, base)
            );
        }
    }
    s
}

pub fn gate_table(gates: &[GateResult]) -> String {
    let mut s = String::new();
    s.push_str("| profile | metric | value | rule | result | note |\n|---|---|---:|---|---|---|\n");
    for g in gates {
        let result = match (g.pass, g.enforced) {
            (true, _) => "pass",
            (false, true) => "FAIL",
            (false, false) => "fail (informational)",
        };
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {} |",
            g.profile,
            g.metric,
            fmt_num(g.value),
            g.rule,
            result,
            g.note
        );
    }
    s
}

pub fn gates_json(gates: &[GateResult]) -> Value {
    Value::Array(
        gates
            .iter()
            .map(|g| {
                json!({
                    "profile": g.profile,
                    "metric": g.metric,
                    "value": g.value,
                    "rule": g.rule,
                    "enforced": g.enforced,
                    "pass": g.pass,
                    "note": g.note,
                })
            })
            .collect(),
    )
}

pub fn metrics_json(m: &Metrics) -> Value {
    let mut map = Map::new();
    for (k, _, _) in METRICS {
        if let Some(v) = m.get(*k) {
            map.insert(
                (*k).to_owned(),
                v.map_or(Value::Null, |x| json!((x * 1e4).round() / 1e4)),
            );
        }
    }
    Value::Object(map)
}

/// UTC "YYYY-MM-DDTHH:MM:SSZ" from the system clock.
pub fn utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_format_is_sane() {
        let s = utc_now();
        assert_eq!(s.len(), 20);
        assert!(s.starts_with("20"));
        assert!(s.ends_with('Z'));
    }

    #[test]
    fn gate_check_min_max_and_missing() {
        let g = Gate {
            metric: "x".into(),
            min: Some(0.95),
            max: Some(1.05),
            enforced: true,
            note: String::new(),
        };
        assert!(check(&g, "p", Some(1.0)).pass);
        assert!(!check(&g, "p", Some(2.0)).pass);
        assert!(!check(&g, "p", None).pass);
    }
}
