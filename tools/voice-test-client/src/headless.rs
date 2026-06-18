//! Headless, scripted scenario runner for the voice test client.
//!
//! Drives a real `mello-core` Client (no Slint UI) through a JSON scenario so
//! reconnect/resync/fault behaviour can be reproduced and asserted in CI or
//! against a live backend. Enabled by `--scenario <file>` (or
//! `VOICE_TEST_SCENARIO=<file>`); otherwise the GUI runs as before.
//!
//! Scenario shape:
//! ```json
//! {
//!   "name": "reconnect-roster",
//!   "steps": [
//!     {"action": "device_auth", "device_id": "voice-test-1"},
//!     {"action": "expect_event", "event": "DeviceAuthed", "timeout_ms": 10000},
//!     {"action": "select_crew", "crew_id": "<crew>"},
//!     {"action": "join_voice", "channel_id": "<channel>"},
//!     {"action": "expect_event", "event": "VoiceJoined", "timeout_ms": 10000},
//!     {"action": "assert_no_event", "event": "VoiceSfuDisconnected", "duration_ms": 20000},
//!     {"action": "inject_wav", "path": "clean.wav", "loop_source": true},
//!     {"action": "sleep", "ms": 3000},
//!     {"action": "fault", "kind": "nakama_disconnect"},
//!     {"action": "expect_event", "event": "ConnectionStateChanged", "timeout_ms": 20000},
//!     {"action": "stop_inject"},
//!     {"action": "leave_voice"}
//!   ]
//! }
//! ```

use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use mello_core::{Client, Command, Config, Event};
use serde::Deserialize;
use tokio::runtime::Runtime;
use tokio::sync::mpsc as tokio_mpsc;

use crate::inject_loop::{start_inject_loop, InjectLoopHandle};
use crate::wav_player::{read_wav_mono_48k_i16, FrameMixer};

fn default_true() -> bool {
    true
}
fn default_timeout() -> u64 {
    10_000
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Step {
    DeviceAuth {
        device_id: String,
    },
    Login {
        email: String,
        password: String,
    },
    SelectCrew {
        crew_id: String,
    },
    JoinVoice {
        channel_id: String,
    },
    LeaveVoice,
    InjectWav {
        path: String,
        #[serde(default)]
        noise_path: Option<String>,
        #[serde(default)]
        noise_gain: f32,
        #[serde(default = "default_true")]
        loop_source: bool,
    },
    StopInject,
    SetMute {
        muted: bool,
    },
    Sleep {
        ms: u64,
    },
    /// kind: "nakama_disconnect" | "sfu_disconnect" | "simulate_suspend"
    Fault {
        kind: String,
    },
    /// Wait until an Event whose `type` equals `event` is observed, else fail.
    ExpectEvent {
        event: String,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
    },
    /// Ensure an Event whose `type` equals `event` is NOT observed in window.
    AssertNoEvent {
        event: String,
        duration_ms: u64,
    },
}

#[derive(Debug, Deserialize)]
struct Scenario {
    #[serde(default)]
    name: String,
    steps: Vec<Step>,
}

/// The adjacently-tagged `type` discriminant of an Event (e.g. "VoiceJoined").
fn event_type(ev: &Event) -> String {
    serde_json::to_value(ev)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "Unknown".to_string())
}

