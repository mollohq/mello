//! Objective speech quality (MOS-LQO) through an external scorer.
//!
//! The scorer is `scripts/voice-gate-score.py`: PESQ wideband (ITU-T
//! P.862.2) from the `pesq` Python package. The harness cuts one aligned
//! reference/output pair per corpus clip, resamples both to 16 kHz, and
//! asks the script to score all pairs in one call. When Python or the
//! package is absent, the scorer is reported as unavailable and every
//! structural metric is still produced.

use crate::analysis::{lag_at, LagPoint};
use crate::dsp;
use crate::run::ClipSpan;
use crate::wav;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SCORE_RATE: u32 = 16_000;

/// One aligned pair to score.
pub struct Segment {
    pub id: String,
    pub profile: String,
    pub ref_path: PathBuf,
    pub deg_path: PathBuf,
}

/// Scorer identity and per-segment results.
pub struct Scores {
    pub available: bool,
    /// For example "pesq-wb (ITU-T P.862.2), pesq 0.0.4".
    pub scorer: String,
    pub detail: Value,
    /// (segment id, MOS-LQO or None when the scorer failed on it).
    pub results: Vec<(String, Option<f64>)>,
}

fn to_16k(x: &[i16]) -> Vec<i16> {
    let taps = dsp::lowpass_taps(7_400.0 / 48_000.0, 97);
    dsp::decimate(x, 3, &taps)
        .into_iter()
        .map(|v| v.round().clamp(-32768.0, 32767.0) as i16)
        .collect()
}

/// Write one 16 kHz reference/output pair per clip. The output segment
/// starts at the clip position plus the smallest lag measured inside the
/// clip and ends at the clip end plus the largest one.
pub fn write_segments(
    dir: &Path,
    profile: &str,
    reference: &[i16],
    output: &[i16],
    clips: &[ClipSpan],
    points: &[LagPoint],
) -> Result<Vec<Segment>, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut segs = Vec::new();
    for (i, c) in clips.iter().enumerate() {
        let inside: Vec<f64> = points
            .iter()
            .filter(|p| {
                let k = (p.t_s * 48_000.0) as usize;
                k >= c.start && k < c.start + c.len
            })
            .map(|p| p.lag_ms)
            .collect();
        // The output window covers the clip at every lag measured inside
        // it, so delay growth within a clip does not cut words off.
        let (lag_lo, lag_hi) = match (
            dsp::percentile(&inside, 0.0),
            dsp::percentile(&inside, 100.0),
        ) {
            (Some(lo), Some(hi)) => ((lo * 48.0).round() as usize, (hi * 48.0).round() as usize),
            _ => match lag_at(points, c.start + c.len / 2) {
                Some(l) => (l, l),
                None => continue,
            },
        };
        let r = &reference[c.start..c.start + c.len];
        let len = c.len + (lag_hi - lag_lo);
        let mut d = vec![0i16; len];
        let from = c.start + lag_lo;
        if from < output.len() {
            let n = (output.len() - from).min(len);
            d[..n].copy_from_slice(&output[from..from + n]);
        }
        let id = format!("{profile}-{i:03}-{}", c.name.trim_end_matches(".wav"));
        let ref_path = dir.join(format!("{id}-ref.wav"));
        let deg_path = dir.join(format!("{id}-deg.wav"));
        wav::write(&ref_path, SCORE_RATE, &to_16k(r))?;
        wav::write(&deg_path, SCORE_RATE, &to_16k(&d))?;
        segs.push(Segment {
            id,
            profile: profile.to_owned(),
            ref_path,
            deg_path,
        });
    }
    Ok(segs)
}

/// Python interpreter for the scorer: `VOICE_GATE_PYTHON`, else `python3`,
/// else `python`.
fn python_candidates() -> Vec<String> {
    match std::env::var("VOICE_GATE_PYTHON") {
        Ok(p) if !p.is_empty() => vec![p],
        _ => vec!["python3".into(), "python".into()],
    }
}

fn unavailable(reason: String) -> Scores {
    Scores {
        available: false,
        scorer: "none".into(),
        detail: json!({ "available": false, "reason": reason }),
        results: Vec::new(),
    }
}

/// Score all segments in one scorer call.
pub fn score(script: &Path, manifest_dir: &Path, segments: &[Segment]) -> Scores {
    let manifest = manifest_dir.join("segments.json");
    let list: Vec<Value> = segments
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "ref": s.ref_path.to_string_lossy(),
                "deg": s.deg_path.to_string_lossy(),
            })
        })
        .collect();
    let body = json!({ "sample_rate": SCORE_RATE, "segments": list });
    if let Err(e) = std::fs::write(&manifest, body.to_string()) {
        return unavailable(format!("cannot write {}: {e}", manifest.display()));
    }
    let mut last_err = String::from("no python interpreter found");
    for py in python_candidates() {
        let out = match Command::new(&py).arg(script).arg(&manifest).output() {
            Ok(o) => o,
            Err(e) => {
                last_err = format!("{py}: {e}");
                continue;
            }
        };
        if !out.status.success() {
            last_err = format!(
                "{py} exited with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            continue;
        }
        let v: Value = match serde_json::from_slice(&out.stdout) {
            Ok(v) => v,
            Err(e) => {
                last_err = format!("{py}: bad scorer output: {e}");
                continue;
            }
        };
        if v.get("available").and_then(Value::as_bool) != Some(true) {
            last_err = format!(
                "{py}: {}",
                v.get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("scorer unavailable")
            );
            continue;
        }
        let results = v
            .get("results")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|r| {
                        (
                            r.get("id").and_then(Value::as_str).unwrap_or("").to_owned(),
                            r.get("mos").and_then(Value::as_f64),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let scorer = v
            .get("scorer")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let mut detail = v.clone();
        if let Some(m) = detail.as_object_mut() {
            m.remove("results");
        }
        return Scores {
            available: true,
            scorer,
            detail,
            results,
        };
    }
    unavailable(last_err)
}
