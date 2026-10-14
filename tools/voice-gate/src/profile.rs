//! The gate definition: corpus, analysis settings, impairment profiles and
//! gates, read from `benchmarks/baselines/voice/profiles.json`.

use serde_json::Value;
use std::path::Path;

/// Gilbert-Elliott burst loss. One state step per 20 ms send slot (shim.rs).
#[derive(Clone, Debug)]
pub struct Gilbert {
    /// Probability to go from the good to the bad state.
    pub p_enter: f64,
    /// Probability to go from the bad to the good state.
    pub p_exit: f64,
    /// Loss probability in the bad state.
    pub loss_in_bad: f64,
}

/// A time window with different network conditions (a jitter burst).
#[derive(Clone, Debug)]
pub struct Episode {
    pub start_s: f64,
    pub dur_s: f64,
    /// Gaussian jitter in the window, replaces the network value.
    pub jitter_sigma_ms: f64,
    /// Random loss in the window, replaces the network value.
    pub loss_random: f64,
    /// A stall at the window start: packets sent in it arrive at its end.
    pub stall_ms: f64,
}

/// A rate-limited link with a tail-drop queue in a time window: congestion
/// that the sender's own rate causes. A packet that finds more than
/// `queue_ms` of backlog is dropped; the others leave the link at the rate.
#[derive(Clone, Debug)]
pub struct Bottleneck {
    pub start_s: f64,
    pub dur_s: f64,
    pub rate_kbps: f64,
    pub queue_ms: f64,
    /// Bytes per packet on the link besides the RTP packet: SRTP tag, UDP
    /// and IP headers.
    pub overhead_bytes: f64,
}

/// Network impairments between the sender and the receiver.
#[derive(Clone, Debug, Default)]
pub struct Network {
    pub base_delay_ms: f64,
    pub jitter_sigma_ms: f64,
    pub loss_random: f64,
    pub burst: Option<Gilbert>,
    /// Random stalls (delay spikes): mean count per minute and length.
    pub spikes_per_min: f64,
    pub spike_ms: f64,
    /// Probability that a packet skips the FIFO order with extra delay.
    pub reorder_prob: f64,
    pub reorder_extra_ms: f64,
    pub episodes: Vec<Episode>,
    /// Every packet sent in [start, start + dur) is lost.
    pub outage: Option<(f64, f64)>,
    /// Profiles added after the baseline only (profiles-congestion.json).
    pub bottleneck: Option<Bottleneck>,
}

impl Network {
    /// True when the network loses packets: random or burst loss, an
    /// episode with loss, or an outage.
    pub fn has_loss(&self) -> bool {
        self.loss_random > 0.0
            || self.burst.is_some()
            || self.bottleneck.is_some()
            || self.outage.is_some()
            || self.episodes.iter().any(|e| e.loss_random > 0.0)
    }

    /// The same network without loss: delay, jitter, stalls and reorder stay.
    /// The shim draws every random stream once per send slot, lost or not,
    /// so every packet keeps its delay.
    pub fn without_loss(&self) -> Network {
        let mut n = self.clone();
        n.loss_random = 0.0;
        n.burst = None;
        n.outage = None;
        for e in &mut n.episodes {
            e.loss_random = 0.0;
        }
        n
    }
}

/// One gate on one metric of one profile.
#[derive(Clone, Debug)]
pub struct Gate {
    pub metric: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// A lower bound from another profile's value of the same metric, plus
    /// an offset.
    pub min_ref: Option<GateRef>,
    /// An upper bound from another profile's value, plus an offset.
    pub max_ref: Option<GateRef>,
    /// A failing enforced gate fails the run. A failing informational gate
    /// is printed only.
    pub enforced: bool,
    pub note: String,
}

/// Where a relative bound comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefSource {
    /// The frozen baseline (baseline-2.9-slot.json, or the one given with
    /// `--baseline`).
    Baseline,
    /// This run.
    Run,
}

/// A bound relative to another profile: its value of the gate's metric in
/// `source`, plus `offset`.
#[derive(Clone, Debug)]
pub struct GateRef {
    pub source: RefSource,
    pub profile: String,
    pub offset: f64,
}

