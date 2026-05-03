mod inject_loop;
mod wav_player;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use inject_loop::{start_inject_loop, InjectLoopHandle};
use mello_core::{Client, Command, Config, Event, NsMode};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::mpsc as tokio_mpsc;
use wav_player::{read_wav_mono_48k_i16, FrameMixer};

slint::slint! {
import { VerticalBox, HorizontalBox, Button, LineEdit, Slider, CheckBox } from "std-widgets.slint";

export component VoiceTestWindow inherits Window {
    width: 980px;
    height: 760px;
    title: "Mello Voice Test Client";

    in-out property <string> status_text: "Ready";
    in-out property <string> telemetry_text: "";
    in-out property <string> event_log: "";
    in-out property <string> device_id;
    in-out property <string> email;
    in-out property <string> password;
    in-out property <string> crew_id;
    in-out property <string> channel_id;
    in-out property <string> clean_wav_path;
    in-out property <string> noise_wav_path;
    in-out property <float> noise_gain: 0.25;
    in-out property <bool> loop_source: true;
    in-out property <bool> transient_enabled: false;
    in-out property <bool> high_pass_enabled: false;
    in-out property <bool> debug_enabled: false;
    in-out property <int> selected_ns_mode: 1;

    callback login_device();
    callback login_email();
    callback join_voice();
    callback leave_voice();
    callback set_ns_mode(mode: int);
    callback set_transient();
    callback set_high_pass();
    callback toggle_debug();
    callback browse_clean_wav();
    callback browse_noise_wav();
    callback start_inject();
    callback stop_inject();

    VerticalBox {
        spacing: 8px;
        padding: 10px;

        Text {
            text: root.status_text;
            wrap: word-wrap;
        }

        HorizontalBox {
            Text { text: "Device ID"; width: 90px; }
            LineEdit { text <=> root.device_id; }
            Button { text: "Login (DeviceAuth)"; clicked => { root.login_device(); } }
        }

        HorizontalBox {
            Text { text: "Email"; width: 90px; }
            LineEdit { text <=> root.email; }
            Text { text: "Password"; width: 90px; }
            LineEdit { text <=> root.password; input-type: InputType.password; }
            Button { text: "Login (Email)"; clicked => { root.login_email(); } }
        }

        HorizontalBox {
            Text { text: "Crew ID"; width: 90px; }
            LineEdit { text <=> root.crew_id; }
            Text { text: "Channel ID"; width: 90px; }
            LineEdit { text <=> root.channel_id; }
            Button { text: "Join Voice"; clicked => { root.join_voice(); } }
            Button { text: "Leave Voice"; clicked => { root.leave_voice(); } }
        }

        HorizontalBox {
            CheckBox {
                text: "Debug stats";
                checked <=> root.debug_enabled;
                toggled => { root.toggle_debug(); }
            }
            CheckBox {
                text: "Transient suppression";
                checked <=> root.transient_enabled;
                toggled => { root.set_transient(); }
            }
            CheckBox {
                text: "High-pass filter";
                checked <=> root.high_pass_enabled;
                toggled => { root.set_high_pass(); }
            }
        }

        HorizontalBox {
            Text { text: "NS Mode"; width: 90px; }
            Button {
                text: root.selected_ns_mode == 0 ? "[Off]" : "Off";
                clicked => { root.selected_ns_mode = 0; root.set_ns_mode(0); }
            }
            Button {
                text: root.selected_ns_mode == 1 ? "[RNNoise]" : "RNNoise";
                clicked => { root.selected_ns_mode = 1; root.set_ns_mode(1); }
            }
            Button {
                text: root.selected_ns_mode == 2 ? "[WebRTC Low]" : "WebRTC Low";
                clicked => { root.selected_ns_mode = 2; root.set_ns_mode(2); }
            }
            Button {
                text: root.selected_ns_mode == 3 ? "[WebRTC Moderate]" : "WebRTC Moderate";
                clicked => { root.selected_ns_mode = 3; root.set_ns_mode(3); }
            }
            Button {
                text: root.selected_ns_mode == 4 ? "[WebRTC High]" : "WebRTC High";
                clicked => { root.selected_ns_mode = 4; root.set_ns_mode(4); }
            }
            Button {
                text: root.selected_ns_mode == 5 ? "[WebRTC Very High]" : "WebRTC Very High";
                clicked => { root.selected_ns_mode = 5; root.set_ns_mode(5); }
            }
        }

        HorizontalBox {
            Text { text: "Clean WAV"; width: 90px; }
            LineEdit { text <=> root.clean_wav_path; }
            Button { text: "Browse"; clicked => { root.browse_clean_wav(); } }
        }

        HorizontalBox {
            Text { text: "Noise WAV"; width: 90px; }
            LineEdit { text <=> root.noise_wav_path; }
            Button { text: "Browse"; clicked => { root.browse_noise_wav(); } }
        }

        HorizontalBox {
            Text { text: "Noise Gain"; width: 90px; }
            Slider { value <=> root.noise_gain; minimum: 0.0; maximum: 1.0; }
            CheckBox { text: "Loop source"; checked <=> root.loop_source; }
            Button { text: "Start Inject"; clicked => { root.start_inject(); } }
            Button { text: "Stop Inject"; clicked => { root.stop_inject(); } }
        }

        Rectangle {
            border-width: 1px;
            height: 120px;
            Text {
                text: root.telemetry_text;
                wrap: word-wrap;
            }
        }

        Rectangle {
            border-width: 1px;
            height: 260px;
            Text {
                text: root.event_log;
                wrap: word-wrap;
            }
        }
    }
}
}

