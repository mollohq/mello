use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::nakama::NakamaClient;

use super::config::StreamConfig;
use super::error::StreamError;
use super::manager::{AudioPacket, StreamManager, StreamSession, VideoPacket};
use super::sink::PacketSink;
use super::teardown::{NativeTeardownGuard, TeardownPtr};

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
    pub bitrate_kbps: u32,
    /// Game executable name for logging only. The server never trusts it to
    /// allow a hook; the client matches exe against the capture block lists.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub exe: String,
}

#[derive(Debug, Clone, Deserialize)]
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
    /// Backend `capture` block (streaming-reliability plan §8). `None` on
    /// older backend responses, which the client treats as "never hook".
    #[serde(default)]
    pub capture: Option<CapturePolicy>,
}

/// Backend hook policy for one stream start. Lists carry executable names
/// (e.g. "heaven.exe") and are matched case-insensitively. Deny wins over
/// allow; unknown executables are never hooked.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CapturePolicy {
    #[serde(default)]
    pub hook_enabled: bool,
    #[serde(default)]
    pub policy_version: String,
    #[serde(default)]
    pub hook_allow: Vec<String>,
    #[serde(default)]
    pub hook_deny: Vec<String>,
}

fn default_mode() -> String {
    "p2p".to_string()
}

