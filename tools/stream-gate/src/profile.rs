//! The gate definition: clips, lanes, impairment profiles
//! (`benchmarks/baselines/stream/profiles.json`) and gates
//! (`benchmarks/baselines/stream/gates.json`).

use crate::clip::ClipSpec;
use crate::netmodel::{Cap, Gilbert, Leg, SHIM_VERSION};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// A clip as `profiles.json` freezes it.
#[derive(Clone, Debug)]
pub struct ClipEntry {
    pub spec: ClipSpec,
    /// FNV-1a 64 of the rendered file; empty until frozen.
    pub fnv1a: String,
}

/// A lane: which clip, which encoder and decoder, which preset bitrate.
#[derive(Clone, Debug)]
pub struct Lane {
    pub name: String,
    pub clip: String,
    /// MELLO_SOFT_ENCODER=1 and MELLO_SOFT_DECODER=1.
    pub soft: bool,
    pub bitrate_kbps: u32,
    pub fps: u32,
    pub preset: String,
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub name: String,
    pub description: String,
    pub duration_s: f64,
    pub seed: u64,
    pub host: Leg,
    pub viewer: Leg,
    /// A second viewer joins at this profile time; the profile is measured
    /// on that viewer.
    pub late_viewer_s: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct Analysis {
    /// A present gap over this is a freeze.
    pub freeze_gap_ms: f64,
    /// VMAF on every n-th presented frame (1 = all).
    pub vmaf_subsample: u32,
}

#[derive(Clone, Debug)]
pub struct GateFile {
    pub clips: BTreeMap<String, ClipEntry>,
    pub lanes: BTreeMap<String, Lane>,
    pub analysis: Analysis,
    pub profiles: Vec<Profile>,
    /// FNV-1a of every frozen input: the file without descriptions and
    /// comments. A baseline is comparable only with runs of the same value.
    pub inputs_fingerprint: String,
}

fn f(v: &Value, key: &str) -> Result<f64, String> {
    v.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("missing number '{key}'"))
}

fn f_or(v: &Value, key: &str, d: f64) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(d)
}

fn s(v: &Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("missing string '{key}'"))
}

pub fn parse_leg(v: Option<&Value>, default_base_ms: f64) -> Result<Leg, String> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(Leg {
            base_delay_ms: default_base_ms,
            ..Default::default()
        });
    };
    let burst = match v.get("burst") {
        Some(b) if !b.is_null() => Some(Gilbert {
            p_enter: f(b, "p_enter")?,
            p_exit: f(b, "p_exit")?,
            loss_in_bad: f(b, "loss_in_bad")?,
        }),
        _ => None,
    };
    let outage = match v.get("outage") {
        Some(o) if !o.is_null() => Some((f(o, "start_s")?, f(o, "dur_s")?)),
        _ => None,
    };
    let cap = match v.get("cap") {
        Some(c) if !c.is_null() => Some(Cap {
            start_s: f(c, "start_s")?,
            dur_s: f(c, "dur_s")?,
            rate_fraction: f(c, "rate_fraction")?,
            rate_kbps: 0.0,
            queue_ms: f(c, "queue_ms")?,
            overhead_bytes: f_or(c, "overhead_bytes", 28.0),
        }),
        _ => None,
    };
    Ok(Leg {
        base_delay_ms: f_or(v, "base_delay_ms", default_base_ms),
        jitter_sigma_ms: f_or(v, "jitter_sigma_ms", 0.0),
        loss_random: f_or(v, "loss_random", 0.0),
        burst,
        spikes_per_min: f_or(v, "spikes_per_min", 0.0),
        spike_ms: f_or(v, "spike_ms", 0.0),
        reorder_prob: f_or(v, "reorder_prob", 0.0),
        reorder_extra_ms: f_or(v, "reorder_extra_ms", 0.0),
        outage,
        cap,
        drift_ppm: f_or(v, "drift_ppm", 0.0),
    })
}

/// Read and validate `profiles.json`.
pub fn load(path: &Path) -> Result<GateFile, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let root: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    let shim = root.get("shim").and_then(Value::as_u64).unwrap_or(0);
    if shim != SHIM_VERSION {
        return Err(format!(
            "{}: written for shim version {shim}, this harness implements version {SHIM_VERSION}",
            path.display()
        ));
    }
    let clip_version = root
        .get("clip_version")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if clip_version != u64::from(crate::clip::CLIP_VERSION) {
        return Err(format!(
            "{}: written for clip version {clip_version}, this generator is version {}",
            path.display(),
            crate::clip::CLIP_VERSION
        ));
    }
    let mut clips = BTreeMap::new();
    for (name, c) in root
        .get("clips")
        .and_then(Value::as_object)
        .ok_or("missing 'clips'")?
    {
        clips.insert(
            name.clone(),
            ClipEntry {
                spec: ClipSpec {
                    width: f(c, "width")? as u32,
                    height: f(c, "height")? as u32,
                    fps: f(c, "fps")? as u32,
                    frames: f(c, "frames")? as u32,
                    seed: f(c, "seed")? as u64,
                },
                fnv1a: c
                    .get("fnv1a")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            },
        );
    }
    let mut lanes = BTreeMap::new();
    for (name, l) in root
        .get("lanes")
        .and_then(Value::as_object)
        .ok_or("missing 'lanes'")?
    {
        let clip = s(l, "clip")?;
        if !clips.contains_key(&clip) {
            return Err(format!("lane '{name}': unknown clip '{clip}'"));
        }
        lanes.insert(
            name.clone(),
            Lane {
                name: name.clone(),
                clip,
                soft: l.get("soft").and_then(Value::as_bool).unwrap_or(false),
                bitrate_kbps: f(l, "bitrate_kbps")? as u32,
                fps: f(l, "fps")? as u32,
                preset: s(l, "preset")?,
            },
        );
    }
    let a = root.get("analysis").ok_or("missing 'analysis'")?;
    let analysis = Analysis {
        freeze_gap_ms: f(a, "freeze_gap_ms")?,
        vmaf_subsample: f_or(a, "vmaf_subsample", 1.0).max(1.0) as u32,
    };
    let base_ms = f_or(&root, "base_delay_ms", 10.0);
    let mut profiles = Vec::new();
    for p in root
        .get("profiles")
        .and_then(Value::as_array)
        .ok_or("missing 'profiles'")?
    {
        let name = s(p, "name")?;
        let ctx = |e: String| format!("profile '{name}': {e}");
        profiles.push(Profile {
            description: s(p, "description").map_err(ctx)?,
            duration_s: f(p, "duration_s").map_err(ctx)?,
            seed: f(p, "seed").map_err(ctx)? as u64,
            host: parse_leg(p.get("host_leg"), base_ms).map_err(ctx)?,
            viewer: parse_leg(p.get("viewer_leg"), base_ms).map_err(ctx)?,
            late_viewer_s: p.get("late_viewer_s").and_then(Value::as_f64),
            name,
        });
    }
    Ok(GateFile {
        clips,
        lanes,
        analysis,
        profiles,
        inputs_fingerprint: fingerprint(&root),
    })
}