fn send_command(rt: &Handle, cmd_tx: &tokio_mpsc::Sender<Command>, cmd: Command) {
    let tx = cmd_tx.clone();
    rt.spawn(async move {
        let _ = tx.send(cmd).await;
    });
}

fn push_log(window: &VoiceTestWindow, logs: &RcLog, message: String) {
    let mut lines = logs.borrow_mut();
    if lines.len() >= 18 {
        let _ = lines.pop_front();
    }
    lines.push_back(message);
    let text = lines.iter().cloned().collect::<Vec<_>>().join("\n");
    window.set_event_log(text.into());
}

type RcLog = std::rc::Rc<RefCell<VecDeque<String>>>;

fn default_device_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    format!("voice-test-{}", millis)
}

fn map_ns_mode(mode: i32) -> NsMode {
    match mode {
        0 => NsMode::Off,
        1 => NsMode::Rnnoise,
        2 => NsMode::WebRtcLow,
        3 => NsMode::WebRtcModerate,
        4 => NsMode::WebRtcHigh,
        5 => NsMode::WebRtcVeryHigh,
        _ => NsMode::Rnnoise,
    }
}

fn parse_env_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn build_config() -> Config {
    let mut cfg = if std::env::var("VOICE_TEST_PRODUCTION").is_ok() {
        Config::production()
    } else {
        Config::development()
    };

    if let Ok(v) = std::env::var("NAKAMA_HOST") {
        if !v.is_empty() {
            cfg.nakama_host = v;
        }
    }
    if let Ok(v) = std::env::var("NAKAMA_PORT") {
        if let Ok(port) = v.parse::<u16>() {
            cfg.nakama_port = port;
        }
    }
    if let Ok(v) = std::env::var("NAKAMA_SSL") {
        if let Some(ssl) = parse_env_bool(&v) {
            cfg.nakama_ssl = ssl;
        }
    }
    if let Ok(v) = std::env::var("NAKAMA_SERVER_KEY") {
        if !v.is_empty() {
            cfg.nakama_key = v;
        }
    }
    if let Ok(v) = std::env::var("NAKAMA_HTTP_KEY") {
        if !v.is_empty() {
            cfg.nakama_http_key = v;
        }
    }

    cfg
}

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let runtime = Runtime::new()?;
    let rt_handle = runtime.handle().clone();

    let (cmd_tx, cmd_rx) = tokio_mpsc::channel::<Command>(512);
    let (event_tx, event_rx) = mpsc::channel::<Event>();
    let (inject_status_tx, inject_status_rx) = mpsc::channel::<String>();

    let frame_slot: mello_core::FrameSlot = Arc::new(Mutex::new(None));
    let native_frame_slot: mello_core::NativeFrameSlot = Arc::new(Mutex::new(None));
    let frame_consumed = Arc::new(AtomicBool::new(true));
    let frame_lifecycle = Arc::new(AtomicU8::new(mello_core::FRAME_STATE_PRESENTED));
    let cfg = build_config();

    let cfg_for_client = cfg.clone();
    runtime.spawn(async move {
        let mut client = Client::new_with_game_sensor(
            cfg_for_client,
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

    let window = VoiceTestWindow::new()?;
    window.set_device_id(default_device_id().into());
    window.set_crew_id("1026b922-0080-498a-b603-2fb5ee0e002b".into());
    window.set_channel_id("ch_07xpw0ij".into());
    window.set_status_text(
        "Client started. Login with DeviceAuth or Email+Password, then join voice.".into(),
    );

    let logs: RcLog = std::rc::Rc::new(RefCell::new(VecDeque::new()));
    let pending_telemetry = std::rc::Rc::new(RefCell::new(None::<String>));
    let last_telemetry_update = std::rc::Rc::new(RefCell::new(Instant::now()));
    let inject_handle: Arc<Mutex<Option<InjectLoopHandle>>> = Arc::new(Mutex::new(None));
    let event_rx = std::rc::Rc::new(RefCell::new(event_rx));
    let inject_status_rx = std::rc::Rc::new(RefCell::new(inject_status_rx));

    push_log(
        &window,
        &logs,
        "Tip: set VOICE_TEST_PRODUCTION=1 for prod backend".to_string(),
    );
    push_log(
        &window,
        &logs,
        format!(
            "Config: {}://{}:{}",
            if cfg.nakama_ssl { "https" } else { "http" },
            cfg.nakama_host,
            cfg.nakama_port
        ),
    );
    if cfg.nakama_key == "defaultkey" {
        push_log(
            &window,
            &logs,
            "Warning: NAKAMA_SERVER_KEY is defaultkey. Export real production key.".to_string(),
        );
    }

    {
        let cmd_tx = cmd_tx.clone();
        let rt = rt_handle.clone();
        let ui = window.as_weak();
        window.on_login_device(move || {
            if let Some(window) = ui.upgrade() {
                let device_id = window.get_device_id().to_string();
                send_command(&rt, &cmd_tx, Command::DeviceAuth { device_id });
            }
        });
    }

    {
        let cmd_tx = cmd_tx.clone();
        let rt = rt_handle.clone();
        let ui = window.as_weak();
        window.on_login_email(move || {
            if let Some(window) = ui.upgrade() {
                let email = window.get_email().to_string();
                let password = window.get_password().to_string();
                if email.is_empty() || password.is_empty() {
                    window.set_status_text("Email and password are required".into());
                    return;
                }
                send_command(&rt, &cmd_tx, Command::Login { email, password });
            }
        });
    }

    {
        let cmd_tx = cmd_tx.clone();
        let rt = rt_handle.clone();
        let ui = window.as_weak();
        window.on_join_voice(move || {
            if let Some(window) = ui.upgrade() {
                let crew_id = window.get_crew_id().to_string();
                let channel_id = window.get_channel_id().to_string();
                if crew_id.is_empty() || channel_id.is_empty() {
                    window.set_status_text("Crew ID and Channel ID are required".into());
                    return;
                }

                send_command(
                    &rt,
                    &cmd_tx,
                    Command::SetDebugMode {
                        enabled: window.get_debug_enabled(),
                    },
                );
                send_command(
                    &rt,
                    &cmd_tx,
                    Command::SelectCrew {
                        crew_id: crew_id.clone(),
                    },
                );
                send_command(&rt, &cmd_tx, Command::JoinVoice { channel_id });
                window.set_status_text(format!("Joining voice in crew {}...", crew_id).into());
            }
        });
    }

    {
        let cmd_tx = cmd_tx.clone();
        let rt = rt_handle.clone();
        window.on_leave_voice(move || {
            send_command(&rt, &cmd_tx, Command::LeaveVoice);
        });
    }

    {
        let cmd_tx = cmd_tx.clone();
        let rt = rt_handle.clone();
        window.on_set_ns_mode(move |mode| {
            send_command(
                &rt,
                &cmd_tx,
                Command::SetNsMode {
                    mode: map_ns_mode(mode),
                },
            );
        });
    }

    {
        let cmd_tx = cmd_tx.clone();
        let rt = rt_handle.clone();
        let ui = window.as_weak();
        window.on_set_transient(move || {
            if let Some(window) = ui.upgrade() {
                send_command(
                    &rt,
                    &cmd_tx,
                    Command::SetTransientSuppression {
                        enabled: window.get_transient_enabled(),
                    },
                );
            }
        });
    }

    {
        let cmd_tx = cmd_tx.clone();
        let rt = rt_handle.clone();
        let ui = window.as_weak();
        window.on_set_high_pass(move || {
            if let Some(window) = ui.upgrade() {
                send_command(
                    &rt,
                    &cmd_tx,
                    Command::SetHighPassFilter {
                        enabled: window.get_high_pass_enabled(),
                    },
                );
            }
        });
    }

    {
        let cmd_tx = cmd_tx.clone();
        let rt = rt_handle.clone();
        let ui = window.as_weak();
        window.on_toggle_debug(move || {
            if let Some(window) = ui.upgrade() {
                send_command(
                    &rt,
                    &cmd_tx,
                    Command::SetDebugMode {
                        enabled: window.get_debug_enabled(),
                    },
                );
            }
        });
    }

    {
        let ui = window.as_weak();
        window.on_browse_clean_wav(move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("WAV", &["wav"])
                .pick_file()
            {
                let value = path.to_string_lossy().to_string();
                if let Some(window) = ui.upgrade() {
                    window.set_clean_wav_path(value.into());
                }
            }
        });
    }

    {
        let ui = window.as_weak();
        window.on_browse_noise_wav(move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("WAV", &["wav"])
                .pick_file()
            {
                let value = path.to_string_lossy().to_string();
                if let Some(window) = ui.upgrade() {
                    window.set_noise_wav_path(value.into());
                }
            }
        });
    }

    {
        let cmd_tx = cmd_tx.clone();
        let rt = rt_handle.clone();
        let inject_handle = Arc::clone(&inject_handle);
        let inject_status_tx = inject_status_tx.clone();
        let ui = window.as_weak();
        window.on_start_inject(move || {
            let Some(window) = ui.upgrade() else {
                return;
            };

            let clean_path = window.get_clean_wav_path().to_string();
            if clean_path.is_empty() {
                window.set_status_text("Clean WAV path is required".into());
                return;
            }

            let clean = match read_wav_mono_48k_i16(&clean_path) {
                Ok(samples) => samples,
                Err(e) => {
                    window.set_status_text(e.into());
                    return;
                }
            };

            let noise = {
                let p = window.get_noise_wav_path().to_string();
                if p.is_empty() {
                    None
                } else {
                    match read_wav_mono_48k_i16(&p) {
                        Ok(samples) => Some(samples),
                        Err(e) => {
                            window.set_status_text(e.into());
                            return;
                        }
                    }
                }
            };

            let mixer = FrameMixer::new(
                clean,
                noise,
                window.get_noise_gain(),
                window.get_loop_source(),
            );

            if let Ok(mut guard) = inject_handle.lock() {
                if let Some(existing) = guard.as_mut() {
                    existing.stop();
                }
                let loop_handle =
                    start_inject_loop(&rt, cmd_tx.clone(), mixer, inject_status_tx.clone());
                *guard = Some(loop_handle);
                window.set_status_text("Injection loop started".into());
            } else {
                window.set_status_text("Failed to acquire inject loop lock".into());
            }
        });
    }

    {
        let inject_handle = Arc::clone(&inject_handle);
        let ui = window.as_weak();
        window.on_stop_inject(move || {
            if let Ok(mut guard) = inject_handle.lock() {
                if let Some(existing) = guard.as_mut() {
                    existing.stop();
                }
                *guard = None;
            }
            if let Some(window) = ui.upgrade() {
                window.set_status_text("Injection loop stopped".into());
            }
        });
    }

    let poll_timer = slint::Timer::default();
    poll_timer.start(slint::TimerMode::Repeated, Duration::from_millis(50), {
        let logs = logs.clone();
        let pending_telemetry = pending_telemetry.clone();
        let last_telemetry_update = last_telemetry_update.clone();
        let event_rx = event_rx.clone();
        let inject_status_rx = inject_status_rx.clone();
        let ui = window.as_weak();
        move || {
            let Some(window) = ui.upgrade() else {
                return;
            };
            loop {
                match event_rx.borrow_mut().try_recv() {
                    Ok(event) => match event {
                        Event::LoggedIn { user } => {
                            window.set_status_text(
                                format!("Logged in as {} ({})", user.display_name, user.id).into(),
                            );
                            push_log(
                                &window,
                                &logs,
                                format!("login: {} ({})", user.display_name, user.id),
                            );
                        }
                        Event::DeviceAuthed { user, created } => {
                            let msg = if created {
                                format!(
                                    "Device auth created account {} ({})",
                                    user.display_name, user.id
                                )
                            } else {
                                format!("Device auth ok: {} ({})", user.display_name, user.id)
                            };
                            window.set_status_text(msg.clone().into());
                            push_log(&window, &logs, msg);
                        }
                        Event::LoginFailed { reason } => {
                            let msg = format!("login failed: {}", reason);
                            window.set_status_text(msg.clone().into());
                            push_log(&window, &logs, msg);
                        }
                        Event::VoiceStateChanged { in_call } => {
                            let msg = if in_call {
                                "Voice connected"
                            } else {
                                "Voice disconnected"
                            };
                            window.set_status_text(msg.into());
                            push_log(&window, &logs, msg.to_string());
                        }
                        Event::VoiceJoined {
                            crew_id,
                            channel_id,
                            members,
                        } => {
                            push_log(
                                &window,
                                &logs,
                                format!(
                                    "voice joined: crew={} channel={} members={}",
                                    crew_id,
                                    channel_id,
                                    members.len()
                                ),
                            );
                        }
                        Event::AudioDebugStats {
                            input_level,
                            silero_vad_prob,
                            rnnoise_prob,
                            is_speaking,
                            packets_encoded,
                            incoming_streams,
                            pipeline_delay_ms,
                            rtt_ms,
                            ..
                        } => {
                            let telemetry = format!(
                                "input={:.3} vad={:.3} rnnoise={:.3} speaking={} pkts={} streams={} delay_ms={:.1} rtt_ms={:.1}",
                                input_level,
                                silero_vad_prob,
                                rnnoise_prob,
                                is_speaking,
                                packets_encoded,
                                incoming_streams,
                                pipeline_delay_ms,
                                rtt_ms
                            );
                            *pending_telemetry.borrow_mut() = Some(telemetry);
                        }
                        Event::Error { message } => {
                            window.set_status_text(format!("Error: {}", message).into());
                            push_log(&window, &logs, format!("error: {}", message));
                        }
                        _ => {}
                    },
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }

            loop {
                match inject_status_rx.borrow_mut().try_recv() {
                    Ok(msg) => {
                        window.set_status_text(msg.clone().into());
                        push_log(&window, &logs, msg);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }

            let now = Instant::now();
            if now.duration_since(*last_telemetry_update.borrow()) >= Duration::from_millis(500) {
                if let Some(telemetry) = pending_telemetry.borrow_mut().take() {
                    window.set_telemetry_text(telemetry.into());
                    *last_telemetry_update.borrow_mut() = now;
                }
            }
        }
    });

    window.run()?;

    if let Ok(mut guard) = inject_handle.lock() {
        if let Some(existing) = guard.as_mut() {
            existing.stop();
        }
        *guard = None;
    }

    Ok(())
}