/// Sender settings. Default: the production defaults (VAD gate, RNNoise).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SenderSpec {
    /// Push-to-talk: every frame is encoded and sent (no VAD gate), so the
    /// stream has no pauses.
    pub push_to_talk: bool,
    /// RNNoise on the sender (production default: on).
    pub noise_suppression: bool,
}

impl Default for SenderSpec {
    fn default() -> Self {
        Self {
            push_to_talk: false,
            noise_suppression: true,
        }
    }
}

impl std::fmt::Display for SenderSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}, RNNoise {}",
            if self.push_to_talk {
                "push-to-talk"
            } else {
                "VAD"
            },
            if self.noise_suppression { "on" } else { "off" }
        )
    }
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub name: String,
    /// The send-leg feedback reaches the sender (stage 5). False for the
    /// `-no-adapt` twin of a profile with `compare_no_adapt`.
    pub adapt: bool,
    /// Added after the baseline (profiles-congestion.json): no baseline
    /// values, its own inputs fingerprint.
    pub added: bool,
    /// The profile whose seeds this run belongs to: its own name, or for a
    /// run of an `extra_seeds` entry the name of the profile it repeats.
    /// Gates read the median over a group (`<metric>_median`).
    pub group: String,
    pub description: String,
    pub duration_s: f64,
    pub seed: u64,
    /// First 16-bit RTP sequence number the shim gives the sender's packets.
    pub rtp_seq_start: u16,
    /// Sender clock offset. Positive: the sender clock runs fast.
    pub drift_ppm: f64,
    pub sender: SenderSpec,
    pub network: Network,
    pub gates: Vec<Gate>,
}

#[derive(Clone, Debug)]
pub struct CorpusSpec {
    pub dir: String,
    pub clips: Vec<String>,
    pub gap_ms: u32,
    pub noise_dbfs: f64,
    pub noise_seed: u64,
}

#[derive(Clone, Debug)]
pub struct AnalysisSpec {
    pub window_ms: f64,
    pub hop_ms: f64,
    pub max_lag_ms: f64,
    pub min_ncc: f64,
    pub active_dbfs: f64,
    pub recovery_tolerance_ms: f64,
    pub recovery_hold_ms: f64,
    pub dropout_db: f64,
    pub dropout_ncc: f64,
}

#[derive(Clone, Debug)]
pub struct GateFile {
    /// FNV-1a of the frozen inputs of the profiles added after the baseline
    /// (profiles-congestion.json); empty without that file.
    pub added_inputs_fingerprint: String,
    pub corpus: CorpusSpec,
    pub tail_ms: u32,
    pub analysis: AnalysisSpec,
    pub mos_max_drop: f64,
    pub profiles: Vec<Profile>,
    /// FNV-1a of the frozen inputs: everything in the file except gates,
    /// descriptions and comments. A baseline is only comparable with runs
    /// that have the same value.
    pub inputs_fingerprint: String,
}

fn f(v: &Value, key: &str) -> Result<f64, String> {
    v.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("missing number '{key}'"))
}

fn f_or(v: &Value, key: &str, default: f64) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(default)
}

fn s(v: &Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("missing string '{key}'"))
}

/// FNV-1a 64 of bytes, as hex.
pub fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    format!("{h:016x}")
}

