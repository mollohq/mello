//! Linux: the ScreenCast portal for stream hosting (spec 12 section 3.1).
//!
//! The portal is the only screen capture on Wayland. Its dialog, drawn by the
//! desktop, lets the person pick a monitor or a window; the session then
//! gives a PipeWire socket and the stream's node, which libmello reads
//! (`MELLO_CAPTURE_PIPEWIRE`). The pointer is embedded in the picture.
//!
//! `persist_mode` 2 keeps a restore token, stored in
//! `$XDG_STATE_HOME/m3llo/screencast-restore-token`. With it the desktop
//! restores the last choice and skips the dialog (KDE since Plasma 5.25).
//!
//! The session stays open for the whole stream: a thread with its own async
//! runtime owns it and closes it when `ScreencastSource` drops. Closing it
//! ends the PipeWire stream, so it drops after the capture stopped
//! (`NativeTeardownGuard::then`).

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::mpsc;

use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::PersistMode;

/// An open portal session: its PipeWire socket and node. Dropping it closes
/// the session.
pub struct ScreencastSource {
    pub fd: OwnedFd,
    pub node: u32,
    /// The stream's size as the portal reports it; (0, 0) when it does not.
    pub size: (i32, i32),
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ScreencastSource {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        log::info!("[screencast] portal session closed (node {})", self.node);
    }
}

struct Started {
    fd: OwnedFd,
    node: u32,
    size: (i32, i32),
}

/// Ask the portal for a source. Blocks while the desktop's dialog is open:
/// call it on the stream start thread, never on the command loop.
pub fn open() -> Result<ScreencastSource, String> {
    let token = load_token();
    let (result_tx, result_rx) = mpsc::channel::<Result<Started, String>>();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("screencast-portal".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = result_tx.send(Err(format!("portal runtime: {e}")));
                    return;
                }
            };
            runtime.block_on(run(token, result_tx, stop_rx));
        })
        .map_err(|e| format!("portal thread: {e}"))?;

    match result_rx.recv() {
        Ok(Ok(started)) => Ok(ScreencastSource {
            fd: started.fd,
            node: started.node,
            size: started.size,
            stop: Some(stop_tx),
            thread: Some(thread),
        }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err("the screen sharing portal stopped without an answer".into())
        }
    }
}

async fn run(
    token: Option<String>,
    result_tx: mpsc::Sender<Result<Started, String>>,
    stop_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let proxy = match Screencast::new().await {
        Ok(p) => p,
        Err(e) => {
            let _ = result_tx.send(Err(format!("no screen sharing portal: {e}")));
            return;
        }
    };
    let session = match proxy.create_session().await {
        Ok(s) => s,
        Err(e) => {
            let _ = result_tx.send(Err(format!("screen sharing session: {e}")));
            return;
        }
    };
    let started = select_and_start(&proxy, &session, token.as_deref()).await;
    let ok = started.is_ok();
    let _ = result_tx.send(started);
    if ok {
        // The stream runs until ScreencastSource drops.
        let _ = stop_rx.await;
    }
    let _ = session.close().await;
}

async fn select_and_start(
    proxy: &Screencast<'_>,
    session: &ashpd::desktop::Session<'_, Screencast<'_>>,
    token: Option<&str>,
) -> Result<Started, String> {
    proxy
        .select_sources(
            session,
            CursorMode::Embedded,
            SourceType::Monitor | SourceType::Window,
            false,
            token,
            PersistMode::ExplicitlyRevoked,
        )
        .await
        .map_err(|e| format!("screen sharing source selection: {e}"))?;
    log::info!(
        "[screencast] asking the desktop for a source ({})",
        if token.is_some() {
            "restore token"
        } else {
            "dialog"
        }
    );
    let response = proxy
        .start(session, None)
        .await
        .map_err(|e| format!("screen sharing start: {e}"))?
        .response()
        .map_err(|e| match e {
            ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => {
                "Screen sharing was cancelled".to_string()
            }
            other => format!("screen sharing was refused: {other}"),
        })?;
    if let Some(new_token) = response.restore_token() {
        save_token(new_token);
    }
    let stream = response
        .streams()
        .first()
        .ok_or("the desktop chose no source")?;
    let node = stream.pipe_wire_node_id();
    let size = stream.size().unwrap_or((0, 0));
    let fd = proxy
        .open_pipe_wire_remote(session)
        .await
        .map_err(|e| format!("screen sharing PipeWire socket: {e}"))?;
    log::info!(
        "[screencast] source: node {node} {}x{} ({:?})",
        size.0,
        size.1,
        stream.source_type()
    );
    Ok(Started { fd, node, size })
}

/// `$XDG_STATE_HOME/m3llo/screencast-restore-token`, else under
/// `~/.local/state`.
fn token_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("m3llo").join("screencast-restore-token"))
}

fn load_token() -> Option<String> {
    let token = std::fs::read_to_string(token_path()?).ok()?;
    let token = token.trim().to_string();
    (!token.is_empty()).then_some(token)
}

fn save_token(token: &str) {
    let Some(path) = token_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, token) {
        log::warn!("[screencast] restore token not saved: {e}");
    }
}
