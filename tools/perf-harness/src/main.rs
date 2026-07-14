mod gui_runner;
mod inject_loop;
mod report;
mod runner;
mod sampler;
mod wav_player;

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command as OsCommand;
use std::time::SystemTime;

use gui_runner::{resolve_mello_bin, run_gui_scenario};
use mello_core::Config;
use report::{
    compare_report, report_to_baseline, write_summary_md, BaselineFile, BuildInfo, GitInfo,
    PerfReport, PlatformInfo,
};
use runner::run_scenario;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        std::process::exit(2);
    }

    match args[1].as_str() {
        "run" => run_cmd(&args[2..]),
        "run-gui" => run_gui_cmd(&args[2..]),
        "compare" => compare_cmd(&args[2..]),
        "write-baseline" => write_baseline_cmd(&args[2..]),
        "--help" | "-h" | "help" => {
            print_usage();
        }
        other => {
            eprintln!("unknown subcommand: {other}");
            print_usage();
            std::process::exit(2);
        }
    }
}

fn print_usage() {
    eprintln!(
        "Usage:
  perf-harness run [--scenario-dir DIR] [--output PATH] [--compare BASELINE.json]
  perf-harness run-gui [--scenario-dir DIR] [--output PATH] [--compare BASELINE.json] [--mello-bin PATH]
  perf-harness compare --report REPORT.json --baseline BASELINE.json
  perf-harness write-baseline --report REPORT.json --output BASELINE.json

Environment:
  PERF_TEST_EMAIL / PERF_TEST_PASSWORD / PERF_TEST_CREW_ID / PERF_TEST_CHANNEL_ID
  PERF_TEST_WAV (mono 48kHz 16-bit PCM; headless voice scenarios only)
  MELLO_BIN (optional path to release mello binary for run-gui)
  NAKAMA_SERVER_KEY (development backend)"
    );
}