fn parse_network(v: &Value) -> Result<Network, String> {
    let burst = match v.get("burst") {
        Some(b) if !b.is_null() => Some(Gilbert {
            p_enter: f(b, "p_enter")?,
            p_exit: f(b, "p_exit")?,
            loss_in_bad: f(b, "loss_in_bad")?,
        }),
        _ => None,
    };
    let base_sigma = f_or(v, "jitter_sigma_ms", 0.0);
    let base_loss = f_or(v, "loss_random", 0.0);
    let mut episodes = Vec::new();
    if let Some(list) = v.get("episodes").and_then(Value::as_array) {
        for e in list {
            episodes.push(Episode {
                start_s: f(e, "start_s")?,
                dur_s: f(e, "dur_s")?,
                jitter_sigma_ms: f_or(e, "jitter_sigma_ms", base_sigma),
                loss_random: f_or(e, "loss_random", base_loss),
                stall_ms: f_or(e, "stall_ms", 0.0),
            });
        }
    }
    let outage = match v.get("outage") {
        Some(o) if !o.is_null() => Some((f(o, "start_s")?, f(o, "dur_s")?)),
        _ => None,
    };
    let bottleneck = match v.get("bottleneck") {
        Some(b) if !b.is_null() => Some(Bottleneck {
            start_s: f(b, "start_s")?,
            dur_s: f(b, "dur_s")?,
            rate_kbps: f(b, "rate_kbps")?,
            queue_ms: f(b, "queue_ms")?,
            overhead_bytes: f_or(b, "overhead_bytes", 0.0),
        }),
        _ => None,
    };
    Ok(Network {
        base_delay_ms: f(v, "base_delay_ms")?,
        jitter_sigma_ms: base_sigma,
        loss_random: base_loss,
        burst,
        spikes_per_min: f_or(v, "spikes_per_min", 0.0),
        spike_ms: f_or(v, "spike_ms", 0.0),
        reorder_prob: f_or(v, "reorder_prob", 0.0),
        reorder_extra_ms: f_or(v, "reorder_extra_ms", 0.0),
        episodes,
        outage,
        bottleneck,
    })
}

/// `min_ref` or `max_ref` of a gate: `{ "source", "profile", "offset" }`.
fn parse_ref(g: &Value, key: &str) -> Result<Option<GateRef>, String> {
    match g.get(key) {
        Some(r) if !r.is_null() => Ok(Some(GateRef {
            source: match s(r, "source")?.as_str() {
                "baseline" => RefSource::Baseline,
                "run" => RefSource::Run,
                other => return Err(format!("{key}.source '{other}': use 'baseline' or 'run'")),
            },
            profile: s(r, "profile")?,
            offset: f_or(r, "offset", 0.0),
        })),
        _ => Ok(None),
    }
}

fn parse_gates(v: &Value) -> Result<Vec<Gate>, String> {
    let mut out = Vec::new();
    if let Some(list) = v.as_array() {
        for g in list {
            let min_ref = parse_ref(g, "min_ref")?;
            let max_ref = parse_ref(g, "max_ref")?;
            out.push(Gate {
                metric: s(g, "metric")?,
                min: g.get("min").and_then(Value::as_f64),
                max: g.get("max").and_then(Value::as_f64),
                min_ref,
                max_ref,
                enforced: g.get("enforced").and_then(Value::as_bool).unwrap_or(true),
                note: g
                    .get("note")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            });
        }
    }
    Ok(out)
}

/// The shim version a gate file is written for: its `"shim"` field, 1 when
/// absent. It must be the one this harness implements
/// ([`crate::shim::SHIM_VERSION`]). The field is a frozen input, so a new
/// shim rule changes `inputs_fingerprint` on purpose.
fn check_shim_version(root: &Value, path: &Path) -> Result<(), String> {
    let want = crate::shim::SHIM_VERSION;
    let got = match root.get("shim") {
        None => 1,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| format!("{}: 'shim' must be an integer", path.display()))?,
    };
    if got == want {
        Ok(())
    } else {
        Err(format!(
            "{}: written for shim version {got}, this harness implements version {want}",
            path.display()
        ))
    }
}

