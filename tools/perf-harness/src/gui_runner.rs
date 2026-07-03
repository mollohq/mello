use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as OsCommand};
use std::time::{Duration, Instant};

use perf_scenarios::load_scenario;
use serde::Deserialize;

use crate::report::ScenarioResult;
use crate::sampler::{collect_for, summarize};

#[derive(Debug, Deserialize)]
struct SamplingSignal {
    duration_s: u64,
    label: String,
}

#[derive(Debug, Deserialize)]
struct DoneSignal {
    status: String,
    error: Option<String>,
}

pub struct GuiScenarioOutput {
    pub result: ScenarioResult,
}

pub fn resolve_mello_bin() -> PathBuf {
    if let Ok(path) = std::env::var("MELLO_BIN") {
        return PathBuf::from(path);
    }
    let mello_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let release = mello_root.join("target/release/mello");
    if release.exists() {
        return release;
    }
    mello_root.join("target/debug/mello")
}

pub fn run_gui_scenario(path: &str, mello_bin: &Path) -> Result<GuiScenarioOutput, Box<dyn Error>> {
    let scenario = load_scenario(path)?;
    println!(
        "== perf gui scenario: {} ({} steps) ==",
        scenario.name,
        scenario.steps.len()
    );

    let signal_dir = std::env::temp_dir().join(format!(
        "mello-perf-gui-{}-{}",
        std::process::id(),
        scenario.name
    ));
    std::fs::create_dir_all(&signal_dir)?;

    let mut child = spawn_mello(mello_bin, path, &signal_dir)?;
    let pid = child.id();

    let sample = wait_and_sample(&mut child, pid, &signal_dir, &scenario.name)?;
    wait_for_done(&mut child, &signal_dir)?;

    let status = child.wait()?;
    if !status.success() {
        return Err(format!("mello exited with {status}").into());
    }

    let _ = std::fs::remove_dir_all(&signal_dir);
    Ok(GuiScenarioOutput { result: sample })
}

fn spawn_mello(
    mello_bin: &Path,
    scenario_path: &str,
    signal_dir: &Path,
) -> Result<Child, Box<dyn Error>> {
    let mut cmd = OsCommand::new(mello_bin);
    cmd.env("MELLO_PERF_MODE", "1")
        .env("MELLO_PERF_SCENARIO", scenario_path)
        .env("MELLO_PERF_SIGNAL_DIR", signal_dir)
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        );

    for key in [
        "PERF_TEST_EMAIL",
        "PERF_TEST_PASSWORD",
        "PERF_TEST_CREW_ID",
        "PERF_TEST_CHANNEL_ID",
        "NAKAMA_SERVER_KEY",
        "CI",
    ] {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }

    println!(
        "  spawned {} (signal_dir={})",
        mello_bin.display(),
        signal_dir.display()
    );
    Ok(cmd.spawn()?)
}

fn wait_and_sample(
    child: &mut Child,
    pid: u32,
    signal_dir: &Path,
    fallback_id: &str,
) -> Result<ScenarioResult, Box<dyn Error>> {
    let sampling_path = signal_dir.join("sampling.json");
    let deadline = Instant::now() + Duration::from_secs(300);

    loop {
        if let Some(status) = child.try_wait()? {
            return Err(format!("mello exited before sampling ({status})").into());
        }
        if sampling_path.exists() {
            let raw = std::fs::read_to_string(&sampling_path)?;
            let signal: SamplingSignal = serde_json::from_str(&raw)?;
            let id = if signal.label.is_empty() {
                fallback_id.to_string()
            } else {
                signal.label
            };
            println!("  sampling pid={pid} for {}s...", signal.duration_s);
            let samples = collect_for(
                pid,
                Duration::from_secs(signal.duration_s),
                Duration::from_secs(1),
            );
            let summary = summarize(&samples);
            return Ok(ScenarioResult::from_summary(
                id,
                signal.duration_s,
                &summary,
            ));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err("timed out waiting for sampling.json (300s)".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wait_for_done(child: &mut Child, signal_dir: &Path) -> Result<(), Box<dyn Error>> {
    let done_path = signal_dir.join("done.json");
    let deadline = Instant::now() + Duration::from_secs(120);

    while !done_path.exists() {
        if let Some(status) = child.try_wait()? {
            return Err(format!("mello exited before done.json ({status})").into());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err("timed out waiting for done.json (120s)".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let raw = std::fs::read_to_string(&done_path)?;
    let done: DoneSignal = serde_json::from_str(&raw)?;
    if done.status != "ok" {
        return Err(done
            .error
            .unwrap_or_else(|| "scenario failed".to_string())
            .into());
    }
    Ok(())
}
