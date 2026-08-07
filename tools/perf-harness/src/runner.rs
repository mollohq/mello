use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use mello_core::{Client, Command, Config, Event, MelloStats};
use perf_scenarios::{event_type, Scenario, Step};
use tokio::runtime::Runtime;
use tokio::sync::mpsc as tokio_mpsc;

use crate::inject_loop::{start_inject_loop, InjectLoopHandle};
use crate::report::ScenarioResult;
use crate::sampler::{collect_for, summarize};
use crate::wav_player::{read_wav_mono_48k_i16, FrameMixer};

pub struct ScenarioRunOutput {
    pub result: ScenarioResult,
}

pub use perf_scenarios::expand_env;

pub fn run_scenario(path: &str, cfg: Config) -> Result<ScenarioRunOutput, Box<dyn Error>> {
    let raw = std::fs::read_to_string(path)?;
    let raw = expand_env(&raw);
    let scenario: Scenario = serde_json::from_str(&raw)?;

    println!(
        "== perf scenario: {} ({} steps) ==",
        scenario.name,
        scenario.steps.len()
    );

    let runtime = Runtime::new()?;
    let rt_handle = runtime.handle().clone();

    let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel::<Command>();
    let (event_tx, event_rx) = mpsc::channel::<Event>();

    let frame_slot: mello_core::FrameSlot = Arc::new(Mutex::new(None));
    let native_frame_slot: mello_core::NativeFrameSlot = Arc::new(Mutex::new(None));
    let frame_consumed = Arc::new(AtomicBool::new(true));
    let frame_lifecycle = Arc::new(AtomicU8::new(mello_core::FRAME_STATE_PRESENTED));

    runtime.spawn(async move {
        let mut client = Client::new_with_game_sensor(
            cfg,
            event_tx,
            false,
            frame_slot,
            native_frame_slot,
            frame_consumed,
            frame_lifecycle,
            false,
            true,
        );
        client.run(cmd_rx).await;
    });

    let pid = std::process::id();
    let mut inject: Option<InjectLoopHandle> = None;
    let mut sample_output: Option<ScenarioResult> = None;

    let result = (|| -> Result<(), String> {
        for (i, step) in scenario.steps.iter().enumerate() {
            println!("step {}: {:?}", i + 1, step);
            match step {
                Step::DeviceAuth { device_id } => send(
                    &cmd_tx,
                    Command::DeviceAuth {
                        device_id: device_id.clone(),
                    },
                )?,
                Step::Login { email, password } => send(
                    &cmd_tx,
                    Command::Login {
                        email: email.clone(),
                        password: password.clone(),
                    },
                )?,
                // Signup-only steps. This harness measures CPU/RSS of an
                // already-authenticated client, so onboarding has no meaning
                // here — the release smoke test runs them via perf_mode in the
                // real GUI client instead. Rejected loudly rather than skipped,
                // so a misdirected scenario is obvious.
                Step::DiscoverCrews | Step::FinalizeOnboarding { .. } => {
                    return Err(format!(
                        "step {:?} is only supported by the GUI scenario runner \
                         (MELLO_PERF_MODE=1); run it via scripts/run-signup-smoke.sh",
                        step
                    ));
                }
                Step::SelectCrew { crew_id } => send(
                    &cmd_tx,
                    Command::SelectCrew {
                        crew_id: crew_id.clone(),
                    },
                )?,
                Step::JoinVoice { channel_id } => send(
                    &cmd_tx,
                    Command::JoinVoice {
                        channel_id: channel_id.clone(),
                    },
                )?,
                Step::LeaveVoice => send(&cmd_tx, Command::LeaveVoice)?,
                Step::SetMute { muted } => send(&cmd_tx, Command::SetMute { muted: *muted })?,
                Step::InjectWav { path, loop_source } => {
                    if path.is_empty() {
                        return Err("inject_wav path is empty (set PERF_TEST_WAV)".to_string());
                    }
                    let clean = read_wav_mono_48k_i16(path)?;
                    let mixer = FrameMixer::new(clean, *loop_source);
                    if let Some(mut existing) = inject.take() {
                        existing.stop();
                    }
                    inject = Some(start_inject_loop(&rt_handle, cmd_tx.clone(), mixer));
                }
                Step::StopInject => {
                    if let Some(mut existing) = inject.take() {
                        existing.stop();
                    }
                }
                Step::Sleep { ms } => drain_for(&event_rx, Duration::from_millis(*ms)),
                Step::ExpectEvent { event, timeout_ms } => {
                    expect_event(&event_rx, event, Duration::from_millis(*timeout_ms))?;
                }
                Step::Sample { duration_s, label } => {
                    let id = if label.is_empty() {
                        scenario.name.clone()
                    } else {
                        label.clone()
                    };
                    println!("  sampling pid={} for {}s...", pid, duration_s);
                    let samples = collect_for(
                        pid,
                        Duration::from_secs(*duration_s),
                        Duration::from_secs(1),
                    );
                    let summary = summarize(&samples);
                    let mut result = ScenarioResult::from_summary(id, *duration_s, &summary);
                    result.mello_stats_last = last_stats(&event_rx);
                    sample_output = Some(result);
                }
            }
        }
        Ok(())
    })();

    if let Some(mut existing) = inject.take() {
        existing.stop();
    }

    result.map_err(|e| -> Box<dyn Error> { e.into() })?;

    let result = sample_output.ok_or("scenario did not include a sample step")?;
    Ok(ScenarioRunOutput { result })
}

fn last_stats(event_rx: &mpsc::Receiver<Event>) -> Option<MelloStats> {
    let mut last = None;
    while let Ok(ev) = event_rx.try_recv() {
        if let Event::StatsUpdated { stats } = ev {
            last = Some(stats);
        }
    }
    last
}

fn send(cmd_tx: &tokio_mpsc::UnboundedSender<Command>, cmd: Command) -> Result<(), String> {
    cmd_tx
        .send(cmd)
        .map_err(|_| "command channel closed (client exited)".to_string())
}

fn drain_for(event_rx: &mpsc::Receiver<Event>, dur: Duration) {
    let deadline = Instant::now() + dur;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        match event_rx.recv_timeout(remaining) {
            Ok(ev) => {
                let _ = event_type(&ev);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => return,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn expect_event(
    event_rx: &mpsc::Receiver<Event>,
    want: &str,
    timeout: Duration,
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
                let ty = event_type(&ev);
                if ty == want {
                    println!("  matched expected event: {want}");
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