/// Read and validate the gate definition.
pub fn load(path: &Path) -> Result<GateFile, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let root: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    check_shim_version(&root, path)?;
    let c = root.get("corpus").ok_or("missing 'corpus'")?;
    let corpus = CorpusSpec {
        dir: s(c, "dir")?,
        clips: c
            .get("clips")
            .and_then(Value::as_array)
            .ok_or("missing 'corpus.clips'")?
            .iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect(),
        gap_ms: f(c, "gap_ms")? as u32,
        noise_dbfs: f(c, "noise_dbfs")?,
        noise_seed: f(c, "noise_seed")? as u64,
    };
    let a = root.get("analysis").ok_or("missing 'analysis'")?;
    let analysis = AnalysisSpec {
        window_ms: f(a, "window_ms")?,
        hop_ms: f(a, "hop_ms")?,
        max_lag_ms: f(a, "max_lag_ms")?,
        min_ncc: f(a, "min_ncc")?,
        active_dbfs: f(a, "active_dbfs")?,
        recovery_tolerance_ms: f(a, "recovery_tolerance_ms")?,
        recovery_hold_ms: f(a, "recovery_hold_ms")?,
        dropout_db: f(a, "dropout_db")?,
        dropout_ncc: f(a, "dropout_ncc")?,
    };
    let receiver = root.get("receiver").ok_or("missing 'receiver'")?;
    let tail_ms = f(receiver, "tail_ms")? as u32;
    let mos_max_drop = root
        .get("gates")
        .map(|g| f(g, "mos_max_drop"))
        .transpose()?
        .unwrap_or(0.1);
    let profiles = parse_profiles(&root, false)?;
    Ok(GateFile {
        added_inputs_fingerprint: String::new(),
        corpus,
        tail_ms,
        analysis,
        mos_max_drop,
        profiles,
        inputs_fingerprint: inputs_fingerprint(&root),
    })
}

/// Add the profiles of a file added after the baseline (it has only
/// `profiles`). The baseline's fingerprint does not cover them; they get
/// their own. A profile with `"compare_no_adapt": true` also runs as
/// `<name>-no-adapt`, without the send-leg feedback and without gates, so a
/// gate can compare the two with `min_ref`. `"extra_seeds"` works in both
/// files: see [`parse_profiles`].
pub fn load_added(gf: &mut GateFile, path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let root: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    check_shim_version(&root, path)?;
    let added = parse_profiles(&root, true)?;
    for p in &added {
        if gf.profiles.iter().any(|q| q.name == p.name) {
            return Err(format!("{}: profile '{}' exists", path.display(), p.name));
        }
    }
    gf.profiles.extend(added);
    gf.added_inputs_fingerprint = inputs_fingerprint(&root);
    Ok(())
}

