//! The gate definition: corpus, analysis settings, impairment profiles and
//! gates, read from `benchmarks/baselines/voice/profiles.json`.

use serde_json::Value;
use std::path::Path;

/// Gilbert-Elliott burst loss. One state step per packet.
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
}

/// One gate on one metric of one profile.
#[derive(Clone, Debug)]
pub struct Gate {
    pub metric: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// A failing enforced gate fails the run. A failing informational gate
    /// is printed only.
    pub enforced: bool,
    pub note: String,
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
    })
}

fn parse_gates(v: &Value) -> Result<Vec<Gate>, String> {
    let mut out = Vec::new();
    if let Some(list) = v.as_array() {
        for g in list {
            out.push(Gate {
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
    }
    Ok(out)
}

/// Read and validate the gate definition.
pub fn load(path: &Path) -> Result<GateFile, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let root: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
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
        profiles.push(Profile {
            name: name.clone(),
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
        });
    }
    Ok(GateFile {
        corpus,
        tail_ms,
        analysis,
        mos_max_drop,
        profiles,
        inputs_fingerprint: inputs_fingerprint(&root),
    })
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
