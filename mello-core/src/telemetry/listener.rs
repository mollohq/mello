//! Loopback HTTP listener that receives game telemetry POSTs (e.g. CS2 GSI) and
//! forwards parsed [`TelemetryEvent`]s into the client event loop.
//!
//! Mirrors the localhost-server pattern used by the OAuth flow (`oauth.rs`):
//! a long-lived `tiny_http::Server` on a dedicated thread.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use tiny_http::{Response, Server, StatusCode};

use super::{AdapterRegistry, TelemetryEvent};

/// Loopback port telemetry adapters point their game at. Distinct from the
/// OAuth callback port (`29405`).
pub const TELEMETRY_PORT: u16 = 29406;

/// Owns the listener thread. Dropping it does not stop the thread (best-effort,
/// matches the game sensor); the process owns it for its lifetime.
pub struct TelemetryListener {
    _handle: Option<std::thread::JoinHandle<()>>,
}

impl TelemetryListener {
    /// Bind the listener and start routing inbound payloads through `registry`.
    /// Binding failure is surfaced so the caller can log and continue without
    /// telemetry (sensing and the manual post-game flow are unaffected).
    pub fn start(
        registry: Arc<AdapterRegistry>,
        token: String,
    ) -> std::io::Result<(Self, Receiver<TelemetryEvent>)> {
        let server = Server::http(format!("127.0.0.1:{TELEMETRY_PORT}"))
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let (tx, rx) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("telemetry-listener".into())
            .spawn(move || listen_loop(server, &registry, &token, &tx))?;

        log::info!("[telemetry] listener bound on 127.0.0.1:{TELEMETRY_PORT}");
        Ok((
            Self {
                _handle: Some(handle),
            },
            rx,
        ))
    }
}

fn listen_loop(
    server: Server,
    registry: &AdapterRegistry,
    token: &str,
    tx: &Sender<TelemetryEvent>,
) {
    for mut request in server.incoming_requests() {
        let mut body = String::new();
        let _ = request.as_reader().read_to_string(&mut body);

        // Acknowledge promptly so the game doesn't back-pressure on us.
        let _ = request.respond(Response::empty(StatusCode(200)));

        if body.is_empty() {
            continue;
        }

        // First adapter to claim the payload wins (others return no events).
        for adapter in registry.all() {
            let events = adapter.parse(&body, token);
            if events.is_empty() {
                continue;
            }
            for ev in events {
                if tx.send(ev).is_err() {
                    log::info!("[telemetry] receiver dropped, listener exiting");
                    return;
                }
            }
            break;
        }
    }
}