/// The profiles of a gate file. A profile with `"extra_seeds": [..]` (in
/// `profiles.json` or in an added file) also runs once per extra seed as
/// `<name>@<seed>` (and `<name>-no-adapt@<seed>`), without gates; the
/// harness adds the median over the seeds of [`SEED_MEDIAN_METRICS`] to the
/// profile as `<metric>_median`.
fn parse_profiles(root: &Value, added: bool) -> Result<Vec<Profile>, String> {
    let mut profiles = Vec::new();
    for p in root
        .get("profiles")
        .and_then(Value::as_array)
        .ok_or("missing 'profiles'")?
    {
        let name = s(p, "name")?;
        let ctx = |e: String| format!("profile '{name}': {e}");
        let seq = f(p, "rtp_seq_start").map_err(ctx)?;
        if !(0.0..=65535.0).contains(&seq) {
            return Err(ctx("rtp_seq_start out of range".into()));
        }
        let compare_no_adapt = p
            .get("compare_no_adapt")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let extra_seeds: Vec<u64> = match p.get("extra_seeds") {
            None => Vec::new(),
            Some(v) => v
                .as_array()
                .ok_or_else(|| ctx("extra_seeds must be an array".into()))?
                .iter()
                .map(|x| {
                    x.as_u64()
                        .ok_or_else(|| ctx("extra seed is not an integer".into()))
                })
                .collect::<Result<_, _>>()?,
        };
        profiles.push(Profile {
            name: name.clone(),
            adapt: true,
            added,
            description: s(p, "description").map_err(ctx)?,
            duration_s: f(p, "duration_s").map_err(ctx)?,
            seed: f(p, "seed").map_err(ctx)? as u64,
            rtp_seq_start: seq as u16,
            drift_ppm: f_or(p, "drift_ppm", 0.0),
            sender: SenderSpec {
                push_to_talk: p
                    .get("sender")
                    .and_then(|x| x.get("push_to_talk"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                noise_suppression: p
                    .get("sender")
                    .and_then(|x| x.get("noise_suppression"))
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            },
            network: parse_network(
                p.get("network")
                    .ok_or_else(|| ctx("missing network".into()))?,
            )
            .map_err(ctx)?,
            gates: parse_gates(p.get("gates").unwrap_or(&Value::Null)).map_err(ctx)?,
            group: name.clone(),
        });
        let base = profiles.last().cloned().ok_or("no profile")?;
        if compare_no_adapt {
            profiles.push(no_adapt_twin(&base));
        }
        for &seed in &extra_seeds {
            let mut rep = base.clone();
            rep.name = format!("{name}@{seed}");
            rep.description = format!("{name} with seed {seed}");
            rep.seed = seed;
            rep.gates = Vec::new();
            profiles.push(rep.clone());
            if compare_no_adapt {
                let mut twin = no_adapt_twin(&base);
                twin.name = format!("{}@{seed}", twin.name);
                twin.seed = seed;
                profiles.push(twin);
            }
        }
    }
    Ok(profiles)
}

/// Metrics of a profile with `extra_seeds` whose median over the seeds the
/// harness adds as `<metric>_median`.
pub const SEED_MEDIAN_METRICS: &[&str] =
    &["bottleneck_delay_mean_ms", "latency_clip_growth_ms", "mos"];

fn no_adapt_twin(base: &Profile) -> Profile {
    let mut twin = base.clone();
    twin.name = format!("{}-no-adapt", base.name);
    twin.group = twin.name.clone();
    twin.description = format!("{} without the send-leg feedback (comparison)", base.name);
    twin.adapt = false;
    twin.gates = Vec::new();
    twin
}

/// Fingerprint of the frozen inputs. Gates may change between stages;
/// network parameters, seeds, corpus and analysis settings may not.
fn inputs_fingerprint(root: &Value) -> String {
    let mut text = String::new();
    canonical(root, &mut text);
    fnv1a_hex(text.as_bytes())
}

/// Serialize with sorted keys and without gates, descriptions or comments.
/// Explicit sorting: serde_json key order depends on a cargo feature that
/// another workspace crate can switch on.
fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m
                .keys()
                .filter(|k| {
                    !matches!(k.as_str(), "gates" | "description" | "note") && !k.starts_with('_')
                })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The gate files of the repository.
    fn gate_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks/baselines/voice")
    }

    #[test]
    fn extra_seeds_work_in_profiles_json() {
        let gf = load(&gate_dir().join("profiles.json")).expect("profiles.json");
        for (name, seed) in [("home-wifi", 202), ("mobile", 303), ("bad", 404)] {
            let group: Vec<(&str, u64, usize)> = gf
                .profiles
                .iter()
                .filter(|p| p.group == name)
                .map(|p| (p.name.as_str(), p.seed, p.gates.len()))
                .collect();
            let rep1 = format!("{name}@{}", seed + 1);
            let rep2 = format!("{name}@{}", seed + 2);
            assert_eq!(group.len(), 3, "{name}: {group:?}");
            assert_eq!((group[0].0, group[0].1), (name, seed));
            assert!(group[0].2 > 0, "{name} keeps its gates");
            assert_eq!(group[1], (rep1.as_str(), seed + 1, 0));
            assert_eq!(group[2], (rep2.as_str(), seed + 2, 0));
            let base = &gf.profiles[gf.profiles.iter().position(|p| p.name == name).unwrap()];
            assert!(base
                .gates
                .iter()
                .any(|g| g.metric == "latency_clip_growth_ms_median" && g.enforced));
        }
        assert!(SEED_MEDIAN_METRICS.contains(&"latency_clip_growth_ms"));
    }

    #[test]
    fn a_gate_file_for_another_shim_version_is_refused() {
        let p = Path::new("profiles.json");
        assert!(check_shim_version(&serde_json::json!({ "shim": 2 }), p).is_ok());
        let old = check_shim_version(&serde_json::json!({}), p).unwrap_err();
        assert!(old.contains("shim version 1"), "{old}");
        assert!(check_shim_version(&serde_json::json!({ "shim": 3 }), p).is_err());
    }

    #[test]
    fn the_shim_version_is_a_frozen_input() {
        let a = serde_json::json!({ "shim": 2, "profiles": [] });
        let b = serde_json::json!({ "shim": 1, "profiles": [] });
        assert_ne!(inputs_fingerprint(&a), inputs_fingerprint(&b));
    }
}
