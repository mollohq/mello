//! voice-gate: run the voice quality gate. Use scripts/voice-gate.sh.
//!
//! ```text
//! voice-gate [--only clean,wrap] [--out DIR] [--no-score] [--send-tick] [--no-adapt]
//!            [--baseline FILE] [--report-only] [--write-baseline]
//! ```
//!
//! `--baseline FILE` compares with FILE instead of
//! `benchmarks/baselines/voice/baseline-2.9-slot.json`. A bare file name is
//! looked up in that directory. With `--write-baseline` the run writes FILE
//! and the Markdown file next to it.
//!
//! `--report-only` prints every metric and every gate with its result, but
//! a failing gate does not fail the run: for a platform that has no baseline
//! of its own. `scripts/voice-gate.sh` sets it when the host is not the
//! platform of the baseline.
//!
//! `--send-tick` replays the sender on the 20 ms voice tick of the command
//! loop, as before the stage 2 packet sink, for comparison. The default sends
//! each packet at its encode time, as the SFU path does now.
//!
//! `--no-adapt` runs without the send-leg feedback loop (rtcp.rs): no
//! receiver report reaches the sender, as before stage 5, for comparison.
//!
//! Exit codes: 0 = every enforced gate passed, 1 = an enforced gate failed,
//! 2 = setup or run error. With `--report-only`: 1 only for decode errors or
//! a scorer that ran and gave no MOS for a clip.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;
use voice_gate::report::{self, Metrics};
use voice_gate::{measure, profile, run, score, wav};

/// The baseline that `scripts/voice-gate.sh` compares with: the 2.9 audio
/// state (af8c7f0), measured with shim version 2 and the extra seeds, on
/// Windows x86-64. `baseline-2.9.json` is the same audio state with shim
/// version 1; it stays as the record.
const DEFAULT_BASELINE: &str = "baseline-2.9-slot.json";

const DEFINITIONS: &str = "
Definitions:

- Delay is mouth-to-ear: playout time minus capture time, from windowed
  cross-correlation of the output against the input.
- Start and end are the first and the last pass over the corpus. Growth is
  end minus start, so it compares the same speech.
- In-clip growth is the median over clips of the delay change from the first
  to the last third of the clip.
- Recovery (since stage 3b) is on the receiver's NetEQ buffer: packet
  buffer, sync buffer and output ring at every 10 ms pull, the mean per
  250 ms over the pulls that played decoded audio. It is the time from the
  event end until that level is within the tolerance of its median over the
  10 s before the event, and stays there for the hold time. The baseline
  value of recovery_s is the earlier definition, on the mouth-to-ear delay;
  that one is now recovery_e2e_s. recovery_target_s is the same test on
  NetEQ's target delay.
- Concealment per lost frame counts concealment frames for losses (network
  loss plus receiver drops). The target is 1. Since NetEQ: the run's
  concealed samples (in 20 ms frames) plus frames repaired from in-band FEC,
  minus the same for the same packets on the same network without loss. A
  lost Opus DTX frame is not counted as lost: comfort noise plays through it.
- A dropout is a 20 ms speech frame that comes out 20 dB too quiet, or does
  not match the input (concealment or wrong audio).
- A talk spurt starts at the first 10 ms reference frame at the active level
  after at least 500 ms below it. Onset loss is the level of the output's
  first 200 ms of the spurt against the reference's, at the spurt's measured
  delay, less the run's overall level offset. Positive is quieter.
";

