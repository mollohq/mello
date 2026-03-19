mod mesh;

use std::ffi::CString;
use std::sync::mpsc as std_mpsc;

use crate::events::Event;

pub use mesh::{SignalEnvelope, SignalMessage, SignalPurpose, VoiceMesh};

const PACKET_BUF_SIZE: usize = 4000;
const MAX_DEVICES: usize = 32;

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

        // Set up VAD callback to send speaking events
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

        let result = unsafe { mello_sys::mello_voice_start_capture(self.ctx) };
        if result != mello_sys::MelloResult_MELLO_OK {
            log::error!("Failed to start voice capture: {}", result);
            return;
        }

        self.active = true;
        log::info!("Voice capture started, {} peers to connect", members.len());

        // Create peer connections for each member and generate offers/answers
        for member_id in members {
            self.mesh.create_peer(self.ctx, local_id, member_id);
        }
    }

    pub fn leave_voice(&mut self) {
        if !self.active {
            return;
        }

        unsafe {
            mello_sys::mello_voice_stop_capture(self.ctx);
        }
        self.mesh.destroy_all_peers();
        self.active = false;
        log::info!("Voice stopped");
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

    pub fn set_capture_device(&mut self, device_id: &str) {
        if self.ctx.is_null() {
            return;
        }
        let c_id = CString::new(device_id).unwrap_or_default();
        let result = unsafe { mello_sys::mello_set_audio_input(self.ctx, c_id.as_ptr()) };
        if result != mello_sys::MelloResult_MELLO_OK {
            log::error!("Failed to set capture device: {}", result);
        } else {
            log::info!("Capture device set to: {}", device_id);
        }
    }

    pub fn set_playback_device(&mut self, device_id: &str) {
        if self.ctx.is_null() {
            return;
        }
        let c_id = CString::new(device_id).unwrap_or_default();
        let result = unsafe { mello_sys::mello_set_audio_output(self.ctx, c_id.as_ptr()) };
        if result != mello_sys::MelloResult_MELLO_OK {
            log::error!("Failed to set playback device: {}", result);
        } else {
            log::info!("Playback device set to: {}", device_id);
        }
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
        self.mesh.handle_signal(self.ctx, from, signal);
    }

    /// Called when a new member joins the crew while voice is active
    pub fn on_member_joined(&mut self, local_id: &str, member_id: &str) {
        if !self.active {
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

        if self.debug_mode && self.tick_counter.is_multiple_of(3) {
            let mut stats: mello_sys::MelloDebugStats = unsafe { std::mem::zeroed() };
            unsafe {
                mello_sys::mello_get_debug_stats(self.ctx, &mut stats);
            }
            let _ = self.event_tx.send(Event::AudioDebugStats {
                input_level: stats.input_level,
                silero_vad_prob: stats.silero_vad_prob,
                rnnoise_prob: stats.rnnoise_prob,
                is_speaking: stats.is_speaking,
                is_capturing: stats.is_capturing,
                is_muted: stats.is_muted,
                is_deafened: stats.is_deafened,
                packets_encoded: stats.packets_encoded,
            });
        }

        let mut buf = [0u8; PACKET_BUF_SIZE];
        let loopback_id = std::ffi::CString::new("loopback").unwrap();

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
                self.mesh.broadcast_audio(pkt);
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

        if self.active {
            self.mesh.poll_incoming(self.ctx);
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
