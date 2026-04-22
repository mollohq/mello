use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::nakama::NakamaClient;

use super::config::StreamConfig;
use super::error::StreamError;
use super::manager::{AudioPacket, StreamManager, StreamSession, VideoPacket};
use super::sink::PacketSink;

const VIDEO_QUEUE_CAPACITY: usize = 32;
const AUDIO_QUEUE_CAPACITY: usize = 128;

#[derive(Debug, Serialize)]
pub struct StartStreamRequest {
    pub crew_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub supports_av1: bool,
    pub width: u32,
    pub height: u32,
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
    title: &str,
    supports_av1: bool,
    width: u32,
    height: u32,
) -> Result<StartStreamResponse, StreamError> {
    let req = StartStreamRequest {
        crew_id: crew_id.to_string(),
        title: title.to_string(),
        supports_av1,
        width,
        height,
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

// ---------------------------------------------------------------------------
// C callback trampolines
// ---------------------------------------------------------------------------

struct VideoCallbackCtx {
    tx: mpsc::Sender<VideoPacket>,
    dropped: AtomicU64,
}

struct AudioCallbackCtx {
    tx: mpsc::Sender<AudioPacket>,
    dropped: AtomicU64,
}

unsafe extern "C" fn on_video_packet(
    user_data: *mut std::ffi::c_void,
    data: *const u8,
    size: i32,
    is_keyframe: bool,
    ts: u64,
) {
    let ctx = &*(user_data as *const VideoCallbackCtx);
    let payload = std::slice::from_raw_parts(data, size as usize).to_vec();
    let packet = VideoPacket {
        data: payload,
        is_keyframe,
        timestamp: ts,
    };
    if let Err(err) = ctx.tx.try_send(packet) {
        if matches!(err, mpsc::error::TrySendError::Full(_)) {
            let n = ctx.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 5 || n.is_multiple_of(120) {
                log::warn!(
                    "Stream host video queue full: dropped={} cap={}",
                    n,
                    VIDEO_QUEUE_CAPACITY
                );
            }
        }
    }
}

unsafe extern "C" fn on_audio_packet(
    user_data: *mut std::ffi::c_void,
    data: *const u8,
    size: i32,
    ts: u64,
) {
    let ctx = &*(user_data as *const AudioCallbackCtx);
    let payload = std::slice::from_raw_parts(data, size as usize).to_vec();
    let packet = AudioPacket {
        data: payload,
        timestamp: ts,
    };
    if let Err(err) = ctx.tx.try_send(packet) {
        if matches!(err, mpsc::error::TrySendError::Full(_)) {
            let n = ctx.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 5 || n.is_multiple_of(300) {
                log::warn!(
                    "Stream host audio queue full: dropped={} cap={}",
                    n,
                    AUDIO_QUEUE_CAPACITY
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stream host lifecycle
// ---------------------------------------------------------------------------

/// Holds the leaked callback contexts so they can be reclaimed on drop.
pub struct HostResources {
    video_ctx: *mut VideoCallbackCtx,
    audio_ctx: *mut AudioCallbackCtx,
}

unsafe impl Send for HostResources {}
unsafe impl Sync for HostResources {}

impl Drop for HostResources {
    fn drop(&mut self) {
        unsafe {
            drop(Box::from_raw(self.video_ctx));
            drop(Box::from_raw(self.audio_ctx));
        }
    }
}

type StartHostResult = (
    *mut mello_sys::MelloStreamHost,
    mpsc::Receiver<VideoPacket>,
    mpsc::Receiver<AudioPacket>,
    HostResources,
);

/// Start the C++ host pipeline with callback-based packet delivery.
/// Returns the host handle, channel receivers, and ownership of leaked callback contexts.
///
/// # Safety
/// `ctx` must be a valid, non-null `MelloContext` pointer returned by libmello.
pub unsafe fn start_host(
    ctx: *mut mello_sys::MelloContext,
    source: &mello_sys::MelloCaptureSource,
    config: &mello_sys::MelloStreamConfig,
) -> Result<StartHostResult, StreamError> {
    let (video_tx, video_rx) = mpsc::channel(VIDEO_QUEUE_CAPACITY);
    let (audio_tx, audio_rx) = mpsc::channel(AUDIO_QUEUE_CAPACITY);

    let video_cb_ctx = Box::into_raw(Box::new(VideoCallbackCtx {
        tx: video_tx,
        dropped: AtomicU64::new(0),
    }));
    let audio_cb_ctx = Box::into_raw(Box::new(AudioCallbackCtx {
        tx: audio_tx,
        dropped: AtomicU64::new(0),
    }));

    let host = unsafe {
        mello_sys::mello_stream_start_host(
            ctx,
            source,
            config,
            Some(on_video_packet),
            video_cb_ctx as *mut std::ffi::c_void,
        )
    };

    if host.is_null() {
        unsafe {
            drop(Box::from_raw(video_cb_ctx));
            drop(Box::from_raw(audio_cb_ctx));
        }
        return Err(StreamError::EncodeFailed(
            "Failed to start stream host (libmello)".to_string(),
        ));
    }

    unsafe {
        mello_sys::mello_stream_set_audio_callback(
            host,
            Some(on_audio_packet),
            audio_cb_ctx as *mut std::ffi::c_void,
        );
    }

    let resources = HostResources {
        video_ctx: video_cb_ctx,
        audio_ctx: audio_cb_ctx,
    };

    Ok((host, video_rx, audio_rx, resources))
}

/// Create the manager and spawn the run loop. The caller provides the sink.
#[allow(clippy::too_many_arguments)]
pub fn create_stream_session(
    ctx: *mut mello_sys::MelloContext,
    host: *mut mello_sys::MelloStreamHost,
    resp: &StartStreamResponse,
    config: StreamConfig,
    video_rx: mpsc::Receiver<VideoPacket>,
    audio_rx: mpsc::Receiver<AudioPacket>,
    _resources: HostResources,
    sink: Arc<dyn PacketSink>,
) -> Result<StreamSession, StreamError> {
    let session_id = resp.session_id();
    let mode = resp.mode.clone();

    let manager = StreamManager::new(ctx, host, sink, config, video_rx, audio_rx);

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let session = StreamSession::new(session_id, mode, stop_tx);

    tokio::spawn(async move {
        // Keep callback contexts alive for the entire lifetime of the host.
        // IMPORTANT: Drop StreamManager (which stops the C++ host) before
        // dropping callback contexts, otherwise capture threads can still invoke
        // callbacks with dangling user_data pointers during shutdown.
        let _res = _resources;
        {
            let mut mgr = manager;
            mgr.run(stop_rx).await;
        }
    });

    Ok(session)
}