fn run_cmd(args: &[String]) {
    let mut scenario_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios");
    let mut output = PathBuf::from("perf-report.json");
    let mut baseline_path: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scenario-dir" => {
                i += 1;
                scenario_dir = PathBuf::from(args.get(i).expect("--scenario-dir needs a value"));
            }
            "--output" => {
                i += 1;
                output = PathBuf::from(args.get(i).expect("--output needs a value"));
            }
            "--compare" => {
                i += 1;
                baseline_path = Some(PathBuf::from(args.get(i).expect("--compare needs a value")));
            }
            flag => {
                eprintln!("unknown flag: {flag}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let cfg = Config::development();
    let mut scenarios = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(&scenario_dir)
        .expect("read scenario dir")
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if file_name.starts_with("voice_")
            && env::var("PERF_TEST_EMAIL").unwrap_or_default().is_empty()
        {
            println!(
                "SKIP {} (set PERF_TEST_EMAIL for voice scenarios)",
                file_name
            );
            continue;
        }
        match run_scenario(path.to_str().unwrap(), cfg.clone()) {
            Ok(out) => {
                println!(
                    "PASS {}: footprint_p95={:.1}MB wakeups={:.1}/s cpu={:.1}%",
                    out.result.id,
                    out.result.footprint_mb.p95,
                    out.result.wakeups_per_s,
                    out.result.cpu_pct
                );
                scenarios.push(out.result);
            }
            Err(e) => {
                eprintln!("FAIL {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    }

    if scenarios.is_empty() {
        eprintln!("no scenarios found in {}", scenario_dir.display());
        std::process::exit(2);
    }

    let mut report = PerfReport {
        run_id: run_id(),
        git: git_info(),
        platform: platform_info(),
        build: BuildInfo {
            profile: if cfg!(debug_assertions) {
                "debug".to_string()
            } else {
                "release".to_string()
            },
        },
        scenarios,
        regressions: Vec::new(),
    };

    if let Some(base_path) = baseline_path {
        let raw = std::fs::read_to_string(&base_path).expect("read baseline");
        let baseline: BaselineFile = serde_json::from_str(&raw).expect("parse baseline");
        report.regressions = compare_report(&report, &baseline);
    }

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    let json = serde_json::to_string_pretty(&report).expect("serialize report");
    std::fs::write(&output, &json).expect("write report");
    write_summary_md(&output.with_extension("md"), &report);

    if report.regressions.is_empty() {
        println!("perf-harness PASS: wrote {}", output.display());
    } else {
        eprintln!(
            "perf-harness FAIL: {} regression(s), wrote {}",
            report.regressions.len(),
            output.display()
        );
        std::process::exit(1);
    }
}

fn run_gui_cmd(args: &[String]) {
    let mut scenario_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios-gui");
    let mut output = PathBuf::from("perf-report-gui.json");
    let mut baseline_path: Option<PathBuf> = None;
    let mut mello_bin: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scenario-dir" => {
                i += 1;
                scenario_dir = PathBuf::from(args.get(i).expect("--scenario-dir needs a value"));
            }
            "--output" => {
                i += 1;
                output = PathBuf::from(args.get(i).expect("--output needs a value"));
            }
            "--compare" => {
                i += 1;
                baseline_path = Some(PathBuf::from(args.get(i).expect("--compare needs a value")));
            }
            "--mello-bin" => {
                i += 1;
                mello_bin = Some(PathBuf::from(
                    args.get(i).expect("--mello-bin needs a value"),
                ));
            }
            flag => {
                eprintln!("unknown flag: {flag}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let mello_bin = mello_bin.unwrap_or_else(resolve_mello_bin);
    if !mello_bin.exists() {
        eprintln!(
            "mello binary not found at {} (build with `cargo build --release -p mello-client`)",
            mello_bin.display()
        );
        std::process::exit(2);
    }

    let mut scenarios = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(&scenario_dir)
        .expect("read scenario dir")
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if env::var("PERF_TEST_EMAIL").unwrap_or_default().is_empty() {
            eprintln!(
                "SKIP {} (set PERF_TEST_EMAIL or source voice local-fixtures.env)",
                file_name
            );
            continue;
        }
        match run_gui_scenario(path.to_str().unwrap(), &mello_bin) {
            Ok(out) => {
                println!(
                    "PASS {}: footprint_p95={:.1}MB wakeups={:.1}/s cpu={:.1}%",
                    out.result.id,
                    out.result.footprint_mb.p95,
                    out.result.wakeups_per_s,
                    out.result.cpu_pct
                );
                scenarios.push(out.result);
            }
            Err(e) => {
                eprintln!("FAIL {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    }

    if scenarios.is_empty() {
        eprintln!("no GUI scenarios ran in {}", scenario_dir.display());
        if env::var("PERF_TEST_EMAIL").unwrap_or_default().is_empty() {
            eprintln!("hint: set PERF_TEST_EMAIL (run-gui.sh loads voice fixtures automatically)");
        }
        std::process::exit(2);
    }

    let mut report = PerfReport {
        run_id: run_id(),
        git: git_info(),
        platform: platform_info(),
        build: BuildInfo {
            profile: "release".to_string(),
        },
        scenarios,
        regressions: Vec::new(),
    };

    if let Some(base_path) = baseline_path {
        let raw = std::fs::read_to_string(&base_path).expect("read baseline");
        let baseline: BaselineFile = serde_json::from_str(&raw).expect("parse baseline");
        report.regressions = compare_report(&report, &baseline);
    }

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    let json = serde_json::to_string_pretty(&report).expect("serialize report");
    std::fs::write(&output, &json).expect("write report");
    write_summary_md(&output.with_extension("md"), &report);

    if report.regressions.is_empty() {
        println!("perf-harness GUI PASS: wrote {}", output.display());
    } else {
        eprintln!(
            "perf-harness GUI FAIL: {} regression(s), wrote {}",
            report.regressions.len(),
            output.display()
        );
        std::process::exit(1);
    }
}

fn compare_cmd(args: &[String]) {
    let mut report_path = None;
    let mut baseline_path = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--report" => {
                i += 1;
                report_path = Some(PathBuf::from(args.get(i).expect("--report")));
            }
            "--baseline" => {
                i += 1;
                baseline_path = Some(PathBuf::from(args.get(i).expect("--baseline")));
            }
            flag => {
                eprintln!("unknown flag: {flag}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let report_path = report_path.expect("--report required");
    let baseline_path = baseline_path.expect("--baseline required");
    let report: PerfReport =
        serde_json::from_str(&std::fs::read_to_string(&report_path).expect("read report"))
            .expect("parse report");
    let baseline: BaselineFile =
        serde_json::from_str(&std::fs::read_to_string(&baseline_path).expect("read baseline"))
            .expect("parse baseline");
    let regressions = compare_report(&report, &baseline);
    if regressions.is_empty() {
        println!("compare PASS");
    } else {
        for r in &regressions {
            eprintln!(
                "REGRESSION {} {}: baseline={:.2} actual={:.2}",
                r.scenario, r.metric, r.baseline, r.actual
            );
        }
        std::process::exit(1);
    }
}

fn write_baseline_cmd(args: &[String]) {
    let mut report_path = None;
    let mut output = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--report" => {
                i += 1;
                report_path = Some(PathBuf::from(args.get(i).expect("--report")));
            }
            "--output" => {
                i += 1;
                output = Some(PathBuf::from(args.get(i).expect("--output")));
            }
            flag => {
                eprintln!("unknown flag: {flag}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let report: PerfReport =
        serde_json::from_str(&std::fs::read_to_string(report_path.as_ref().unwrap()).unwrap())
            .unwrap();
    let baseline = report_to_baseline(&report);
    let out = output.unwrap_or_else(|| PathBuf::from("baseline.json"));
    std::fs::write(&out, serde_json::to_string_pretty(&baseline).unwrap()).unwrap();
    println!("wrote baseline {}", out.display());
}

fn run_id() -> String {
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("perf-{ts}-{}", std::process::id())
}

fn git_info() -> GitInfo {
    let head = git_output(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let branch = git_output(&["symbolic-ref", "--short", "HEAD"])
        .or_else(|| git_output(&["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    GitInfo { head, branch }
}

fn git_output(args: &[&str]) -> Option<String> {
    let mello_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = OsCommand::new("git")
        .args(args)
        .current_dir(mello_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn platform_info() -> PlatformInfo {
    PlatformInfo {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
    }
}
