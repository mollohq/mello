use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc;
use std::time::Duration;

use crate::protocol::{HudAction, HudMessage, ACTION_PIPE_NAME, STATE_PIPE_NAME};

/// Manages the named pipe connection to the main m3llo client.
pub struct IpcClient;

impl IpcClient {
    /// Spawn background threads that connect to the named pipes and relay
    /// state messages / actions. Returns the state receiver and action sender.
    pub fn connect() -> (mpsc::Receiver<HudMessage>, mpsc::Sender<HudAction>) {
        let (state_tx, state_rx) = mpsc::channel::<HudMessage>();
        let (action_tx, action_rx) = mpsc::channel::<HudAction>();

        // Reader thread: state pipe (server → HUD)
        std::thread::spawn(move || {
            state_pipe_loop(state_tx);
        });

        // Writer thread: action pipe (HUD → server)
        std::thread::spawn(move || {
            action_pipe_loop(action_rx);
        });

        (state_rx, action_tx)
    }
}

#[cfg(target_os = "windows")]
fn state_pipe_loop(state_tx: mpsc::Sender<HudMessage>) {
    use std::fs::OpenOptions;

    log::info!("[ipc] connecting to state pipe {}", STATE_PIPE_NAME);

    let file = loop {
        match OpenOptions::new().read(true).open(STATE_PIPE_NAME) {
            Ok(f) => break f,
            Err(e) => {
                log::debug!("[ipc] state pipe not ready, retrying in 200ms: {}", e);
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    };
    log::info!("[ipc] connected to state pipe");

    let reader = BufReader::new(file);
    for line in reader.lines() {
        match line {
            Ok(text) if !text.is_empty() => match serde_json::from_str::<HudMessage>(&text) {
                Ok(msg) => {
                    log::info!("[ipc] received: {:?}", std::mem::discriminant(&msg));
                    if state_tx.send(msg).is_err() {
                        log::info!("[ipc] state receiver dropped, exiting");
                        break;
                    }
                }
                Err(e) => {
                    log::warn!(
                        "[ipc] bad message: {} — {:?}",
                        e,
                        &text[..text.len().min(200)]
                    );
                }
            },
            Err(e) => {
                log::warn!("[ipc] state pipe read error: {}", e);
                break;
            }
            _ => {}
        }
    }

    log::info!("[ipc] state pipe disconnected, exiting");
    std::process::exit(0);
}

#[cfg(target_os = "windows")]
fn action_pipe_loop(action_rx: mpsc::Receiver<HudAction>) {
    use std::fs::OpenOptions;

    log::info!("[ipc] connecting to action pipe {}", ACTION_PIPE_NAME);

    let mut file = loop {
        match OpenOptions::new().write(true).open(ACTION_PIPE_NAME) {
            Ok(f) => break f,
            Err(e) => {
                log::debug!("[ipc] action pipe not ready, retrying in 200ms: {}", e);
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    };
    log::info!("[ipc] connected to action pipe");

    while let Ok(action) = action_rx.recv() {
        if let Ok(json) = serde_json::to_string(&action) {
            let line = format!("{}\n", json);
            if let Err(e) = file.write_all(line.as_bytes()) {
                log::warn!("[ipc] action pipe write error: {}", e);
                break;
            }
        }
    }
    log::info!("[ipc] action pipe closed");
}

#[cfg(not(target_os = "windows"))]
fn state_pipe_loop(_state_tx: mpsc::Sender<HudMessage>) {
    log::warn!("[ipc] HUD IPC is only supported on Windows");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

#[cfg(not(target_os = "windows"))]
fn action_pipe_loop(_action_rx: mpsc::Receiver<HudAction>) {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