/// FNV-1a 64 of the canonical form: sorted keys, without descriptions,
/// notes and `_` comments. The clip hashes are inside it.
pub fn fingerprint(root: &Value) -> String {
    let mut text = String::new();
    canonical(root, &mut text);
    format!(
        "{:016x}",
        crate::clip::fnv1a(crate::clip::FNV_INIT, text.as_bytes())
    )
}

fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m
                .keys()
                .filter(|k| !matches!(k.as_str(), "description" | "note") && !k.starts_with('_'))
                .collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*k).clone()).to_string());
                out.push(':');
                canonical(&m[*k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(x, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// One gate on one metric of one profile.
#[derive(Clone, Debug)]
pub struct Gate {
    pub metric: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub enforced: bool,
    pub note: String,
}

/// `gates.json`: per lane, per profile, the gates; and the two rules
/// against the baseline.
#[derive(Clone, Debug)]
pub struct Gates {
    /// A run fails when VMAF drops by more than this against the baseline.
    pub vmaf_max_drop: f64,
    /// Per lane and profile: a run fails when freeze time rises by more
    /// than this (ms) against the baseline, the run-to-run spread measured at
    /// the baseline.
    pub freeze_ms_max_rise: BTreeMap<String, BTreeMap<String, f64>>,
    pub lanes: BTreeMap<String, BTreeMap<String, Vec<Gate>>>,
}

pub fn load_gates(path: &Path) -> Result<Gates, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let root: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut rise = BTreeMap::new();
    if let Some(m) = root.get("freeze_ms_max_rise").and_then(Value::as_object) {
        for (lane, per) in m {
            let mut p = BTreeMap::new();
            for (k, v) in per
                .as_object()
                .ok_or("freeze_ms_max_rise: object per lane")?
            {
                p.insert(k.clone(), v.as_f64().ok_or("freeze_ms_max_rise: number")?);
            }
            rise.insert(lane.clone(), p);
        }
    }
    let mut lanes = BTreeMap::new();
    if let Some(m) = root.get("lanes").and_then(Value::as_object) {
        for (lane, profiles) in m {
            let mut per = BTreeMap::new();
            for (pname, list) in profiles.as_object().ok_or("lane: object")? {
                let mut gates = Vec::new();
                for g in list.as_array().ok_or("gates: array")? {
                    gates.push(Gate {
                        metric: s(g, "metric")?,
                        min: g.get("min").and_then(Value::as_f64),
                        max: g.get("max").and_then(Value::as_f64),
                        enforced: g.get("enforced").and_then(Value::as_bool).unwrap_or(true),
                        note: g
                            .get("note")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                    });
                }
                per.insert(pname.clone(), gates);
            }
            lanes.insert(lane.clone(), per);
        }
    }
    Ok(Gates {
        vmaf_max_drop: f_or(&root, "vmaf_max_drop", 1.0),
        freeze_ms_max_rise: rise,
        lanes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks/baselines/stream")
    }

    #[test]
    fn the_committed_profiles_load() {
        let gf = load(&dir().join("profiles.json")).expect("profiles.json");
        let names: Vec<&str> = gf.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "clean",
                "home-wifi",
                "mobile",
                "bad",
                "congested",
                "outage",
                "late-join"
            ]
        );
        assert!(gf.lanes.contains_key("cpu") && gf.lanes.contains_key("hw"));
        let congested = &gf.profiles[4];
        assert!(congested.host.cap.is_some());
        assert_eq!(gf.profiles[6].late_viewer_s, Some(5.0));
        load_gates(&dir().join("gates.json")).expect("gates.json");
    }

    #[test]
    fn descriptions_are_not_frozen_inputs() {
        let a = serde_json::json!({"profiles":[{"name":"x","description":"a","seed":1}]});
        let b = serde_json::json!({"profiles":[{"name":"x","description":"b","seed":1}]});
        let c = serde_json::json!({"profiles":[{"name":"x","description":"a","seed":2}]});
        assert_eq!(fingerprint(&a), fingerprint(&b));
        assert_ne!(fingerprint(&a), fingerprint(&c));
    }
}
