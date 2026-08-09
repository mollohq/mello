//! Headless perf scenario driver for the full Slint client (v2 GUI harness).
//!
//! Enabled when `MELLO_PERF_MODE=1`. Runs JSON scenarios via `cmd_tx`, signals the
//! external harness at sample boundaries, then quits the event loop.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mello_core::{Command, Event};
use perf_scenarios::{event_type, load_scenario, Step};
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Serialize)]
struct SamplingSignal {
    duration_s: u64,
    label: String,
}

#[derive(Serialize)]
struct DoneSignal {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Facts a scenario can only learn while it runs.
///
/// `delete_crew` with no explicit id targets the crew this run created, whose
/// id does not exist until `finalize_onboarding` succeeds. Every event the
/// runner consumes passes through `observe` so the id is captured wherever it
/// happens to arrive — `expect_event` discards non-matching events, and
/// `CrewCreated` is one of them.
#[derive(Default)]
struct RunState {
    created_crew_id: Option<String>,
}

impl RunState {
    fn observe(&mut self, ev: &Event) {
        if let Event::CrewCreated { crew, .. } = ev {
            self.created_crew_id = Some(crew.id.clone());
        }
    }
}

pub fn enabled() -> bool {
    std::env::var("MELLO_PERF_MODE").ok().as_deref() == Some("1")
}

pub fn signal_dir() -> PathBuf {
    std::env::var("MELLO_PERF_SIGNAL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join(format!("mello-perf-{}", std::process::id())))
}

pub fn scenario_path() -> Option<PathBuf> {
    std::env::var("MELLO_PERF_SCENARIO").ok().map(PathBuf::from)
}

pub fn start(cmd_tx: UnboundedSender<Command>, event_rx: mpsc::Receiver<Event>) {
    let Some(path) = scenario_path() else {
        log::error!("[perf] MELLO_PERF_SCENARIO not set");
        return;
    };
    let signal_dir = signal_dir();
    if let Err(e) = std::fs::create_dir_all(&signal_dir) {
        log::error!(
            "[perf] failed to create signal dir {}: {e}",
            signal_dir.display()
        );
        return;
    }

    log::info!(
        "[perf] scenario={} signal_dir={}",
        path.display(),
        signal_dir.display()
    );

    std::thread::spawn(move || {
        let result = run_scenario(&path, &cmd_tx, &event_rx, &signal_dir);
        write_done(&signal_dir, &result);
        if let Err(e) = slint::invoke_from_event_loop(|| {
            slint::quit_event_loop().ok();
        }) {
            log::error!("[perf] failed to quit event loop: {e}");
        }
    });
}

fn run_scenario(
    path: &Path,
    cmd_tx: &UnboundedSender<Command>,
    event_rx: &mpsc::Receiver<Event>,
    signal_dir: &Path,
) -> Result<(), String> {
    let scenario = load_scenario(path.to_str().unwrap_or_default()).map_err(|e| e.to_string())?;
    let mut state = RunState::default();

    for (i, step) in scenario.steps.iter().enumerate() {
        log::info!("[perf] step {}: {:?}", i + 1, step);
        match step {
            Step::DeviceAuth { device_id } => send(
                cmd_tx,
                Command::DeviceAuth {
                    device_id: device_id.clone(),
                },
            )?,
            Step::Login { email, password } => send(
                cmd_tx,
                Command::Login {
                    email: email.clone(),
                    password: password.clone(),
                },
            )?,
            Step::DiscoverCrews => send(cmd_tx, Command::DiscoverCrews { cursor: None })?,
            Step::FinalizeOnboarding {
                crew_id,
                crew_name,
                display_name,
            } => send(
                cmd_tx,
                Command::FinalizeOnboarding {
                    crew_id: crew_id.clone(),
                    crew_name: crew_name.clone(),
                    crew_description: None,
                    crew_open: Some(false),
                    crew_avatar: None,
                    display_name: display_name.clone(),
                    avatar_data: None,
                    avatar_format: None,
                    avatar_style: None,
                    avatar_seed: None,
                },
            )?,
            Step::DeleteCrew { crew_id } => {
                let target = crew_id
                    .clone()
                    .or_else(|| state.created_crew_id.clone())
                    .ok_or_else(|| {
                        "delete_crew without crew_id, but this run created no crew \
                         (is finalize_onboarding missing, or did it fail?)"
                            .to_string()
                    })?;
                send(cmd_tx, Command::DeleteCrew { crew_id: target })?
            }
            Step::DeleteAccount => send(cmd_tx, Command::DeleteAccount)?,
            Step::SelectCrew { crew_id } => send(
                cmd_tx,
                Command::SelectCrew {
                    crew_id: crew_id.clone(),
                },
            )?,
            Step::JoinVoice { channel_id } => send(
                cmd_tx,
                Command::JoinVoice {
                    channel_id: channel_id.clone(),
                },
            )?,
            Step::LeaveVoice => send(cmd_tx, Command::LeaveVoice)?,
            Step::SetMute { muted } => send(cmd_tx, Command::SetMute { muted: *muted })?,
            Step::InjectWav { .. } | Step::StopInject => {
                return Err("inject_wav is not supported in GUI perf mode".to_string());
            }
            Step::Sleep { ms } => drain_for(event_rx, Duration::from_millis(*ms), &mut state),
            Step::ExpectEvent { event, timeout_ms } => {
                expect_event(
                    event_rx,
                    event,
                    Duration::from_millis(*timeout_ms),
                    &mut state,
                )?;
            }
            Step::Sample { duration_s, label } => {
                let sample_label = if label.is_empty() {
                    scenario.name.clone()
                } else {
                    label.clone()
                };
                write_sampling(signal_dir, *duration_s, &sample_label)?;
                std::thread::sleep(Duration::from_secs(*duration_s));
            }
        }
    }
    Ok(())
}

fn write_sampling(dir: &Path, duration_s: u64, label: &str) -> Result<(), String> {
    let signal = SamplingSignal {
        duration_s,
        label: label.to_string(),
    };
    let path = dir.join("sampling.json");
    let json = serde_json::to_string(&signal).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    log::info!("[perf] wrote {} ({}s)", path.display(), duration_s);
    Ok(())
}

fn write_done(dir: &Path, result: &Result<(), String>) {
    let signal = match result {
        Ok(()) => DoneSignal {
            status: "ok",
            error: None,
        },
        Err(e) => DoneSignal {
            status: "error",
            error: Some(e.clone()),
        },
    };
    let path = dir.join("done.json");
    if let Ok(json) = serde_json::to_string(&signal) {
        let _ = std::fs::write(&path, json);
        log::info!("[perf] wrote {}", path.display());
    }
}

fn send(cmd_tx: &UnboundedSender<Command>, cmd: Command) -> Result<(), String> {
    cmd_tx
        .send(cmd)
        .map_err(|_| "command channel closed (client exited)".to_string())
}

fn drain_for(event_rx: &mpsc::Receiver<Event>, dur: Duration, state: &mut RunState) {
    let deadline = Instant::now() + dur;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        match event_rx.recv_timeout(remaining) {
            Ok(ev) => state.observe(&ev),
            Err(mpsc::RecvTimeoutError::Timeout) => return,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn expect_event(
    event_rx: &mpsc::Receiver<Event>,
    want: &str,
    timeout: Duration,
    state: &mut RunState,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "timed out waiting for event {want} after {timeout:?}"
            ));
        }
        match event_rx.recv_timeout(remaining) {
            Ok(ev) => {
                state.observe(&ev);
                if event_type(&ev) == want {
                    log::info!("[perf] matched event: {want}");
                    return Ok(());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(format!(
                    "timed out waiting for event {want} after {timeout:?}"
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("event channel disconnected (client exited)".to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mello_core::crew::Crew;

    fn crew(id: &str) -> Crew {
        Crew {
            id: id.to_string(),
            name: "Canary Smoke".into(),
            description: String::new(),
            member_count: 1,
            max_members: 6,
            open: false,
            avatar_url: None,
        }
    }

    /// `delete_crew` with no id cleans up whatever this run created, so the id
    /// has to survive the `expect_event` that discards non-matching events.
    #[test]
    fn observe_captures_the_created_crew_id() {
        let mut state = RunState::default();
        assert!(state.created_crew_id.is_none());

        state.observe(&Event::CrewCreated {
            crew: crew("crew-123"),
            invite_code: None,
        });

        assert_eq!(state.created_crew_id.as_deref(), Some("crew-123"));
    }

    #[test]
    fn observe_ignores_unrelated_events() {
        let mut state = RunState::default();
        state.observe(&Event::CrewDeleted {
            crew_id: "crew-123".into(),
        });
        assert!(state.created_crew_id.is_none());
    }
}
