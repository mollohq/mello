mod mesh;

use std::sync::mpsc as std_mpsc;

use crate::events::Event;

pub use mesh::{SignalMessage, VoiceMesh};

const PACKET_BUF_SIZE: usize = 4000;

struct VadCallbackData {
    tx: std_mpsc::Sender<Event>,
    local_id: String,
}

pub struct VoiceManager {
    ctx: *mut mello_sys::MelloContext,
    event_tx: std_mpsc::Sender<Event>,
    mesh: VoiceMesh,
    muted: bool,
    deafened: bool,
    active: bool,
    loopback: bool,
}

// Safety: VoiceManager is only ever accessed from a single tokio task (the client run loop).
// The raw pointers to libmello objects are thread-safe because libmello uses internal locking.
unsafe impl Send for VoiceManager {}
unsafe impl Sync for VoiceManager {}

impl VoiceManager {
    pub fn new(event_tx: std_mpsc::Sender<Event>, loopback: bool) -> Self {
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
        }
    }

    pub fn join_voice(&mut self, local_id: &str, members: &[String]) {
        if self.ctx.is_null() { return; }

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
        if !self.active { return; }

        unsafe { mello_sys::mello_voice_stop_capture(self.ctx); }
        self.mesh.destroy_all_peers();
        self.active = false;
        log::info!("Voice stopped");
    }

    pub fn set_mute(&mut self, muted: bool) {
        self.muted = muted;
        if !self.ctx.is_null() {
            unsafe { mello_sys::mello_voice_set_mute(self.ctx, muted); }
        }
    }

    pub fn set_deafen(&mut self, deafened: bool) {
        self.deafened = deafened;
        if !self.ctx.is_null() {
            unsafe { mello_sys::mello_voice_set_deafen(self.ctx, deafened); }
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
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
        if !self.active { return; }
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
        if !self.active || self.ctx.is_null() { return; }

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
            if size <= 0 { break; }

            let pkt = &buf[..size as usize];
            self.mesh.broadcast_audio(pkt);

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

        self.mesh.poll_incoming(self.ctx);
    }
}

impl Drop for VoiceManager {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            self.leave_voice();
            unsafe { mello_sys::mello_destroy(self.ctx); }
        }
    }
}
