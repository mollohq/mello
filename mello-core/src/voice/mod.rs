mod mesh;

use std::ffi::CString;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;

use crate::events::Event;
use crate::transport::{SfuConnection, SfuEvent};
use serde::{Deserialize, Serialize};

pub use mesh::{SignalEnvelope, SignalMessage, SignalPurpose, VoiceMesh};

const PACKET_BUF_SIZE: usize = 4000;
const MAX_DEVICES: usize = 32;
const DEBUG_STATS_TICK_DIVISOR: u32 = 10; // 100Hz tick -> 10Hz debug updates

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

#[derive(Debug, Clone)]
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
    active: bool,
    loopback: bool,
    debug_mode: bool,
    tick_counter: u32,
    mode: VoiceMode,
    sfu_connection: Option<Arc<SfuConnection>>,
    sfu_crew_id: String,
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
            active: false,
            loopback,
            debug_mode: false,
            tick_counter: 0,
            mode: VoiceMode::Disconnected,
            sfu_connection: None,
            sfu_crew_id: String::new(),
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
        log::info!("Voice capture started (SFU)");
    }

    pub fn leave_voice(&mut self) {
        if !self.active {
            return;
        }

        unsafe {
            mello_sys::mello_voice_stop_capture(self.ctx);
        }

        match self.mode {
            VoiceMode::P2P => {
                self.mesh.destroy_all_peers();
            }
            VoiceMode::SFU => {
                self.sfu_connection = None;
                self.sfu_crew_id.clear();
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

        // Send a DC ping every ~2 seconds (200 ticks at 100Hz)
        if self.tick_counter.is_multiple_of(200) {
            if let Some(conn) = &self.sfu_connection {
                conn.send_ping();
            }
        }

        if self.debug_mode && self.tick_counter.is_multiple_of(DEBUG_STATS_TICK_DIVISOR) {
            let mut stats: mello_sys::MelloDebugStats = unsafe { std::mem::zeroed() };
            unsafe {
                mello_sys::mello_get_debug_stats(self.ctx, &mut stats);
            }
            let rtt = self.sfu_connection.as_ref().map_or(0.0, |c| c.rtt_ms());
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
                rtp_recv_total: stats.rtp_recv_total,
                pipeline_delay_ms: stats.pipeline_delay_ms,
                rtt_ms: rtt,
            });
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
                                    let crew_id = self.sfu_crew_id.clone();
                                    self.sfu_connection = None;
                                    self.active = false;
                                    self.mode = VoiceMode::Disconnected;
                                    let _ = self
                                        .event_tx
                                        .send(Event::VoiceSfuDisconnected { crew_id, reason });
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
