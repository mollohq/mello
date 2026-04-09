use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command as StdCommand};
use std::sync::mpsc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

// ── IPC protocol types (mirrored from hud/src/protocol.rs) ────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HudMessage {
    #[serde(rename = "state")]
    State(Box<HudState>),
    #[serde(rename = "settings")]
    Settings(HudSettings),
    #[serde(rename = "shutdown")]
    Shutdown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HudState {
    pub mode: HudMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crew: Option<HudCrew>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<HudVoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_messages: Option<Vec<HudChatMessage>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_card: Option<HudStreamCard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip_toast: Option<HudClipToast>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HudMode {
    #[default]
    Hidden,
    MiniPlayer,
    Overlay,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudCrew {
    pub name: String,
    pub initials: String,
    pub online_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudVoice {
    pub channel_name: String,
    pub members: Vec<HudVoiceMember>,
    pub self_muted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudVoiceMember {
    pub id: String,
    pub display_name: String,
    pub initials: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_rgba: Option<String>,
    pub speaking: bool,
    pub muted: bool,
    #[serde(default)]
    pub is_self: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudChatMessage {
    pub display_name: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudStreamCard {
    pub streamer: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudClipToast {
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudSettings {
    pub overlay_opacity: f32,
    pub show_clip_toasts: bool,
    pub overlay_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HudActionKind {
    MuteToggle,
    DeafenToggle,
    LeaveVoice,
    OpenCrew,
    OpenStream,
    OpenSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HudAction {
    #[serde(rename = "action")]
    Action { action: HudActionKind },
}

// ── HudManager ────────────────────────────────────────────────────────────

/// Manages the m3llo-hud.exe child process: spawning, IPC, crash respawn.
pub struct HudManager {
    state_tx: mpsc::Sender<HudMessage>,
    action_rx: mpsc::Receiver<HudAction>,
    enabled: bool,
}

impl HudManager {
    /// Start the HUD manager. Spawns the named pipe server thread and the HUD
    /// child process. Returns the manager handle.
    pub fn start(enabled: bool) -> Self {
        let (state_tx, state_internal_rx) = mpsc::channel::<HudMessage>();
        let (action_internal_tx, action_rx) = mpsc::channel::<HudAction>();

        if enabled {
            // Spawn the pipe server + process manager on a background thread
            std::thread::spawn(move || {
                server_loop(state_internal_rx, action_internal_tx);
            });
        }

        Self {
            state_tx,
            action_rx,
            enabled,
        }
    }

    /// Push a state update to the HUD process.
    pub fn push_state(&self, state: HudState) {
        if !self.enabled {
            return;
        }
        log::debug!(
            "[hud_mgr] push_state: mode={:?} crew={} voice_members={}",
            state.mode,
            state.crew.is_some(),
            state.voice.as_ref().map_or(0, |v| v.members.len()),
        );
        if let Err(e) = self.state_tx.send(HudMessage::State(Box::new(state))) {
            log::error!("[hud_mgr] send failed (server thread dead?): {}", e);
        }
    }

    /// Push a settings update to the HUD process.
    pub fn push_settings(&self, settings: HudSettings) {
        if !self.enabled {
            return;
        }
        let _ = self.state_tx.send(HudMessage::Settings(settings));
    }

    /// Poll for user actions from the HUD.
    pub fn poll_action(&self) -> Option<HudAction> {
        self.action_rx.try_recv().ok()
    }

    /// Send shutdown and stop managing.
    pub fn shutdown(&self) {
        let _ = self.state_tx.send(HudMessage::Shutdown);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

// ── Pipe server + process management ──────────────────────────────────────

#[cfg(target_os = "windows")]
fn server_loop(state_rx: mpsc::Receiver<HudMessage>, action_tx: mpsc::Sender<HudAction>) {
    kill_zombie_hud_processes();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        server_loop_inner(&state_rx, &action_tx);
    }));
    if let Err(e) = result {
        log::error!("[hud_mgr] server thread PANICKED: {:?}", e);
    }
}

#[cfg(target_os = "windows")]
fn server_loop_inner(state_rx: &mpsc::Receiver<HudMessage>, action_tx: &mpsc::Sender<HudAction>) {
    let mut spawn_failures = 0u32;

    loop {
        let hud_exe = hud_exe_path();
        log::info!("[hud_mgr] spawning HUD process: {}", hud_exe.display());
        let mut child = match StdCommand::new(&hud_exe).spawn() {
            Ok(c) => {
                log::info!("[hud_mgr] HUD process started, pid={}", c.id());
                spawn_failures = 0;
                c
            }
            Err(e) => {
                spawn_failures += 1;
                if spawn_failures <= 3 {
                    log::error!("[hud_mgr] failed to spawn HUD: {}", e);
                } else if spawn_failures == 4 {
                    log::error!(
                        "[hud_mgr] failed to spawn HUD {} times, suppressing further logs",
                        spawn_failures
                    );
                }
                // Back off: 5s, 10s, 30s, then cap at 60s
                let delay = match spawn_failures {
                    1 => 5,
                    2 => 10,
                    3 => 30,
                    _ => 60,
                };
                std::thread::sleep(Duration::from_secs(delay));
                continue;
            }
        };

        match run_pipe_server(state_rx, action_tx, &mut child) {
            Ok(()) => {
                log::info!("[hud_mgr] pipe session ended normally");
            }
            Err(e) => {
                log::warn!("[hud_mgr] pipe session error: {}", e);
            }
        }

        let _ = child.kill();
        let _ = child.wait();

        if let Ok(HudMessage::Shutdown) = state_rx.try_recv() {
            log::info!("[hud_mgr] shutdown received, not respawning");
            return;
        }

        log::info!("[hud_mgr] respawning HUD in 2s");
        std::thread::sleep(Duration::from_secs(2));
    }
}

#[cfg(target_os = "windows")]
fn run_pipe_server(
    state_rx: &mpsc::Receiver<HudMessage>,
    action_tx: &mpsc::Sender<HudAction>,
    child: &mut Child,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::windows::io::FromRawHandle;
    use windows::core::*;
    use windows::Win32::Foundation::*;
    use windows::Win32::Storage::FileSystem::*;
    use windows::Win32::System::Pipes::*;

    unsafe {
        let state_pipe_name = w!(r"\\.\pipe\m3llo-hud-state");
        let action_pipe_name = w!(r"\\.\pipe\m3llo-hud-action");

        // State pipe: server writes, HUD reads
        let state_pipe = CreateNamedPipeW(
            state_pipe_name,
            PIPE_ACCESS_OUTBOUND,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            4096,
            0,
            0,
            None,
        );
        if state_pipe == INVALID_HANDLE_VALUE {
            return Err("CreateNamedPipeW (state) failed".into());
        }

        // Action pipe: HUD writes, server reads
        let action_pipe = CreateNamedPipeW(
            action_pipe_name,
            PIPE_ACCESS_INBOUND,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            0,
            4096,
            0,
            None,
        );
        if action_pipe == INVALID_HANDLE_VALUE {
            return Err("CreateNamedPipeW (action) failed".into());
        }

        log::info!("[hud_mgr] waiting for HUD to connect to pipes");

        let _ = ConnectNamedPipe(state_pipe, None);
        log::info!("[hud_mgr] HUD connected (state pipe)");

        let _ = ConnectNamedPipe(action_pipe, None);
        log::info!("[hud_mgr] HUD connected (action pipe)");

        let mut writer = std::fs::File::from_raw_handle(state_pipe.0);
        let reader = BufReader::new(std::fs::File::from_raw_handle(action_pipe.0));

        // Spawn a thread to read actions from HUD
        let action_tx_clone = action_tx.clone();
        let reader_thread = std::thread::spawn(move || {
            for line_result in reader.lines() {
                match line_result {
                    Ok(ref text) if !text.is_empty() => {
                        match serde_json::from_str::<HudAction>(text) {
                            Ok(action) => {
                                let _ = action_tx_clone.send(action);
                            }
                            Err(e) => {
                                log::warn!("[hud_mgr] bad action from HUD: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        log::debug!("[hud_mgr] reader error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        });

        log::info!("[hud_mgr] entering write loop");
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    log::warn!("[hud_mgr] HUD process exited: {:?}", status);
                    break;
                }
                Err(e) => {
                    log::warn!("[hud_mgr] failed to check child status: {}", e);
                    break;
                }
                Ok(None) => {}
            }

            match state_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(msg) => {
                    let is_shutdown = matches!(msg, HudMessage::Shutdown);
                    match serde_json::to_string(&msg) {
                        Ok(json) => {
                            let line = format!("{}\n", json);
                            if let Err(e) = writer.write_all(line.as_bytes()) {
                                log::warn!("[hud_mgr] pipe write error: {}", e);
                                break;
                            }
                        }
                        Err(e) => {
                            log::warn!("[hud_mgr] failed to serialize: {}", e);
                        }
                    }
                    if is_shutdown {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    log::info!("[hud_mgr] state sender dropped, exiting");
                    break;
                }
            }
        }

        let _ = reader_thread.join();
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
fn server_loop(_state_rx: mpsc::Receiver<HudMessage>, _action_tx: mpsc::Sender<HudAction>) {
    log::warn!("[hud_mgr] HUD manager is Windows-only");
}

/// Kill any orphaned m3llo-hud.exe processes from previous runs.
#[cfg(target_os = "windows")]
fn kill_zombie_hud_processes() {
    use std::process::Command as StdCmd;

    let output = match StdCmd::new("taskkill")
        .args(["/F", "/IM", "m3llo-hud.exe"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return,
    };
    if output.status.success() {
        let msg = String::from_utf8_lossy(&output.stdout);
        log::info!("[hud_mgr] killed zombie HUD process(es): {}", msg.trim());
    }
}

/// Resolve the path to m3llo-hud.exe, adjacent to the main binary.
fn hud_exe_path() -> std::path::PathBuf {
    let mut path = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("."));
    path.pop(); // remove mello.exe filename
    path.push("m3llo-hud");
    #[cfg(target_os = "windows")]
    path.set_extension("exe");
    path
}

/// Derive initials from a display name (max 2 chars, uppercase).
pub fn derive_initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    match words.len() {
        0 => "??".to_string(),
        1 => {
            let chars: Vec<char> = words[0].chars().collect();
            if chars.len() >= 2 {
                format!("{}{}", chars[0], chars[1]).to_uppercase()
            } else {
                format!("{}", chars[0]).to_uppercase()
            }
        }
        _ => {
            let first = words[0].chars().next().unwrap_or('?');
            let second = words[1].chars().next().unwrap_or('?');
            format!("{}{}", first, second).to_uppercase()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_two_words() {
        assert_eq!(derive_initials("Koji Tech"), "KT");
    }

    #[test]
    fn initials_single_word() {
        assert_eq!(derive_initials("nova"), "NO");
    }

    #[test]
    fn initials_empty() {
        assert_eq!(derive_initials(""), "??");
    }

    #[test]
    fn initials_single_char() {
        assert_eq!(derive_initials("X"), "X");
    }

    #[test]
    fn hud_state_serialization() {
        let state = HudState {
            mode: HudMode::Overlay,
            crew: Some(HudCrew {
                name: "The Vanguard".into(),
                initials: "TV".into(),
                online_count: 5,
            }),
            voice: Some(HudVoice {
                channel_name: "General".into(),
                members: vec![HudVoiceMember {
                    id: "abc".into(),
                    display_name: "k0ji_tech".into(),
                    initials: "KT".into(),
                    avatar_rgba: None,
                    speaking: true,
                    muted: false,
                    is_self: false,
                }],
                self_muted: false,
            }),
            recent_messages: None,
            stream_card: None,
            clip_toast: None,
        };
        let msg = HudMessage::State(Box::new(state));
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"state\""));
        assert!(json.contains("\"mode\":\"overlay\""));
        assert!(json.contains("k0ji_tech"));

        // Roundtrip
        let parsed: HudMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            HudMessage::State(ref s) => {
                assert_eq!(s.mode, HudMode::Overlay);
                assert!(s.crew.is_some());
            }
            _ => panic!("expected State"),
        }
    }

    #[test]
    fn hud_action_serialization() {
        let action = HudAction::Action {
            action: HudActionKind::MuteToggle,
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains("mute_toggle"));

        let parsed: HudAction = serde_json::from_str(&json).unwrap();
        match parsed {
            HudAction::Action { action } => {
                assert_eq!(action, HudActionKind::MuteToggle);
            }
        }
    }
}