/// Decide whether the game capture hook may run for `exe` under `policy`.
///
/// All three must hold: the backend kill switch is on, the exe is on the
/// allow list, and it is not on the deny list. Matching is ASCII
/// case-insensitive because Windows executable names vary in case. `None`
/// (no `capture` block) or an empty exe never allows the hook.
pub fn hook_allowed_for_exe(exe: &str, policy: Option<&CapturePolicy>) -> bool {
    let policy = match policy {
        Some(p) => p,
        None => return false,
    };
    if !policy.hook_enabled {
        return false;
    }
    if exe.is_empty() {
        return false;
    }
    let denied = policy.hook_deny.iter().any(|d| d.eq_ignore_ascii_case(exe));
    if denied {
        return false;
    }
    policy
        .hook_allow
        .iter()
        .any(|a| a.eq_ignore_ascii_case(exe))
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
#[allow(clippy::too_many_arguments)]
pub async fn request_start_stream(
    nakama: &NakamaClient,
    crew_id: &str,
    title: &str,
    supports_av1: bool,
    width: u32,
    height: u32,
    bitrate_kbps: u32,
    exe: &str,
) -> Result<StartStreamResponse, StreamError> {
    let req = StartStreamRequest {
        crew_id: crew_id.to_string(),
        title: title.to_string(),
        supports_av1,
        width,
        height,
        bitrate_kbps,
        exe: exe.to_string(),
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
struct HostResources {
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
    NativeTeardownGuard,
);

/// Build the guard that stops a native stream host and then frees its callback
/// contexts, on a dedicated teardown thread.
///
/// Order is load-bearing: the contexts are freed only after
/// `mello_stream_stop_host` returns, because capture and encode threads call
/// back into them until then. If the stop never returns, the contexts are never
/// freed.
fn host_teardown_guard(
    host: *mut mello_sys::MelloStreamHost,
    resources: HostResources,
) -> NativeTeardownGuard {
    let host = TeardownPtr(host);
    NativeTeardownGuard::new("stream_host", move |steps| {
        steps.step("mello_stream_stop_audio");
        unsafe { mello_sys::mello_stream_stop_audio(host.get()) };
        steps.step("mello_stream_stop_host");
        unsafe { mello_sys::mello_stream_stop_host(host.get()) };
        steps.step("free callback contexts");
        drop(resources);
    })
}

/// Start the C++ host pipeline with callback-based packet delivery.
/// Returns the host handle, channel receivers, and the teardown guard. Dropping
/// the guard stops the host on a teardown thread; nothing else may stop it.
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

    Ok((
        host,
        video_rx,
        audio_rx,
        host_teardown_guard(host, resources),
    ))
}

/// Create the manager and spawn the run loop. The caller provides the sink.
///
/// `pause_event_tx` carries pause transitions to the host UI (`None` in
/// tools/tests without a UI loop). The control-channel broadcast to viewers
/// runs regardless.
#[allow(clippy::too_many_arguments)]
pub fn create_stream_session(
    ctx: *mut mello_sys::MelloContext,
    host: *mut mello_sys::MelloStreamHost,
    resp: &StartStreamResponse,
    config: StreamConfig,
    video_rx: mpsc::Receiver<VideoPacket>,
    audio_rx: mpsc::Receiver<AudioPacket>,
    teardown: NativeTeardownGuard,
    sink: Arc<dyn PacketSink>,
    pause_event_tx: Option<std::sync::mpsc::Sender<crate::events::Event>>,
) -> Result<StreamSession, StreamError> {
    let session_id = resp.session_id();
    let mode = resp.mode.clone();

    let mut manager = StreamManager::new(ctx, host, sink, config, video_rx, audio_rx);
    let capture_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    manager.set_capture_failed_flag(std::sync::Arc::clone(&capture_failed));
    manager.set_pause_event_tx(pause_event_tx);

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let manager_task = tokio::spawn(async move {
        // The guard owns the native host for the task's whole life. Declared
        // first, it drops last: after the manager stops using the host pointer.
        // That holds on a normal exit and when the task is aborted at an await,
        // so every path stops the host through the bounded teardown thread.
        let _teardown = teardown;
        let mut mgr = manager;
        mgr.run(stop_rx).await;
        drop(mgr);
    });
    let session = StreamSession::with_capture_failed_flag(
        session_id,
        mode,
        stop_tx,
        manager_task,
        capture_failed,
    );

    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::{hook_allowed_for_exe, CapturePolicy, StartStreamRequest, StartStreamResponse};

    fn policy() -> CapturePolicy {
        CapturePolicy {
            hook_enabled: true,
            policy_version: "2026-09-19:2001".to_string(),
            hook_allow: vec!["heaven.exe".to_string()],
            hook_deny: vec!["cs2.exe".to_string()],
        }
    }

    #[test]
    fn hook_allowed_for_listed_exe_case_insensitive() {
        assert!(hook_allowed_for_exe("Heaven.exe", Some(&policy())));
    }

    #[test]
    fn hook_denied_wins_over_allow() {
        let mut p = policy();
        p.hook_allow.push("cs2.exe".to_string());
        assert!(!hook_allowed_for_exe("cs2.exe", Some(&p)));
    }

    #[test]
    fn hook_denied_for_unknown_exe() {
        assert!(!hook_allowed_for_exe("unknown-game.exe", Some(&policy())));
    }

    #[test]
    fn hook_denied_when_kill_switch_off() {
        let mut p = policy();
        p.hook_enabled = false;
        assert!(!hook_allowed_for_exe("heaven.exe", Some(&p)));
    }

    #[test]
    fn hook_denied_without_capture_block() {
        assert!(!hook_allowed_for_exe("heaven.exe", None));
    }

    #[test]
    fn hook_denied_for_empty_exe() {
        assert!(!hook_allowed_for_exe("", Some(&policy())));
    }

    #[test]
    fn old_backend_response_without_capture_parses() {
        let resp: StartStreamResponse = serde_json::from_str(r#"{"session_id":"s1","mode":"p2p"}"#)
            .expect("old response parses");
        assert!(resp.capture.is_none());
        assert!(!hook_allowed_for_exe("heaven.exe", resp.capture.as_ref()));
    }

    #[test]
    fn new_backend_response_with_capture_parses() {
        let resp: StartStreamResponse = serde_json::from_str(
            r#"{"session_id":"s1","mode":"p2p","capture":{"hook_enabled":true,"policy_version":"v1","hook_allow":["heaven.exe"],"hook_deny":["cs2.exe"]}}"#,
        )
        .expect("new response parses");
        let capture = resp.capture.as_ref().expect("capture block present");
        assert_eq!(capture.policy_version, "v1");
        assert!(hook_allowed_for_exe("heaven.exe", Some(capture)));
        assert!(!hook_allowed_for_exe("cs2.exe", Some(capture)));
    }

    #[test]
    fn start_stream_request_serializes_configured_bitrate() {
        let request = StartStreamRequest {
            crew_id: "crew".to_string(),
            title: "title".to_string(),
            supports_av1: false,
            width: 1920,
            height: 1080,
            bitrate_kbps: 4_500,
            exe: String::new(),
        };
        let json = serde_json::to_value(request).expect("serialize request");
        assert_eq!(json["bitrate_kbps"], 4_500);
    }

    #[test]
    fn start_stream_request_omits_empty_exe() {
        let request = StartStreamRequest {
            crew_id: "crew".to_string(),
            title: String::new(),
            supports_av1: false,
            width: 1280,
            height: 720,
            bitrate_kbps: 2_500,
            exe: String::new(),
        };
        let json = serde_json::to_value(request).expect("serialize request");
        assert!(json.get("exe").is_none(), "empty exe stays off the wire");
    }
}
