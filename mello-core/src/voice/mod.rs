mod mesh;

use std::collections::VecDeque;
use std::ffi::CString;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::Instant;

use crate::events::Event;
use crate::transport::{SfuConnection, SfuEvent};
use serde::{Deserialize, Serialize};

pub use mesh::{SignalEnvelope, SignalMessage, SignalPurpose, VoiceMesh};

const PACKET_BUF_SIZE: usize = 4000;
const MAX_DEVICES: usize = 32;
const DEBUG_STATS_TICK_DIVISOR: u32 = 10; // 100Hz tick -> 10Hz debug updates
const UNDERRUN_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);
const SFU_PONG_STALE_MS: i64 = 8_000;
const SFU_SIGNALING_GRACE_MS: u64 = 5_000;
/// Grace window after an SFU connection is established during which "no pong
/// observed yet" (pong_age_ms < 0) is tolerated. After this, a persistent lack
/// of pongs means the control round-trip is dead and the session is unhealthy.
const SFU_PONG_GRACE_MS: i64 = 15_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceMode {
    Disconnected,
    P2P,
    SFU,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NsMode {
    Off,
    Rnnoise,
    WebRtcLow,
    WebRtcModerate,
    WebRtcHigh,
    WebRtcVeryHigh,
}

impl NsMode {
    fn as_ffi(self) -> mello_sys::MelloNsMode {
        match self {
            NsMode::Off => mello_sys::MelloNsMode_MELLO_NS_OFF,
            NsMode::Rnnoise => mello_sys::MelloNsMode_MELLO_NS_RNNOISE,
            NsMode::WebRtcLow => mello_sys::MelloNsMode_MELLO_NS_WEBRTC_LOW,
            NsMode::WebRtcModerate => mello_sys::MelloNsMode_MELLO_NS_WEBRTC_MODERATE,
            NsMode::WebRtcHigh => mello_sys::MelloNsMode_MELLO_NS_WEBRTC_HIGH,
            NsMode::WebRtcVeryHigh => mello_sys::MelloNsMode_MELLO_NS_WEBRTC_VERY_HIGH,
        }
    }
}

struct VadCallbackData {
    tx: std_mpsc::Sender<Event>,
    local_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

pub struct VoiceManager {
    ctx: *mut mello_sys::MelloContext,
    event_tx: std_mpsc::Sender<Event>,
    mesh: VoiceMesh,
    muted: bool,
    deafened: bool,
    push_to_talk: bool,
    active: bool,
    loopback: bool,
    debug_mode: bool,
    /// Diagnostic capture: when true, per-frame audio stats are also written to
    /// the log file (for a user-driven repro upload), independent of whether the
    /// debug panel is open.
    capture_mode: bool,
    /// Rolling samples of (time, underrun delta) used to compute a windowed
    /// underrun rate. Deltas are only counted while incoming audio is expected
    /// (incoming_streams > 0), so a quiet/solo session reads 0 instead of the
    /// ever-growing lifetime counter's idle-starvation noise.
    underrun_window: VecDeque<(Instant, i32)>,
    prev_underrun_total: i32,
    tick_counter: u32,
    mode: VoiceMode,
    sfu_connection: Option<Arc<SfuConnection>>,
    sfu_crew_id: String,
    /// Consecutive SFU health checks (every ~2s) that found the peer connection
    /// down. Used to detect half-open SFU sessions (e.g. after sleep/wake) that
    /// never surface a `Disconnected` signaling event.
    sfu_unhealthy_checks: u32,
    /// When the current SFU connection was established. Used to apply a grace
    /// window before treating "no pong observed yet" as unhealthy.
    sfu_connected_at: Option<Instant>,
    /// Previous `rtp_recv_total` and consecutive liveness ticks (~2s each) where
    /// inbound RTP was flat while a remote stream was present. Detection-only
    /// for now (logged, not acted on) so the field signal can be validated
    /// before it drives a reconnect.
    prev_rtp_recv_total: i32,
    rtp_stall_checks: u32,
}

// Safety: VoiceManager is only ever accessed from a single tokio task (the client run loop).
// The raw pointers to libmello objects are thread-safe because libmello uses internal locking.
unsafe impl Send for VoiceManager {}
unsafe impl Sync for VoiceManager {}

unsafe extern "C" fn libmello_log_bridge(
    _user_data: *mut std::ffi::c_void,
    level: i32,
    tag: *const std::ffi::c_char,
    message: *const std::ffi::c_char,
) {
    let tag = if tag.is_null() {
        "?"
    } else {
        std::ffi::CStr::from_ptr(tag).to_str().unwrap_or("?")
    };
    let msg = if message.is_null() {
        ""
    } else {
        std::ffi::CStr::from_ptr(message).to_str().unwrap_or("")
    };
    let target = &format!("libmello::{}", tag);
    match level {
        0 => log::debug!(target: target, "{}", msg),
        1 => log::info!(target: target, "{}", msg),
        2 => log::warn!(target: target, "{}", msg),
        _ => log::error!(target: target, "{}", msg),
    }
}

impl VoiceManager {
    pub fn new(event_tx: std_mpsc::Sender<Event>, loopback: bool) -> Self {
        unsafe {
            mello_sys::mello_set_log_callback(Some(libmello_log_bridge), std::ptr::null_mut());
        }

        let ctx = unsafe { mello_sys::mello_init() };
        if ctx.is_null() {
            log::error!("Failed to initialize libmello context");
        } else {
            log::info!("libmello context initialized");
        }

        if loopback {
            log::info!("Loopback mode enabled -- captured audio will play back locally");
        }

        Self {
            ctx,
            event_tx,
            mesh: VoiceMesh::new(),
            muted: false,
            deafened: false,
            push_to_talk: false,
            active: false,
            loopback,
            debug_mode: false,
            capture_mode: false,
            underrun_window: VecDeque::new(),
            prev_underrun_total: 0,
            tick_counter: 0,
            mode: VoiceMode::Disconnected,
            sfu_connection: None,
            sfu_crew_id: String::new(),
            sfu_unhealthy_checks: 0,
            sfu_connected_at: None,
            prev_rtp_recv_total: 0,
            rtp_stall_checks: 0,
        }
    }

    pub fn mello_ctx(&self) -> *mut mello_sys::MelloContext {
        self.ctx
    }

    pub fn join_voice(&mut self, local_id: &str, members: &[String]) {
        if self.ctx.is_null() {
            return;
        }

        self.mesh.init(local_id, members);
        self.setup_vad_callback(local_id);

        let result = unsafe { mello_sys::mello_voice_start_capture(self.ctx) };
        if result != mello_sys::MelloResult_MELLO_OK {
            log::error!("Failed to start voice capture: {}", result);
            return;
        }

        self.active = true;
        self.mode = VoiceMode::P2P;
        self.apply_push_to_talk();
        log::info!(
            "Voice capture started (P2P), {} peers to connect",
            members.len()
        );

        for member_id in members {
            self.mesh.create_peer(self.ctx, local_id, member_id);
        }
    }

    /// Join voice via SFU. Called from the client when mode is "sfu".
    /// The SfuConnection is already established and joined.
    pub fn join_voice_sfu(
        &mut self,
        local_id: &str,
        crew_id: &str,
        connection: Arc<SfuConnection>,
    ) {
        if self.ctx.is_null() {
            return;
        }

        self.setup_vad_callback(local_id);

        let result = unsafe { mello_sys::mello_voice_start_capture(self.ctx) };
        if result != mello_sys::MelloResult_MELLO_OK {
            log::error!("Failed to start voice capture for SFU: {}", result);
            return;
        }

        unsafe { connection.start_stats_reporter(self.ctx) };
        self.sfu_connection = Some(connection);
        self.sfu_crew_id = crew_id.to_string();
        self.active = true;
        self.mode = VoiceMode::SFU;
        self.sfu_connected_at = Some(Instant::now());
        self.prev_rtp_recv_total = 0;
        self.rtp_stall_checks = 0;
        self.apply_push_to_talk();
        log::info!("Voice capture started (SFU)");
    }

    pub fn leave_voice(&mut self) {
        if !self.active {
            return;
        }

        self.stop_capture();

        match self.mode {
            VoiceMode::P2P => {
                self.mesh.destroy_all_peers();
            }
            VoiceMode::SFU => {
                self.sfu_connection = None;
                self.sfu_crew_id.clear();
                self.sfu_connected_at = None;
                self.rtp_stall_checks = 0;
            }
            VoiceMode::Disconnected => {}
        }

        self.active = false;
        self.mode = VoiceMode::Disconnected;
        log::info!("Voice stopped");
    }

    pub fn voice_mode(&self) -> VoiceMode {
        self.mode
    }

    pub fn sfu_connection(&self) -> Option<&Arc<SfuConnection>> {
        self.sfu_connection.as_ref()
    }

    /// Force the voice session into the Disconnected state and emit
    /// `VoiceSfuDisconnected` so the client's reconnect scheduler takes over.
    /// Used when an external signal (e.g. sleep/wake) makes the current SFU
    /// session untrustworthy. No-op outside SFU mode.
    pub fn mark_disconnected(&mut self) {
        self.mark_disconnected_with_reason("sleep_wake");
    }

    /// Same as `mark_disconnected`, but allows the caller to attach the source
    /// reason from the triggering signal (fault, liveness, SFU error, etc.).
    pub fn mark_disconnected_with_reason(&mut self, reason: impl Into<String>) {
        if self.mode != VoiceMode::SFU {
            return;
        }
        let reason = reason.into();
        let crew_id = self.sfu_crew_id.clone();
        // Disconnect teardown order matters: stop capture first so libmello
        // state cannot continue capturing against a dead transport.
        if self.active {
            self.stop_capture();
        }
        if let Some(conn) = self.sfu_connection.take() {
            Self::spawn_best_effort_sfu_leave(conn);
        }
        self.active = false;
        self.mode = VoiceMode::Disconnected;
        self.sfu_unhealthy_checks = 0;
        self.sfu_connected_at = None;
        self.rtp_stall_checks = 0;
        self.sfu_crew_id.clear();
        log::info!("Voice stopped (SFU disconnect): reason={}", reason);
        let _ = self
            .event_tx
            .send(Event::VoiceSfuDisconnected { crew_id, reason });
    }

    fn stop_capture(&self) {
        if self.ctx.is_null() {
            return;
        }
        unsafe {
            mello_sys::mello_voice_stop_capture(self.ctx);
        }
    }

    fn spawn_best_effort_sfu_leave(conn: Arc<SfuConnection>) {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    conn.leave().await;
                });
            }
            Err(e) => {
                log::debug!("SFU leave skipped (no tokio runtime): {}", e);
            }
        }
    }

    /// Detection-only inbound RTP-stall signal. Logs (without acting) when no
    /// inbound RTP arrives for ~10s while a remote stream is present -- well
    /// beyond a normal DTX silence gap and matching the field RTP blackout.
    /// Once validated against real captures this can drive a reconnect.
    fn detect_rtp_stall(&mut self) {
        if self.ctx.is_null() {
            return;
        }
        let mut stats: mello_sys::MelloDebugStats = unsafe { std::mem::zeroed() };
        unsafe {
            mello_sys::mello_get_debug_stats(self.ctx, &mut stats);
        }
        if stats.incoming_streams > 0 && stats.rtp_recv_total == self.prev_rtp_recv_total {
            self.rtp_stall_checks += 1;
            // ~2s per check: warn at ~10s, then every ~10s while still stalled.
            if self.rtp_stall_checks == 5
                || (self.rtp_stall_checks > 5 && self.rtp_stall_checks.is_multiple_of(5))
            {
                log::warn!(
                    "SFU RTP stall (detect-only): no inbound RTP for ~{}s while {} stream(s) present (rtp_recv_total={}, pkts_encoded={})",
                    self.rtp_stall_checks * 2,
                    stats.incoming_streams,
                    stats.rtp_recv_total,
                    stats.packets_encoded
                );
            }
        } else {
            if self.rtp_stall_checks >= 5 {
                log::info!(
                    "SFU RTP stall cleared after ~{}s (rtp_recv_total={})",
                    self.rtp_stall_checks * 2,
                    stats.rtp_recv_total
                );
            }
            self.rtp_stall_checks = 0;
        }
        self.prev_rtp_recv_total = stats.rtp_recv_total;
    }

    fn setup_vad_callback(&self, local_id: &str) {
        let event_tx = self.event_tx.clone();
        let local_id_owned = local_id.to_string();
        unsafe {
            extern "C" fn vad_cb(user_data: *mut std::ffi::c_void, speaking: bool) {
                let data = unsafe { &*(user_data as *const VadCallbackData) };
                let _ = data.tx.send(Event::VoiceActivity {
                    member_id: data.local_id.clone(),
                    speaking,
                });
            }

            let cb_data = Box::new(VadCallbackData {
                tx: event_tx,
                local_id: local_id_owned,
            });
            let cb_ptr = Box::into_raw(cb_data) as *mut std::ffi::c_void;
            mello_sys::mello_voice_set_vad_callback(self.ctx, Some(vad_cb), cb_ptr);
        }
    }

    pub fn set_mute(&mut self, muted: bool) {
        self.muted = muted;
        if !self.ctx.is_null() {
            unsafe {
                mello_sys::mello_voice_set_mute(self.ctx, muted);
            }
        }
    }

    pub fn set_push_to_talk(&mut self, enabled: bool) {
        self.push_to_talk = enabled;
        self.apply_push_to_talk();
    }

    fn apply_push_to_talk(&self) {
        if self.ctx.is_null() {
            return;
        }
        unsafe {
            mello_sys::mello_voice_set_push_to_talk(self.ctx, self.push_to_talk);
        }
    }

    pub fn set_deafen(&mut self, deafened: bool) {
        self.deafened = deafened;
        if !self.ctx.is_null() {
            unsafe {
                mello_sys::mello_voice_set_deafen(self.ctx, deafened);
            }
        }
    }

    pub fn set_echo_cancellation(&mut self, enabled: bool) {
        if !self.ctx.is_null() {
            unsafe {
                mello_sys::mello_voice_set_echo_cancellation(self.ctx, enabled);
            }
        }
    }

    pub fn set_agc(&mut self, enabled: bool) {
        if !self.ctx.is_null() {
            unsafe {
                mello_sys::mello_voice_set_agc(self.ctx, enabled);
            }
        }
    }

    pub fn set_noise_suppression(&mut self, enabled: bool) {
        if !self.ctx.is_null() {
            unsafe {
                mello_sys::mello_voice_set_noise_suppression(self.ctx, enabled);
            }
        }
    }

    pub fn set_ns_mode(&mut self, mode: NsMode) {
        if !self.ctx.is_null() {
            unsafe {
                mello_sys::mello_voice_set_ns_mode(self.ctx, mode.as_ffi());
            }
        }
    }

    pub fn set_transient_suppression(&mut self, enabled: bool) {
        if !self.ctx.is_null() {
            unsafe {
                mello_sys::mello_voice_set_transient_suppression(self.ctx, enabled);
            }
        }
    }

    pub fn set_high_pass_filter(&mut self, enabled: bool) {
        if !self.ctx.is_null() {
            unsafe {
                mello_sys::mello_voice_set_high_pass_filter(self.ctx, enabled);
            }
        }
    }

    pub fn set_input_volume(&mut self, volume: f32) {
        if !self.ctx.is_null() {
            unsafe {
                mello_sys::mello_voice_set_input_volume(self.ctx, volume);
            }
        }
    }

    pub fn set_output_volume(&mut self, volume: f32) {
        if !self.ctx.is_null() {
            unsafe {
                mello_sys::mello_voice_set_output_volume(self.ctx, volume);
            }
        }
    }

    pub fn set_loopback(&mut self, enabled: bool) {
        if self.ctx.is_null() {
            return;
        }
        self.loopback = enabled;

        if enabled && !self.active {
            let result = unsafe { mello_sys::mello_voice_start_capture(self.ctx) };
            if result != mello_sys::MelloResult_MELLO_OK {
                log::error!("Failed to start capture for mic test: {}", result);
            } else {
                log::info!("Loopback enabled (started capture for mic test)");
            }
        } else if !enabled && !self.active {
            unsafe {
                mello_sys::mello_voice_stop_capture(self.ctx);
            }
            log::info!("Loopback disabled (stopped mic test capture)");
        } else {
            log::info!("Loopback {}", if enabled { "enabled" } else { "disabled" });
        }
    }

    pub fn start_capture_inject(&mut self) -> bool {
        if self.ctx.is_null() {
            return false;
        }
        let result = unsafe { mello_sys::mello_voice_start_capture_inject(self.ctx) };
        if result != mello_sys::MelloResult_MELLO_OK {
            log::error!("Failed to start capture inject mode: {}", result);
            return false;
        }
        true
    }

    pub fn inject_capture_frame(&mut self, samples: &[i16]) {
        if self.ctx.is_null() || samples.is_empty() {
            return;
        }
        unsafe {
            mello_sys::mello_voice_inject_capture(self.ctx, samples.as_ptr(), samples.len() as i32);
        }
    }

    pub fn stop_capture_inject(&mut self) {
        if self.ctx.is_null() {
            return;
        }
        unsafe {
            mello_sys::mello_voice_stop_capture_inject(self.ctx);
        }
    }

    pub fn list_capture_devices(&self) -> Vec<AudioDevice> {
        if self.ctx.is_null() {
            return vec![];
        }
        self.list_devices(true)
    }

    pub fn list_playback_devices(&self) -> Vec<AudioDevice> {
        if self.ctx.is_null() {
            return vec![];
        }
        self.list_devices(false)
    }

    fn list_devices(&self, capture: bool) -> Vec<AudioDevice> {
        let mut raw = vec![
            mello_sys::MelloDevice {
                id: std::ptr::null(),
                name: std::ptr::null(),
                is_default: false,
            };
            MAX_DEVICES
        ];

        let count = unsafe {
            if capture {
                mello_sys::mello_get_audio_inputs(self.ctx, raw.as_mut_ptr(), MAX_DEVICES as i32)
            } else {
                mello_sys::mello_get_audio_outputs(self.ctx, raw.as_mut_ptr(), MAX_DEVICES as i32)
            }
        };

        let mut devices = Vec::with_capacity(count as usize);
        for dev in raw.iter().take(count as usize) {
            let id = if dev.id.is_null() {
                String::new()
            } else {
                unsafe {
                    std::ffi::CStr::from_ptr(dev.id)
                        .to_string_lossy()
                        .into_owned()
                }
            };
            let name = if dev.name.is_null() {
                String::new()
            } else {
                unsafe {
                    std::ffi::CStr::from_ptr(dev.name)
                        .to_string_lossy()
                        .into_owned()
                }
            };
            devices.push(AudioDevice {
                id,
                name,
                is_default: dev.is_default,
            });
        }

        unsafe {
            mello_sys::mello_free_device_list(raw.as_mut_ptr(), count);
        }
        devices
    }

    /// Returns true if the requested device was not found and the pipeline
    /// fell back to the system default.
    pub fn set_capture_device(&mut self, device_id: &str) -> bool {
        if self.ctx.is_null() {
            return false;
        }
        let c_id = CString::new(device_id).unwrap_or_default();
        let result = unsafe { mello_sys::mello_set_audio_input(self.ctx, c_id.as_ptr()) };
        if result == mello_sys::MelloResult_MELLO_DEVICE_FALLBACK {
            log::warn!(
                "Capture device '{}' not found, fell back to default",
                device_id
            );
            return true;
        } else if result != mello_sys::MelloResult_MELLO_OK {
            log::error!("Failed to set capture device: {}", result);
        } else {
            log::info!("Capture device set to: {}", device_id);
        }
        false
    }

    /// Returns true if the requested device was not found and the pipeline
    /// fell back to the system default.
    pub fn set_playback_device(&mut self, device_id: &str) -> bool {
        if self.ctx.is_null() {
            return false;
        }
        let c_id = CString::new(device_id).unwrap_or_default();
        let result = unsafe { mello_sys::mello_set_audio_output(self.ctx, c_id.as_ptr()) };
        if result == mello_sys::MelloResult_MELLO_DEVICE_FALLBACK {
            log::warn!(
                "Playback device '{}' not found, fell back to default",
                device_id
            );
            return true;
        } else if result != mello_sys::MelloResult_MELLO_OK {
            log::error!("Failed to set playback device: {}", result);
        } else {
            log::info!("Playback device set to: {}", device_id);
        }
        false
    }

    pub fn get_input_level(&self) -> f32 {
        if self.ctx.is_null() {
            return 0.0;
        }
        unsafe { mello_sys::mello_voice_get_input_level(self.ctx) }
    }

    pub fn set_debug_mode(&mut self, enabled: bool) {
        self.debug_mode = enabled;
        log::info!(
            "Audio debug mode {}",
            if enabled { "enabled" } else { "disabled" }
        );
    }

    /// Toggle diagnostic capture: raise libmello verbosity to Debug and start
    /// writing per-frame audio stats to the log (see `tick`). Restores Info on
    /// stop. The Rust-side log filter + file slicing/upload are driven by the
    /// client; this only controls libmello + the audio-stats log lines.
    pub fn set_diagnostic_capture(&mut self, enabled: bool) {
        self.capture_mode = enabled;
        // 0 = Debug, 1 = Info (see mello.h log levels).
        unsafe { mello_sys::mello_set_log_level(if enabled { 0 } else { 1 }) };
        log::info!(
            target: "audio_stats",
            "diagnostic capture {} (crew={})",
            if enabled { "started" } else { "stopped" },
            self.sfu_crew_id
        );
    }

    /// Underruns observed in the last [`UNDERRUN_WINDOW`], counting only ticks
    /// where audio was incoming (`streams > 0`). Mirrors libmello's peer-gated
    /// warning logic so a quiet/solo session reads ~0 instead of the lifetime
    /// counter's constant idle-starvation growth.
    fn windowed_underrun(&mut self, total: i32, streams: i32) -> i32 {
        let now = Instant::now();
        // Don't count the (possibly large) backlog accumulated before the first
        // sample after stats were enabled — seed the baseline instead.
        let first_sample = self.underrun_window.is_empty();
        let inc = (total - self.prev_underrun_total).max(0);
        self.prev_underrun_total = total;

        let counted = if first_sample || streams <= 0 { 0 } else { inc };
        self.underrun_window.push_back((now, counted));

        while let Some(&(t, _)) = self.underrun_window.front() {
            if now.duration_since(t) > UNDERRUN_WINDOW {
                self.underrun_window.pop_front();
            } else {
                break;
            }
        }
        self.underrun_window.iter().map(|&(_, c)| c).sum()
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn set_ice_servers(&mut self, urls: Vec<String>) {
        self.mesh.set_ice_servers(urls);
    }

    /// Get pending signal messages that need to be sent via Nakama
    pub fn drain_signals(&mut self) -> Vec<(String, SignalMessage)> {
        self.mesh.drain_signals()
    }

    /// Handle an incoming signal from Nakama
    pub fn handle_signal(&mut self, from: &str, signal: SignalMessage) {
        if self.mode == VoiceMode::SFU {
            log::debug!("Ignoring P2P voice signal in SFU mode from {}", from);
            return;
        }
        self.mesh.handle_signal(self.ctx, from, signal);
    }

    /// Called when a new member joins the crew while voice is active
    pub fn on_member_joined(&mut self, local_id: &str, member_id: &str) {
        if !self.active {
            return;
        }
        if self.mode == VoiceMode::SFU {
            return;
        }
        self.mesh.create_peer(self.ctx, local_id, member_id);
    }

    /// Called when a member leaves the crew
    pub fn on_member_left(&mut self, member_id: &str) {
        self.mesh.destroy_peer(member_id);
        let _ = self.event_tx.send(Event::VoiceDisconnected {
            peer_id: member_id.to_string(),
        });
    }

    /// Poll audio: read encoded packets from capture and send to all peers,
    /// and feed received packets from peers to the playback pipeline.
    pub fn tick(&mut self) {
        if self.ctx.is_null() {
            return;
        }
        if !self.active && !self.loopback {
            return;
        }

        self.tick_counter = self.tick_counter.wrapping_add(1);

        if self.loopback && self.tick_counter.is_multiple_of(5) {
            let level = self.get_input_level();
            let _ = self.event_tx.send(Event::MicLevel { level });
        }

        // Send a DC ping every ~2 seconds (200 ticks at 100Hz) and run an SFU
        // liveness check. Health is multi-signal (PC connected, control channel
        // open, fresh pong RTT) with a short signaling grace so transient
        // renegotiation churn doesn't look like a dead session.
        if self.tick_counter.is_multiple_of(200) {
            if let Some(conn) = &self.sfu_connection {
                conn.send_ping();
                let connected = conn.is_connected();
                let ctrl_open = conn.is_control_channel_open();
                let rtt_ms = conn.rtt_ms();
                let pong_age_ms = conn.pong_age_ms();
                let signaling_idle_ms = conn.signaling_idle_ms();
                let in_signaling_grace = signaling_idle_ms <= SFU_SIGNALING_GRACE_MS;
                let since_connect_ms = self
                    .sfu_connected_at
                    .map(|t| t.elapsed().as_millis() as i64)
                    .unwrap_or(i64::MAX);
                // Pong freshness: if we've seen a pong, it must be recent. If we've
                // never seen one (pong_age_ms < 0), tolerate it only briefly after
                // connect — past the grace window, a persistent lack of pongs means
                // the control round-trip is dead.
                let pong_fresh = if pong_age_ms < 0 {
                    since_connect_ms < SFU_PONG_GRACE_MS
                } else {
                    pong_age_ms <= SFU_PONG_STALE_MS
                };
                let healthy = connected && ctrl_open && pong_fresh;
                log::debug!(
                    "SFU liveness: connected={} ctrl_open={} rtt_ms={:.1} pong_age_ms={} signaling_idle_ms={}",
                    connected,
                    ctrl_open,
                    rtt_ms,
                    pong_age_ms,
                    signaling_idle_ms
                );
                if self.mode == VoiceMode::SFU && !healthy {
                    if in_signaling_grace {
                        self.sfu_unhealthy_checks = 0;
                        log::debug!(
                            "SFU liveness: suppressing unhealthy check during signaling grace (idle={}ms)",
                            signaling_idle_ms
                        );
                    } else {
                        self.sfu_unhealthy_checks += 1;
                        log::warn!(
                            "SFU peer unhealthy ({}/3 checks): connected={} ctrl_open={} pong_age_ms={} rtt_ms={:.1}",
                            self.sfu_unhealthy_checks,
                            connected,
                            ctrl_open,
                            pong_age_ms,
                            rtt_ms
                        );
                    }
                } else {
                    self.sfu_unhealthy_checks = 0;
                }
            }
            // 3 consecutive bad checks (~6s) -> declare the session dead.
            if self.sfu_unhealthy_checks >= 3 {
                log::warn!("SFU peer connection dead after repeated checks; reconnecting");
                self.mark_disconnected_with_reason("liveness_timeout");
                return;
            }

            // Detection-only: inbound RTP stall while a remote stream is present.
            if self.mode == VoiceMode::SFU && self.sfu_connection.is_some() {
                self.detect_rtp_stall();
            }
        }

        if (self.debug_mode || self.capture_mode)
            && self.tick_counter.is_multiple_of(DEBUG_STATS_TICK_DIVISOR)
        {
            let mut stats: mello_sys::MelloDebugStats = unsafe { std::mem::zeroed() };
            unsafe {
                mello_sys::mello_get_debug_stats(self.ctx, &mut stats);
            }
            let rtt = self.sfu_connection.as_ref().map_or(0.0, |c| c.rtt_ms());
            let underrun_5s = self.windowed_underrun(stats.underrun_count, stats.incoming_streams);

            // Diagnostic capture: persist the same stats to the log so a
            // user-uploaded repro shows the sender/receiver-side timeline.
            // `ur5s` is the windowed underrun count (only while audio is
            // incoming) — the real health signal; `underrun` is the noisy
            // lifetime counter kept for reference.
            if self.capture_mode {
                log::info!(
                    target: "audio_stats",
                    "in={:.3} vad={:.3} rnn={:.3} spk={} cap={} mute={} deaf={} pkts={} streams={} underrun={} ur5s={} rtp_recv={} delay_ms={:.1} rtt_ms={:.1}",
                    stats.input_level,
                    stats.silero_vad_prob,
                    stats.rnnoise_prob,
                    stats.is_speaking,
                    stats.is_capturing,
                    stats.is_muted,
                    stats.is_deafened,
                    stats.packets_encoded,
                    stats.incoming_streams,
                    stats.underrun_count,
                    underrun_5s,
                    stats.rtp_recv_total,
                    stats.pipeline_delay_ms,
                    rtt,
                );
            }

            if self.debug_mode {
                let _ = self.event_tx.send(Event::AudioDebugStats {
                    input_level: stats.input_level,
                    silero_vad_prob: stats.silero_vad_prob,
                    rnnoise_prob: stats.rnnoise_prob,
                    is_speaking: stats.is_speaking,
                    is_capturing: stats.is_capturing,
                    is_muted: stats.is_muted,
                    is_deafened: stats.is_deafened,
                    echo_cancellation_enabled: stats.echo_cancellation_enabled,
                    agc_enabled: stats.agc_enabled,
                    noise_suppression_enabled: stats.noise_suppression_enabled,
                    packets_encoded: stats.packets_encoded,
                    aec_capture_frames: stats.aec_capture_frames,
                    aec_render_frames: stats.aec_render_frames,
                    incoming_streams: stats.incoming_streams,
                    underrun_count: stats.underrun_count,
                    underrun_windowed: underrun_5s,
                    rtp_recv_total: stats.rtp_recv_total,
                    pipeline_delay_ms: stats.pipeline_delay_ms,
                    rtt_ms: rtt,
                });
            }
        }

        let mut buf = [0u8; PACKET_BUF_SIZE];
        let loopback_id = std::ffi::CString::new("loopback").unwrap();

        // Read outgoing audio packets from capture
        loop {
            let size = unsafe {
                mello_sys::mello_voice_get_packet(
                    self.ctx,
                    buf.as_mut_ptr(),
                    PACKET_BUF_SIZE as i32,
                )
            };
            if size <= 0 {
                break;
            }

            let pkt = &buf[..size as usize];

            if self.active {
                match self.mode {
                    VoiceMode::P2P => {
                        self.mesh.broadcast_audio(pkt);
                    }
                    VoiceMode::SFU => {
                        if let Some(ref conn) = self.sfu_connection {
                            // Strip the 4-byte LE sequence header; RTP handles sequencing
                            let opus_payload = if pkt.len() > 4 { &pkt[4..] } else { pkt };
                            match conn.send_audio(opus_payload) {
                                Ok(()) => {}
                                Err(e) => {
                                    log::warn!("SFU voice send failed: {}", e);
                                }
                            }
                        }
                    }
                    VoiceMode::Disconnected => {}
                }
            }

            if self.loopback {
                unsafe {
                    mello_sys::mello_voice_feed_packet(
                        self.ctx,
                        loopback_id.as_ptr(),
                        pkt.as_ptr(),
                        size,
                    );
                }
            }
        }

        // Read incoming audio from peers / SFU
        if self.active {
            match self.mode {
                VoiceMode::P2P => {
                    self.mesh.poll_incoming(self.ctx);
                }
                VoiceMode::SFU => {
                    // Audio arrives via AudioTrackData events (RTP track callback),
                    // signaling events (member join/leave/disconnect) also come here.
                    if let Some(ref conn) = self.sfu_connection {
                        for event in conn.poll_events() {
                            match event {
                                SfuEvent::AudioTrackData { sender_id, data } => {
                                    let peer_id =
                                        CString::new(sender_id.as_str()).unwrap_or_default();
                                    unsafe {
                                        mello_sys::mello_voice_feed_packet(
                                            self.ctx,
                                            peer_id.as_ptr(),
                                            data.as_ptr(),
                                            data.len() as i32,
                                        );
                                    }
                                }
                                SfuEvent::MemberJoined { user_id, .. } => {
                                    log::info!("SFU: member joined voice: {}", user_id);
                                    let _ = self.event_tx.send(Event::VoiceMembershipChanged {
                                        crew_id: self.sfu_crew_id.clone(),
                                    });
                                }
                                SfuEvent::MemberLeft { user_id, reason } => {
                                    log::info!("SFU: member left voice: {} ({})", user_id, reason);
                                    let _ = self.event_tx.send(Event::VoiceMembershipChanged {
                                        crew_id: self.sfu_crew_id.clone(),
                                    });
                                }
                                SfuEvent::Disconnected { reason } => {
                                    log::warn!("SFU voice disconnected: {}", reason);
                                    self.mark_disconnected_with_reason(reason);
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                VoiceMode::Disconnected => {}
            }
        }
    }
}

impl Drop for VoiceManager {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            self.leave_voice();
            unsafe {
                mello_sys::mello_destroy(self.ctx);
                mello_sys::mello_set_log_callback(None, std::ptr::null_mut());
            }
        }
    }
}
