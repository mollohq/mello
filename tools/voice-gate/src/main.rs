//! voice-gate: run the voice quality gate. Use scripts/voice-gate.sh.
//!
//! ```text
//! voice-gate [--only clean,wrap] [--out DIR] [--no-score] [--write-baseline]
//! ```
//!
//! Exit codes: 0 = every enforced gate passed, 1 = an enforced gate failed,
//! 2 = setup error.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;
use voice_gate::report::{self, Metrics};
use voice_gate::{measure, profile, run, score, wav};

const BASELINE_NAME: &str = "2.9";

const DEFINITIONS: &str = "
Definitions:

- Delay is mouth-to-ear: playout time minus capture time, from windowed
  cross-correlation of the output against the input.
- Start and end are the first and the last pass over the corpus. Growth is
  end minus start, so it compares the same speech.
- In-clip growth is the median over clips of the delay change from the first
  to the last third of the clip.
- Concealment per lost frame counts PLC and FEC frames for losses (network
  loss plus receiver drops). The target is 1.
- A dropout is a 20 ms speech frame that comes out 20 dB too quiet, or does
  not match the input (concealment or wrong audio).
";

struct Args {
    only: Option<Vec<String>>,
    out: Option<PathBuf>,
    no_score: bool,
    write_baseline: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        only: None,
        out: None,
        no_score: false,
        write_baseline: false,
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
            "--write-baseline" => a.write_baseline = true,
            "-h" | "--help" => {
                println!("voice-gate [--only a,b] [--out DIR] [--no-score] [--write-baseline]");
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
    let baseline_path = base_dir.join(format!("baseline-{BASELINE_NAME}.json"));
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

    let gf = profile::load(&base_dir.join("profiles.json"))?;
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

    let selected: Vec<&profile::Profile> = gf
        .profiles
        .iter()
        .filter(|p| {
            args.only
                .as_ref()
                .is_none_or(|o| o.iter().any(|n| n == &p.name))
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
        let r = run::run_receiver(p, &reference[..n], &spans, &traces[&p.sender], gf.tail_ms)?;
        let m = measure(p, &r, &gf.analysis, gf.corpus.clips.len());
        eprintln!("{:.1} s", r.wall_s);

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
            &pdir.join("receiver.csv"),
            "t_ms,pipeline_delay_ms,jitter_target_ms,playout_buffer_ms,jitter_buffered",
            r.stats.iter().map(|s| {
                format!(
                    "{},{:.1},{:.1},{:.1},{}",
                    s.t_ms,
                    s.pipeline_delay_ms,
                    s.jitter_target_ms,
                    s.playout_buffer_ms,
                    s.jitter_buffered
                )
            }),
        )?;
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
        "corpus": { "dir": gf.corpus.dir, "clips": gf.corpus.clips, "fingerprint": corpus_fp },
        "scorer": if scores.available {
            let mut d = scores.detail.clone();
            if let Some(m) = d.as_object_mut() { m.insert("scorer".into(), json!(scores.scorer)); }
            d
        } else { scores.detail.clone() },
        "method": {
            "path": "mello_voice_inject_capture -> mello_voice_get_packet -> shim (SFU header rewrite, impairments) -> mello_voice_feed_packet -> mello_voice_test_pull_output",
            "backend": "MELLO_AUDIO_BACKEND=test (no device, no device thread)",
            "sender": "one encode per sender setup, 10 ms capture chunks; profiles replay a prefix of the packet trace",
            "send_tick": "packets leave on a 20 ms tick at sender time 20k+10 ms, like VoiceManager::tick",
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
            "Voice quality gate. Delta against baseline {BASELINE_NAME} ({}).",
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
            m.insert("baseline".into(), json!(BASELINE_NAME));
        }
        std::fs::write(
            &baseline_path,
            serde_json::to_string_pretty(&b).map_err(|e| e.to_string())? + "\n",
        )
        .map_err(|e| format!("{}: {e}", baseline_path.display()))?;
        let md = baseline_markdown(&order, &results, &result, &gates);
        let md_path = base_dir.join(format!("baseline-{BASELINE_NAME}.md"));
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
    if failed.is_empty() {
        println!("Voice gate: PASS");
        Ok(true)
    } else {
        println!("Voice gate: FAIL ({} enforced gate(s))", failed.len());
        Ok(false)
    }
}

fn baseline_markdown(
    order: &[String],
    results: &BTreeMap<String, Metrics>,
    result: &Value,
    gates: &[report::GateResult],
) -> String {
    let commit = result["git"]["commit"].as_str().unwrap_or("unknown");
    let dirty = result["git"]["dirty"].as_bool().unwrap_or(true);
    let mut s = String::new();
    let _ = writeln!(s, "# Voice quality baseline {BASELINE_NAME}\n");
    let _ = writeln!(
        s,
        "> Taken {} on {}/{} from commit `{commit}`{}. This file and \
         `baseline-{BASELINE_NAME}.json` are never edited. Every later run of \
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
