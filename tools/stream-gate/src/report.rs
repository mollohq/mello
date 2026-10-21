//! Metrics table, baseline delta, gates.

use crate::profile::Gates;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Metric key, label, better direction (+1 higher, -1 lower, 0 neither).
/// The order is the print order.
pub const METRICS: &[(&str, &str, i8)] = &[
    ("vmaf_mean", "VMAF mean (vmaf_v0.6.1)", 1),
    ("vmaf_p5", "VMAF 5th percentile", 1),
    ("vmaf_min", "VMAF min", 1),
    ("psnr_y_mean", "PSNR luma mean (dB)", 1),
    ("ssim_y_mean", "SSIM luma mean", 1),
    ("fps_delivered", "Delivered fps (new frames)", 1),
    ("frames_presented", "Frames presented", 0),
    ("frames_new", "New frames presented", 0),
    ("frames_duplicated", "Duplicated presents", -1),
    ("frames_dropped", "Dropped frames (never presented)", -1),
    ("frames_unreadable", "Presents with an unreadable code", -1),
    ("freeze_count", "Freezes (present gap > 150 ms)", -1),
    ("freeze_ms", "Freeze time (ms)", -1),
    ("freeze_pct", "Freeze time (% of the run)", -1),
    ("ttff_ms", "Time to first frame (ms)", -1),
    ("frame_interval_p95_ms", "Frame interval p95 (ms)", -1),
    ("g2g_p50_ms", "Glass-to-glass p50 (ms)", -1),
    ("g2g_p95_ms", "Glass-to-glass p95 (ms)", -1),
    ("g2g_max_ms", "Glass-to-glass max (ms)", -1),
    (
        "outage_recovery_ms",
        "First new frame after the outage end (ms)",
        -1,
    ),
    (
        "other_viewer_freeze_ms",
        "Freeze time of the first viewer (ms)",
        -1,
    ),
    ("host_wire_kbps", "Host uplink, all datagrams (kbps)", 0),
    (
        "host_media_kbps",
        "Host uplink, H.264 + FEC + retransmits (kbps)",
        0,
    ),
    ("fec_overhead_pct", "Host FEC bytes / video bytes (%)", -1),
    (
        "rtx_overhead_pct",
        "Host retransmit bytes / video bytes (%)",
        -1,
    ),
    (
        "viewer_rtx_pct",
        "SFU to viewer retransmit bytes / video bytes (%)",
        -1,
    ),
    ("host_leg_lost", "Host leg datagrams lost by the shim", 0),
    (
        "viewer_leg_lost",
        "Viewer leg datagrams lost by the shim",
        0,
    ),
    ("cap_kbps", "Cap on the host uplink (kbps)", 0),
    (
        "cap_send_kbps",
        "Host send rate in the cap, from 2 s after its start (kbps)",
        0,
    ),
    ("cap_overshoot_pct", "Host send rate over the cap (%)", -1),
    ("cap_queue_drops", "Datagrams the cap queue dropped", -1),
    (
        "bitrate_reaction_s",
        "Send rate at or under the cap after (s)",
        -1,
    ),
    ("rx_nacks", "Viewer NACK sequences sent", 0),
    ("rx_pli", "Viewer PLI requests", -1),
    ("rx_fec_recovered", "Viewer packets recovered by FEC", 0),
    ("rx_missing", "Viewer missing sequences detected", 0),
    ("rx_repaired", "Viewer packets repaired by NACK", 0),
    (
        "rx_gate_entries",
        "Viewer receiver gate entries (waits for an IDR)",
        -1,
    ),
    (
        "rx_gate_dropped_aus",
        "Access units the viewer gate dropped",
        -1,
    ),
    (
        "viewer_backlog_drops",
        "Delta AUs the viewer backlog guard dropped",
        -1,
    ),
    ("tx_pli", "PLI requests at the host", 0),
    ("host_keyframes", "Keyframes the host encoded", 0),
    (
        "host_frames_late",
        "Reference frames handed over more than 2 ms late",
        -1,
    ),
];

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