struct Args {
    only: Option<Vec<String>>,
    out: Option<PathBuf>,
    no_score: bool,
    send_model: run::SendModel,
    write_baseline: bool,
    no_adapt: bool,
    baseline: Option<PathBuf>,
    report_only: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        only: None,
        out: None,
        no_score: false,
        send_model: run::SendModel::Direct,
        write_baseline: false,
        no_adapt: false,
        baseline: None,
        report_only: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--only" => {
                let v = it.next().ok_or("--only needs a list")?;
                a.only = Some(v.split(',').map(str::to_owned).collect());
            }
            "--out" => a.out = Some(PathBuf::from(it.next().ok_or("--out needs a directory")?)),
            "--no-score" => a.no_score = true,
            "--send-tick" => a.send_model = run::SendModel::Tick,
            "--write-baseline" => a.write_baseline = true,
            "--no-adapt" => a.no_adapt = true,
            "--baseline" => {
                a.baseline = Some(PathBuf::from(it.next().ok_or("--baseline needs a file")?));
            }
            "--report-only" => a.report_only = true,
            "-h" | "--help" => {
                println!(
                    "voice-gate [--only a,b] [--out DIR] [--no-score] [--send-tick] [--no-adapt] [--baseline FILE] [--report-only] [--write-baseline]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    Ok(a)
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn write_csv(path: &Path, header: &str, rows: impl Iterator<Item = String>) -> Result<(), String> {
    let mut s = String::from(header);
    s.push('\n');
    for r in rows {
        s.push_str(&r);
        s.push('\n');
    }
    std::fs::write(path, s).map_err(|e| format!("{}: {e}", path.display()))
}

fn corpus_fingerprint(root: &Path, spec: &profile::CorpusSpec) -> Result<String, String> {
    let mut all = Vec::new();
    for c in &spec.clips {
        let p = root.join(&spec.dir).join(c);
        all.extend(std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?);
    }
    Ok(profile::fnv1a_hex(&all))
}

/// "os/arch" in the words people use: "Windows x86-64", "macOS arm64".
fn platform_words(os: &str, arch: &str) -> String {
    let os = match os {
        "windows" => "Windows",
        "macos" | "darwin" => "macOS",
        "linux" => "Linux",
        other => other,
    };
    let arch = match arch {
        "x86_64" => "x86-64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{os} {arch}")
}

/// The banner of `--report-only`.
fn report_only_banner(baseline: Option<&Value>) -> String {
    let theirs = baseline
        .and_then(|b| b.get("platform"))
        .map(|p| {
            platform_words(
                p.get("os").and_then(Value::as_str).unwrap_or("?"),
                p.get("arch").and_then(Value::as_str).unwrap_or("?"),
            )
        })
        .unwrap_or_else(|| "from another platform".into());
    // VOICE_GATE_PLATFORM ("os/arch", uname words allowed) simulates another
    // host; scripts/voice-gate.sh reads it too.
    let ours = match std::env::var("VOICE_GATE_PLATFORM") {
        Ok(p) if !p.is_empty() => {
            let (os, arch) = p.split_once('/').unwrap_or((p.as_str(), "?"));
            format!(
                "{} (VOICE_GATE_PLATFORM)",
                platform_words(&os.to_ascii_lowercase(), arch)
            )
        }
        _ => platform_words(std::env::consts::OS, std::env::consts::ARCH),
    };
    format!(
        "report only: the enforced numbers and the baseline are {theirs}; this platform is {ours}"
    )
}

fn real_main() -> Result<bool, String> {
    let args = parse_args()?;
    let started = Instant::now();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;
    // canonicalize() gives a verbatim path (\\?\C:\...) on Windows. Strip
    // the prefix: Python and people read the plain form.
    let root = PathBuf::from(
        root.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_owned(),
    );
    let base_dir = root.join("benchmarks/baselines/voice");
    let baseline_path = match &args.baseline {
        Some(p) if p.components().count() == 1 => base_dir.join(p),
        Some(p) => p.clone(),
        None => base_dir.join(DEFAULT_BASELINE),
    };
    // "baseline-2.9-slot.json" is baseline "2.9-slot".
    let stem = baseline_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let baseline_name = stem.strip_prefix("baseline-").unwrap_or(&stem).to_owned();
    let out_dir = args
        .out
        .clone()
        .unwrap_or_else(|| root.join("target/voice-gate"));
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;

    if args.write_baseline && baseline_path.exists() {
        return Err(format!(
            "{} exists. A baseline is never edited. Delete it only to re-take it on the commit it came from.",
            baseline_path.display()
        ));
    }

    run::select_test_backend();
    let log_level = std::env::var("VOICE_GATE_LOG")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(3);
    // SAFETY: global setter, no pointers.
    unsafe { mello_sys::mello_set_log_level(log_level) };

    let mut gf = profile::load(&base_dir.join("profiles.json"))?;
    // Profiles added after the baseline (stage 5: congestion). Their own
    // file and fingerprint, so the baseline's frozen inputs do not change.
    let added_path = base_dir.join("profiles-congestion.json");
    if !args.write_baseline && added_path.exists() {
        profile::load_added(&mut gf, &added_path)?;
    }
    let clips = run::load_corpus(&root, &gf.corpus)?;
    let corpus_fp = corpus_fingerprint(&root, &gf.corpus)?;

    let baseline: Option<Value> = if args.write_baseline || !baseline_path.exists() {
        None
    } else {
        let b: Value =
            serde_json::from_slice(&std::fs::read(&baseline_path).map_err(|e| e.to_string())?)
                .map_err(|e| format!("{}: {e}", baseline_path.display()))?;
        let b_inputs = b
            .get("inputs_fingerprint")
            .and_then(Value::as_str)
            .unwrap_or("");
        let b_corpus = b
            .get("corpus")
            .and_then(|c| c.get("fingerprint"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if b_inputs != gf.inputs_fingerprint || b_corpus != corpus_fp {
            return Err(format!(
                "frozen inputs changed since the baseline (profiles {} vs {}, corpus {} vs {}). \
                 Re-take the baseline on the commit it came from; never compare across inputs.",
                gf.inputs_fingerprint, b_inputs, corpus_fp, b_corpus
            ));
        }
        Some(b)
    };
    let banner = args
        .report_only
        .then(|| report_only_banner(baseline.as_ref()));
    if let Some(b) = &banner {
        eprintln!("voice-gate: {b}");
    }

    let selected: Vec<&profile::Profile> = gf
        .profiles
        .iter()
        .filter(|p| {
            args.only
                .as_ref()
                .is_none_or(|o| o.iter().any(|n| n == &p.name || n == &p.group))
        })
        .collect();
    if selected.is_empty() {
        return Err("no profile selected".into());
    }
    if args.write_baseline && selected.len() != gf.profiles.len() {
        return Err("--write-baseline runs every profile; drop --only".into());
    }

    let seg_dir = out_dir.join("segments");
    let _ = std::fs::remove_dir_all(&seg_dir);
    let mut results: BTreeMap<String, Metrics> = BTreeMap::new();
    let mut extras: BTreeMap<String, Value> = BTreeMap::new();
    let mut segments = Vec::new();
    let order: Vec<String> = selected.iter().map(|p| p.name.clone()).collect();

    // One reference for every profile, and one sender trace per sender
    // setup: each profile replays a prefix of its trace (see run.rs).
    let longest = selected.iter().map(|p| p.duration_s).fold(0.0, f64::max);
    let (reference, spans) = run::build_reference(&clips, &gf.corpus, longest);
    let mut traces: BTreeMap<profile::SenderSpec, run::SenderTrace> = BTreeMap::new();
    let mut senders: Vec<profile::SenderSpec> = selected.iter().map(|p| p.sender).collect();
    senders.sort();
    senders.dedup();
    for spec in senders {
        let len = selected
            .iter()
            .filter(|p| p.sender == spec)
            .map(|p| run::reference_len(p.duration_s))
            .max()
            .unwrap_or(0);
        eprint!(
            "voice-gate: encoding {:.0} s of corpus, sender {spec} ... ",
            len as f64 / 48_000.0
        );
        let t = run::encode(&reference[..len], &spec)?;
        eprintln!("{:.1} s, {} packets", t.wall_s, t.packets.len());
        traces.insert(spec, t);
    }

    for p in &selected {
        eprint!(
            "voice-gate: {:<10} {:>5.0} s audio ... ",
            p.name, p.duration_s
        );
        let n = run::reference_len(p.duration_s);
        let r = if args.no_adapt || !p.adapt {
            run::run_receiver(
                p,
                &reference[..n],
                &spans,
                &traces[&p.sender],
                gf.tail_ms,
                args.send_model,
            )?
        } else {
            run::run_adaptive(
                p,
                &reference[..n],
                &spans,
                &traces[&p.sender],
                gf.tail_ms,
                args.send_model,
            )?
        };
        // A live sender's packets differ from the shared trace; the twin
        // replays the packets this run sent.
        let live_trace = r.live_trace.as_ref().map(|packets| run::SenderTrace {
            packets: packets.clone(),
            packets_encoded: 0,
            wall_s: 0.0,
        });
        let sent_trace = live_trace.as_ref().unwrap_or(&traces[&p.sender]);
        // The same packets on the same network without loss: the
        // concealment that loss did not cause (lib.rs, measure).
        // A bottleneck's drops depend on the queue it fills: a twin without
        // them has other delays, so it gives no concealment baseline.
        let lossless = if p.network.has_loss() && p.network.bottleneck.is_none() {
            let mut twin = (*p).clone();
            twin.network = p.network.without_loss();
            Some(run::run_receiver(
                &twin,
                &reference[..n],
                &spans,
                sent_trace,
                gf.tail_ms,
                args.send_model,
            )?)
        } else {
            None
        };
        let m = measure(
            p,
            &r,
            lossless.as_ref(),
            &gf.analysis,
            gf.corpus.clips.len(),
        );
        match &r.adapt {
            Some(a) if a.live_sender => eprintln!(
                "{:.1} s (live sender from {:.1} s: the feedback changed the encoder)",
                r.wall_s,
                a.diverged_ms.unwrap_or(0) as f64 / 1000.0
            ),
            _ => eprintln!("{:.1} s", r.wall_s),
        }

        let pdir = out_dir.join(&p.name);
        std::fs::create_dir_all(&pdir).map_err(|e| e.to_string())?;
        wav::write(&pdir.join("reference.wav"), 48_000, &r.reference)?;
        wav::write(&pdir.join("output.wav"), 48_000, &r.output)?;
        write_csv(
            &pdir.join("delay.csv"),
            "t_s,lag_ms,latency_ms,ncc",
            m.points.iter().map(|q| {
                format!(
                    "{:.3},{:.2},{:.2},{:.3}",
                    q.t_s, q.lag_ms, q.latency_ms, q.ncc
                )
            }),
        )?;
        write_csv(
            &pdir.join("buffer.csv"),
            "t_s,neteq_buffer_ms,neteq_target_ms",
            m.buffer_curve
                .iter()
                .map(|q| format!("{:.3},{:.1},{:.1}", q.t_s, q.level_ms, q.target_ms)),
        )?;
        write_csv(
            &pdir.join("receiver.csv"),
            "t_ms,pipeline_delay_ms,neteq_target_ms,playout_buffer_ms,neteq_filtered_delay_ms,spurt_hold_ms",
            r.stats.iter().map(|s| {
                format!(
                    "{},{:.1},{:.1},{:.1},{:.1},{:.1}",
                    s.t_ms,
                    s.pipeline_delay_ms,
                    s.jitter_target_ms,
                    s.playout_buffer_ms,
                    s.filtered_delay_ms,
                    s.spurt_hold_ms
                )
            }),
        )?;
        if let Some(a) = &r.adapt {
            write_csv(
                &pdir.join("sender.csv"),
                "t_ms,bitrate_kbps,loss_perc",
                a.series
                    .iter()
                    .map(|x| format!("{},{},{}", x.t_ms, x.bitrate_kbps, x.loss_perc)),
            )?;
            write_csv(
                &pdir.join("reports.csv"),
                "sent_ms,arrive_ms,fraction_lost,cumulative_lost,expected,lost,jitter_ms,rtt_ms,rtt_fresh,bitrate_kbps,loss_perc",
                a.reports.iter().map(|x| {
                    format!(
                        "{},{},{:.4},{},{},{},{:.1},{:.1},{},{},{}",
                        x.sent_ms,
                        x.arrive_ms,
                        x.feedback.fraction_lost,
                        x.feedback.cumulative_lost,
                        x.feedback.interval_expected,
                        x.feedback.interval_lost,
                        x.feedback.jitter_ms,
                        x.feedback.rtt_ms,
                        u8::from(x.feedback.rtt_fresh),
                        x.bitrate_kbps,
                        x.loss_perc
                    )
                }),
            )?;
        }
        if !args.no_score {
            segments.extend(score::write_segments(
                &seg_dir,
                &p.name,
                &r.reference,
                &r.output,
                &r.clips,
                &m.points,
            )?);
        }
        results.insert(p.name.clone(), m.metrics);
        extras.insert(p.name.clone(), m.extra);
    }

    let scores = if args.no_score {
        score::Scores {
            available: false,
            scorer: "none".into(),
            detail: json!({ "available": false, "reason": "--no-score" }),
            results: Vec::new(),
        }
    } else {
        eprintln!("voice-gate: scoring {} clips ...", segments.len());
        score::score(
            &root.join("scripts/voice-gate-score.py"),
            &out_dir,
            &segments,
        )
    };
    if !scores.available {
        eprintln!(
            "voice-gate: WARNING: no MOS scorer ({}). Structural metrics only.",
            scores
                .detail
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
    }
    let mut clip_mos: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for seg in &segments {
        let mos = scores
            .results
            .iter()
            .find(|(id, _)| id == &seg.id)
            .and_then(|(_, m)| *m);
        clip_mos
            .entry(seg.profile.clone())
            .or_default()
            .push(json!({ "clip": seg.id, "mos": mos }));
    }
    for (name, list) in &clip_mos {
        let vals: Vec<f64> = list
            .iter()
            .filter_map(|v| v.get("mos").and_then(Value::as_f64))
            .collect();
        if let Some(m) = results.get_mut(name) {
            if !vals.is_empty() {
                m.insert(
                    "mos".into(),
                    Some(vals.iter().sum::<f64>() / vals.len() as f64),
                );
                m.insert("mos_min".into(), vals.iter().cloned().reduce(f64::min));
            }
        }
    }

    // Profiles that run on several seeds: the median over the seeds.
    let groups: BTreeMap<&str, Vec<&str>> = selected.iter().fold(BTreeMap::new(), |mut g, p| {
        g.entry(p.group.as_str())
            .or_insert_with(Vec::new)
            .push(p.name.as_str());
        g
    });
    let mut seed_values = serde_json::Map::new();
    for (group, names) in groups.iter().filter(|(_, n)| n.len() > 1) {
        let mut per_metric = serde_json::Map::new();
        for metric in profile::SEED_MEDIAN_METRICS {
            let vals: Vec<Option<f64>> = names
                .iter()
                .map(|n| {
                    results
                        .get(*n)
                        .and_then(|m| m.get(*metric).cloned().flatten())
                })
                .collect();
            per_metric.insert((*metric).to_owned(), json!(vals));
            let median = if vals.iter().all(Option::is_some) {
                voice_gate::dsp::percentile(
                    &vals.iter().flatten().cloned().collect::<Vec<_>>(),
                    50.0,
                )
            } else {
                None
            };
            if let Some(m) = results.get_mut(*group) {
                m.insert(format!("{metric}_median"), median);
            }
        }
        per_metric.insert("profiles".into(), json!(names));
        seed_values.insert((*group).to_owned(), Value::Object(per_metric));
    }
    for (group, v) in &seed_values {
        if let Some(e) = extras.get_mut(group).and_then(Value::as_object_mut) {
            e.insert("seeds".into(), v.clone());
        }
    }

    let (gates, notes) = report::evaluate(
        &gf,
        &results,
        scores.available,
        baseline.as_ref(),
        &scores.scorer,
    );
    let commit = git(&root, &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&root, &["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.is_empty())
        .unwrap_or(true);

    let mut profiles_json = serde_json::Map::new();
    for p in &selected {
        let mut o = serde_json::Map::new();
        o.insert("description".into(), json!(p.description));
        o.insert("duration_s".into(), json!(p.duration_s));
        o.insert("metrics".into(), report::metrics_json(&results[&p.name]));
        o.insert(
            "clips".into(),
            json!(clip_mos.get(&p.name).cloned().unwrap_or_default()),
        );
        o.insert("info".into(), extras[&p.name].clone());
        profiles_json.insert(p.name.clone(), Value::Object(o));
    }
    let result = json!({
        "schema": 1,
        "kind": "voice-gate-result",
        "git": { "commit": commit, "dirty": dirty },
        "taken_at_utc": report::utc_now(),
        "platform": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
        "inputs_fingerprint": gf.inputs_fingerprint,
        "added_inputs_fingerprint": gf.added_inputs_fingerprint,
        "corpus": { "dir": gf.corpus.dir, "clips": gf.corpus.clips, "fingerprint": corpus_fp },
        "scorer": if scores.available {
            let mut d = scores.detail.clone();
            if let Some(m) = d.as_object_mut() { m.insert("scorer".into(), json!(scores.scorer)); }
            d
        } else { scores.detail.clone() },
        "method": {
            "path": "mello_voice_inject_capture -> mello_voice_set_packet_sink -> shim (RTP packet as the SFU forwards it, impairments) -> mello_voice_feed_rtp (NetEQ) -> mello_voice_test_pull_output",
            "backend": "MELLO_AUDIO_BACKEND=test (no device, no device thread)",
            "sender": "one encode per sender setup, 10 ms capture chunks; profiles replay a prefix of the packet trace until the send-leg feedback changes an encoder setting, then encode live",
            "feedback": if args.no_adapt {
                "none (--no-adapt)"
            } else {
                "Pion receiver reports once per second about the sender's track, from the delivered packets; sender reports with the RTP; reports reach the encoder adaptation at their arrival time (rtcp.rs)"
            },
            "send": args.send_model.describe(),
            "clock": "virtual: mello_test_set_clock_ms, 1 ms steps; receiver pulls 10 ms per step of 10 ms",
            "delay": "windowed cross-correlation at 4 kHz, reference vs output; latency = playout time - capture time",
        },
        "sender": traces.iter().map(|(spec, t)| json!({
            "push_to_talk": spec.push_to_talk,
            "noise_suppression": spec.noise_suppression,
            "packets": t.packets.len(),
            "packets_encoded": t.packets_encoded,
            "encode_wall_s": (t.wall_s * 10.0).round() / 10.0,
        })).collect::<Vec<_>>(),
        "profiles": Value::Object(profiles_json),
        "gates": report::gates_json(&gates),
        "runtime_s": (started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
    });

    let results_path = out_dir.join("results.json");
    std::fs::write(
        &results_path,
        serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", results_path.display()))?;

    let summary = report::summary_table(&order, &results, baseline.as_ref());
    println!();
    match &baseline {
        Some(b) => println!(
            "Voice quality gate. Delta against baseline {baseline_name} ({}).",
            b.get("git")
                .and_then(|g| g.get("commit"))
                .and_then(Value::as_str)
                .unwrap_or("?")
        ),
        None => println!("Voice quality gate. No baseline to compare."),
    }
    println!("Scorer: {}", scores.scorer);
    println!();
    print!("{summary}");
    print!(
        "{}",
        report::detail_table(&order, &results, baseline.as_ref())
    );
    println!();
    print!("{}", report::gate_table(&gates));
    for n in &notes {
        println!("NOTE: {n}");
    }
    println!();
    println!("Results: {}", results_path.display());
    println!("Audio and delay curves: {}", out_dir.display());
    println!("Runtime: {:.1} s", started.elapsed().as_secs_f64());

    if args.write_baseline {
        let mut b = result.clone();
        if let Some(m) = b.as_object_mut() {
            m.insert("kind".into(), json!("voice-gate-baseline"));
            m.insert("baseline".into(), json!(baseline_name));
        }
        std::fs::write(
            &baseline_path,
            serde_json::to_string_pretty(&b).map_err(|e| e.to_string())? + "\n",
        )
        .map_err(|e| format!("{}: {e}", baseline_path.display()))?;
        let md = baseline_markdown(&baseline_name, &order, &results, &result, &gates);
        let md_path = baseline_path.with_extension("md");
        std::fs::write(&md_path, md).map_err(|e| format!("{}: {e}", md_path.display()))?;
        println!(
            "Baseline written: {} and {}",
            baseline_path.display(),
            md_path.display()
        );
        // Gates are informational for the baseline run itself.
        return Ok(true);
    }

    let failed: Vec<&report::GateResult> = gates.iter().filter(|g| g.enforced && !g.pass).collect();
    if let Some(b) = &banner {
        // Only what is broken on every platform fails a report-only run.
        let decode_errors: Vec<&str> = order
            .iter()
            .filter(|n| {
                results[*n]
                    .get("decode_errors")
                    .cloned()
                    .flatten()
                    .is_some_and(|v| v > 0.0)
            })
            .map(String::as_str)
            .collect();
        let unscored: Vec<&str> = if scores.available {
            clip_mos
                .values()
                .flatten()
                .filter(|c| c.get("mos").and_then(Value::as_f64).is_none())
                .filter_map(|c| c.get("clip").and_then(Value::as_str))
                .collect()
        } else {
            Vec::new()
        };
        println!("Voice gate: {b}.");
        println!(
            "Voice gate: REPORT ONLY, {} enforced gate(s) outside their limits on this platform, not enforced.",
            failed.len()
        );
        if !decode_errors.is_empty() {
            println!(
                "Voice gate: FAIL, decode errors in {}",
                decode_errors.join(", ")
            );
        }
        if !unscored.is_empty() {
            println!(
                "Voice gate: FAIL, the scorer gave no MOS for {} clip(s): {}",
                unscored.len(),
                unscored.join(", ")
            );
        }
        return Ok(decode_errors.is_empty() && unscored.is_empty());
    }
    if failed.is_empty() {
        println!("Voice gate: PASS");
        Ok(true)
    } else {
        println!("Voice gate: FAIL ({} enforced gate(s))", failed.len());
        Ok(false)
    }
}

fn baseline_markdown(
    name: &str,
    order: &[String],
    results: &BTreeMap<String, Metrics>,
    result: &Value,
    gates: &[report::GateResult],
) -> String {
    let commit = result["git"]["commit"].as_str().unwrap_or("unknown");
    let dirty = result["git"]["dirty"].as_bool().unwrap_or(true);
    let mut s = String::new();
    let _ = writeln!(s, "# Voice quality baseline {name}\n");
    let _ = writeln!(
        s,
        "> Taken {} on {}/{} from commit `{commit}`{}. This file and \
         `baseline-{name}.json` are never edited. Every later run of \
         `scripts/voice-gate.sh` prints its delta against them.\n",
        result["taken_at_utc"].as_str().unwrap_or(""),
        result["platform"]["os"].as_str().unwrap_or(""),
        result["platform"]["arch"].as_str().unwrap_or(""),
        if dirty { " (dirty tree)" } else { "" }
    );
    let _ = writeln!(s, "| Item | Value |\n|---|---|");
    let _ = writeln!(
        s,
        "| Scorer | {} |",
        result["scorer"]["scorer"].as_str().unwrap_or("none")
    );
    let _ = writeln!(
        s,
        "| Inputs fingerprint (profiles.json without gates) | `{}` |",
        result["inputs_fingerprint"].as_str().unwrap_or("")
    );
    let _ = writeln!(
        s,
        "| Corpus fingerprint | `{}` |",
        result["corpus"]["fingerprint"].as_str().unwrap_or("")
    );
    let _ = writeln!(s, "| Runtime | {} s |", result["runtime_s"]);
    if let Some(m) = result["method"].as_object() {
        for (k, v) in m {
            let _ = writeln!(s, "| Method: {k} | {} |", v.as_str().unwrap_or(""));
        }
    }
    s.push_str(DEFINITIONS);
    let _ = writeln!(s, "\n## Summary\n");
    s.push_str(&report::summary_table(order, results, None));
    let _ = writeln!(s, "\n## All metrics\n");
    s.push_str("| metric |");
    for n in order {
        let _ = write!(s, " {n} |");
    }
    s.push_str("\n|---|");
    for _ in order {
        s.push_str("---:|");
    }
    s.push('\n');
    for (k, label, _) in report::METRICS {
        if order
            .iter()
            .all(|n| results[n].get(*k).cloned().flatten().is_none())
        {
            continue;
        }
        let _ = write!(s, "| {} |", label.trim());
        for n in order {
            let v = results[n].get(*k).cloned().flatten();
            let cell = match v {
                None => "-".to_owned(),
                Some(x) if x.fract() == 0.0 => format!("{x:.0}"),
                Some(x) => format!("{x:.2}"),
            };
            let _ = write!(s, " {cell} |");
        }
        s.push('\n');
    }
    let _ = writeln!(s, "\n## Gates at the baseline (informational)\n");
    s.push_str(&report::gate_table(gates));
    s
}

fn main() -> ExitCode {
    match real_main() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("voice-gate: error: {e}");
            ExitCode::from(2)
        }
    }
}