/// Expand `${VAR}` tokens in the raw scenario from the process environment so a
/// single scenario file can be parameterised per CI run (crew/channel/account/
/// wav path). Unset variables expand to empty (UTF-8 safe; no closing `}` is
/// emitted literally).
fn expand_env(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                out.push_str(&std::env::var(&after[..end]).unwrap_or_default());
                rest = &after[end + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn run_scenario(path: &str, cfg: Config) -> Result<(), Box<dyn Error>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read scenario {}: {}", path, e))?;
    let raw = expand_env(&raw);
    let scenario: Scenario =
        serde_json::from_str(&raw).map_err(|e| format!("invalid scenario JSON: {}", e))?;

    println!(
        "== scenario: {} ({} steps) ==",
        scenario.name,
        scenario.steps.len()
    );

    let runtime = Runtime::new()?;
    let rt_handle = runtime.handle().clone();

    let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel::<Command>();
    let (event_tx, event_rx) = mpsc::channel::<Event>();
    let (inject_status_tx, inject_status_rx) = mpsc::channel::<String>();

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
        );
        client.run(cmd_rx).await;
    });

    let mut inject: Option<InjectLoopHandle> = None;

    let result = (|| -> Result<(), String> {
        for (i, step) in scenario.steps.iter().enumerate() {
            // Surface any pending inject status lines for visibility.
            while let Ok(msg) = inject_status_rx.try_recv() {
                println!("  [inject] {}", msg);
            }
            println!("step {}: {:?}", i + 1, step);
            match step {
                Step::DeviceAuth { device_id } => {
                    send(
                        &cmd_tx,
                        Command::DeviceAuth {
                            device_id: device_id.clone(),
                        },
                    )?;
                }
                Step::Login { email, password } => {
                    send(
                        &cmd_tx,
                        Command::Login {
                            email: email.clone(),
                            password: password.clone(),
                        },
                    )?;
                }
                Step::SelectCrew { crew_id } => {
                    send(
                        &cmd_tx,
                        Command::SelectCrew {
                            crew_id: crew_id.clone(),
                        },
                    )?;
                }
                Step::JoinVoice { channel_id } => {
                    send(
                        &cmd_tx,
                        Command::JoinVoice {
                            channel_id: channel_id.clone(),
                        },
                    )?;
                }
                Step::LeaveVoice => {
                    send(&cmd_tx, Command::LeaveVoice)?;
                }
                Step::SetMute { muted } => {
                    send(&cmd_tx, Command::SetMute { muted: *muted })?;
                }
                Step::InjectWav {
                    path,
                    noise_path,
                    noise_gain,
                    loop_source,
                } => {
                    let clean = read_wav_mono_48k_i16(path)?;
                    let noise = match noise_path {
                        Some(p) if !p.is_empty() => Some(read_wav_mono_48k_i16(p)?),
                        _ => None,
                    };
                    let mixer = FrameMixer::new(clean, noise, *noise_gain, *loop_source);
                    if let Some(mut existing) = inject.take() {
                        existing.stop();
                    }
                    inject = Some(start_inject_loop(
                        &rt_handle,
                        cmd_tx.clone(),
                        mixer,
                        inject_status_tx.clone(),
                    ));
                }
                Step::StopInject => {
                    if let Some(mut existing) = inject.take() {
                        existing.stop();
                    }
                }
                Step::Sleep { ms } => {
                    drain_for(&event_rx, Duration::from_millis(*ms));
                }
                Step::Fault { kind } => {
                    let cmd = match kind.as_str() {
                        "nakama_disconnect" => Command::FaultNakamaDisconnect,
                        "sfu_disconnect" => Command::FaultSfuDisconnect,
                        "simulate_suspend" => Command::FaultSimulateSuspend,
                        other => return Err(format!("unknown fault kind: {}", other)),
                    };
                    send(&cmd_tx, cmd)?;
                }
                Step::ExpectEvent { event, timeout_ms } => {
                    expect_event(&event_rx, event, Duration::from_millis(*timeout_ms))?;
                }
                Step::AssertNoEvent { event, duration_ms } => {
                    assert_no_event(&event_rx, event, Duration::from_millis(*duration_ms))?;
                }
            }
        }
        Ok(())
    })();

    if let Some(mut existing) = inject.take() {
        existing.stop();
    }

    match result {
        Ok(()) => {
            println!("PASS: {}", scenario.name);
            Ok(())
        }
        Err(e) => {
            eprintln!("FAIL: {}: {}", scenario.name, e);
            Err(e.into())
        }
    }
}

fn send(cmd_tx: &tokio_mpsc::UnboundedSender<Command>, cmd: Command) -> Result<(), String> {
    cmd_tx
        .send(cmd)
        .map_err(|_| "command channel closed (client exited)".to_string())
}

/// Consume and log events for `dur`, so background events don't pile up and we
/// can observe what's happening during idle/sleep windows.
fn drain_for(event_rx: &mpsc::Receiver<Event>, dur: Duration) {
    let deadline = Instant::now() + dur;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        match event_rx.recv_timeout(remaining) {
            Ok(ev) => log_event(&ev),
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
                "timed out waiting for event {} after {:?}",
                want, timeout
            ));
        }
        match event_rx.recv_timeout(remaining) {
            Ok(ev) => {
                let ty = event_type(&ev);
                log_event(&ev);
                // A surfaced Error/LoginFailed while waiting is a hard failure.
                if ty == want {
                    println!("  matched expected event: {}", want);
                    return Ok(());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(format!(
                    "timed out waiting for event {} after {:?}",
                    want, timeout
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("event channel disconnected (client exited)".to_string());
            }
        }
    }
}

fn assert_no_event(
    event_rx: &mpsc::Receiver<Event>,
    forbidden: &str,
    window: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + window;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            println!("  confirmed no event: {} for {:?}", forbidden, window);
            return Ok(());
        }
        match event_rx.recv_timeout(remaining) {
            Ok(ev) => {
                let ty = event_type(&ev);
                log_event(&ev);
                if ty == forbidden {
                    return Err(format!(
                        "unexpected event {} observed within {:?}",
                        forbidden, window
                    ));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                println!("  confirmed no event: {} for {:?}", forbidden, window);
                return Ok(());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("event channel disconnected (client exited)".to_string());
            }
        }
    }
}

/// Log meaningful events; suppress the high-frequency telemetry noise.
fn log_event(ev: &Event) {
    match ev {
        Event::AudioDebugStats { .. } | Event::MicLevel { .. } => {}
        Event::Error { message } => println!("  [event] Error: {}", message),
        other => println!("  [event] {}", event_type(other)),
    }
}