pub fn fmt_num(v: Option<f64>) -> String {
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
        _ => String::new(),
    }
}

pub fn metric_of(doc: &Value, profile: &str, metric: &str) -> Option<f64> {
    doc.get("profiles")?
        .get(profile)?
        .get("metrics")?
        .get(metric)?
        .as_f64()
}

fn check(profile: &str, g: &crate::profile::Gate, v: Option<f64>) -> GateResult {
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
    let pass = v.is_some_and(|x| g.min.is_none_or(|m| x >= m) && g.max.is_none_or(|m| x <= m));
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

/// The lane's gates, then the two rules against the baseline: VMAF may not
/// drop by more than `vmaf_max_drop`, freeze time may not rise by more than
/// the measured spread of its profile.
pub fn evaluate(
    lane: &str,
    gates: &Gates,
    results: &BTreeMap<String, Metrics>,
    vmaf_scored: bool,
    baseline: Option<&Value>,
) -> (Vec<GateResult>, Vec<String>) {
    let mut out = Vec::new();
    let mut notes = Vec::new();
    if let Some(per) = gates.lanes.get(lane) {
        for (profile, list) in per {
            let Some(m) = results.get(profile) else {
                continue;
            };
            for g in list {
                if g.metric.starts_with("vmaf") && !vmaf_scored {
                    notes.push(format!(
                        "{profile} {}: VMAF not scored; gate not checked.",
                        g.metric
                    ));
                    continue;
                }
                out.push(check(profile, g, m.get(&g.metric).cloned().flatten()));
            }
        }
    } else {
        notes.push(format!(
            "gates.json has no lane '{lane}': only the baseline rules run."
        ));
    }
    let Some(base) = baseline else {
        notes.push("No baseline: the VMAF-drop and freeze-rise rules did not run.".into());
        return (out, notes);
    };
    for (name, m) in results {
        if vmaf_scored {
            if let (Some(c), Some(b)) = (
                m.get("vmaf_mean").cloned().flatten(),
                metric_of(base, name, "vmaf_mean"),
            ) {
                out.push(GateResult {
                    profile: name.clone(),
                    metric: "vmaf drop vs baseline".into(),
                    value: Some(b - c),
                    rule: format!("<= {}", gates.vmaf_max_drop),
                    enforced: true,
                    pass: b - c <= gates.vmaf_max_drop + 1e-9,
                    note: "plans/streaming-quality.md stage 1".into(),
                });
            }
        }
        if let (Some(c), Some(b)) = (
            m.get("freeze_ms").cloned().flatten(),
            metric_of(base, name, "freeze_ms"),
        ) {
            let rise = gates.freeze_ms_max_rise.get(name).cloned().unwrap_or(0.0);
            out.push(GateResult {
                profile: name.clone(),
                metric: "freeze ms rise vs baseline".into(),
                value: Some(c - b),
                rule: format!("<= {}", fmt_num(Some(rise))),
                enforced: true,
                pass: c - b <= rise + 1e-9,
                note: "the run-to-run spread of the profile at the baseline".into(),
            });
        }
    }
    (out, notes)
}

/// Columns of the summary table.
const SUMMARY: &[(&str, &str)] = &[
    ("vmaf_mean", "VMAF"),
    ("vmaf_p5", "VMAF p5"),
    ("psnr_y_mean", "PSNR"),
    ("ssim_y_mean", "SSIM"),
    ("fps_delivered", "fps"),
    ("frames_dropped", "dropped"),
    ("freeze_count", "freezes"),
    ("freeze_ms", "freeze ms"),
    ("ttff_ms", "TTFF ms"),
    ("frame_interval_p95_ms", "int p95"),
    ("g2g_p50_ms", "g2g p50"),
    ("g2g_p95_ms", "g2g p95"),
    ("host_wire_kbps", "host kbps"),
    ("fec_overhead_pct", "FEC %"),
    ("rtx_overhead_pct", "RTX %"),
    ("viewer_rtx_pct", "v RTX %"),
    ("rx_pli", "PLI"),
    ("bitrate_reaction_s", "react s"),
];

pub fn summary_table(
    order: &[String],
    results: &BTreeMap<String, Metrics>,
    baseline: Option<&Value>,
) -> String {
    let mut s = String::from("| profile |");
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
            let cell = match baseline.map(|b| metric_of(b, name, k)) {
                Some(base) => {
                    let d = fmt_delta(cur, base);
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
            "  {:<58} {:>10} {:>10} {:>10}",
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
                "  {:<58} {:>10} {:>10} {:>10}",
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
    let mut s = String::from(
        "| profile | metric | value | rule | result | note |\n|---|---|---:|---|---|---|\n",
    );
    for g in gates {
        let r = match (g.pass, g.enforced) {
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
            r,
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
                    "profile": g.profile, "metric": g.metric, "value": g.value,
                    "rule": g.rule, "enforced": g.enforced, "pass": g.pass, "note": g.note,
                })
            })
            .collect(),
    )
}

