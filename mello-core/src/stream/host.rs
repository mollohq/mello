use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::nakama::NakamaClient;

use super::config::StreamConfig;
use super::error::StreamError;
use super::manager::{StreamManager, StreamSession};
use super::sink::PacketSink;
use super::sink_p2p::P2PFanoutSink;

#[derive(Debug, Serialize)]
pub struct StartStreamRequest {
    pub crew_id: String,
    #[serde(default)]
    pub supports_av1: bool,
}

#[derive(Debug, Deserialize)]
pub struct StartStreamResponse {
    pub session_id: Option<String>,
    pub stream_id: Option<String>,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub max_viewers: Option<u32>,
    #[serde(default)]
    pub sfu_endpoint: Option<String>,
    #[serde(default)]
    pub sfu_token: Option<String>,
}

fn default_mode() -> String {
    "p2p".to_string()
}

impl StartStreamResponse {
    pub fn session_id(&self) -> String {
        self.session_id
            .clone()
            .or_else(|| self.stream_id.clone())
            .unwrap_or_default()
    }
}

/// Call the backend RPC to start a stream and get topology info.
/// This is a separate async step so raw pointers don't cross await points.
pub async fn request_start_stream(
    nakama: &NakamaClient,
    crew_id: &str,
    supports_av1: bool,
) -> Result<StartStreamResponse, StreamError> {
    let req = StartStreamRequest {
        crew_id: crew_id.to_string(),
        supports_av1,
    };
    let payload = serde_json::to_value(&req).map_err(|e| StreamError::Backend(e.to_string()))?;

    let resp_str = nakama
        .rpc("start_stream", &payload)
        .await
        .map_err(|e| StreamError::Backend(e.to_string()))?;

    let resp: StartStreamResponse =
        serde_json::from_str(&resp_str).map_err(|e| StreamError::Backend(e.to_string()))?;

    log::info!(
        "Backend returned stream session_id={}, mode={}",
        resp.session_id(),
        resp.mode
    );

    Ok(resp)
}

/// Create the sink and manager based on the backend response, then spawn the run loop.
/// This is synchronous (no await) so raw pointers are safe.
pub fn create_stream_session(
    ctx: *mut mello_sys::MelloContext,
    host: *mut mello_sys::MelloStreamHost,
    resp: &StartStreamResponse,
    config: StreamConfig,
) -> Result<StreamSession, StreamError> {
    let session_id = resp.session_id();
    let mode = resp.mode.clone();

    let sink: Arc<dyn PacketSink> = match mode.as_str() {
        "p2p" => Arc::new(P2PFanoutSink::new()),
        "sfu" => {
            // SfuSink::new is async but always returns Err for now.
            // We can't call async from sync, so just return the error directly.
            return Err(StreamError::SfuNotImplemented);
        }
        other => return Err(StreamError::UnknownMode(other.to_string())),
    };

    let manager = StreamManager::new(ctx, host, sink, config);

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let session = StreamSession::new(session_id, mode, stop_tx);

    tokio::spawn(async move {
        let mut mgr = manager;
        mgr.run(stop_rx).await;
    });

    Ok(session)
}