pub fn metrics_json(m: &Metrics) -> Value {
    let mut map = Map::new();
    for (k, _, _) in METRICS {
        if let Some(v) = m.get(*k) {
            map.insert((*k).to_owned(), v.map_or(Value::Null, |x| json!(round4(x))));
        }
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Gate, Gates};

    fn gates() -> Gates {
        let mut per = BTreeMap::new();
        per.insert(
            "clean".to_owned(),
            vec![Gate {
                metric: "fps_delivered".into(),
                min: Some(55.0),
                max: None,
                enforced: true,
                note: String::new(),
            }],
        );
        let mut lanes = BTreeMap::new();
        lanes.insert("cpu".to_owned(), per);
        let mut rise = BTreeMap::new();
        rise.insert("clean".to_owned(), 50.0);
        Gates {
            vmaf_max_drop: 1.0,
            freeze_ms_max_rise: rise,
            lanes,
        }
    }

    fn run(fps: f64, vmaf: f64, freeze: f64) -> BTreeMap<String, Metrics> {
        let mut m = Metrics::new();
        m.insert("fps_delivered".into(), Some(fps));
        m.insert("vmaf_mean".into(), Some(vmaf));
        m.insert("freeze_ms".into(), Some(freeze));
        let mut r = BTreeMap::new();
        r.insert("clean".to_owned(), m);
        r
    }

    #[test]
    fn a_vmaf_drop_over_one_point_fails() {
        let base = json!({"profiles":{"clean":{"metrics":{"vmaf_mean":90.0,"freeze_ms":0.0}}}});
        let (ok, _) = evaluate("cpu", &gates(), &run(59.0, 89.2, 0.0), true, Some(&base));
        assert!(ok.iter().all(|g| g.pass), "{ok:?}");
        let (bad, _) = evaluate("cpu", &gates(), &run(59.0, 88.9, 0.0), true, Some(&base));
        assert!(bad
            .iter()
            .any(|g| !g.pass && g.metric.starts_with("vmaf drop")));
    }

    #[test]
    fn a_freeze_rise_over_the_spread_fails() {
        let base = json!({"profiles":{"clean":{"metrics":{"vmaf_mean":90.0,"freeze_ms":100.0}}}});
        let (ok, _) = evaluate("cpu", &gates(), &run(59.0, 90.0, 150.0), true, Some(&base));
        assert!(ok.iter().all(|g| g.pass));
        let (bad, _) = evaluate("cpu", &gates(), &run(59.0, 90.0, 151.0), true, Some(&base));
        assert!(bad
            .iter()
            .any(|g| !g.pass && g.metric.starts_with("freeze")));
        let (low_fps, _) = evaluate("cpu", &gates(), &run(50.0, 90.0, 0.0), true, Some(&base));
        assert!(low_fps
            .iter()
            .any(|g| !g.pass && g.metric == "fps_delivered"));
    }
}
