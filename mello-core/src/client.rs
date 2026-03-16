use tokio::sync::mpsc;

use crate::command::Command;
use crate::config::Config;
use crate::events::Event;
use crate::nakama::NakamaClient;
use crate::presence::PresenceStatus;
use crate::session;
use crate::nakama::{InternalPresence, InternalSignal};
use crate::stream::manager::StreamSession;
use crate::stream::sink_p2p::P2PFanoutSink;
use crate::stream::viewer::{StreamViewer, ViewerAction, ViewerFeedResult};
use crate::voice::{SignalEnvelope, SignalMessage, SignalPurpose, VoiceManager};

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::sync::Arc;

const VIEWER_RECV_BUF_SIZE: usize = 64 * 1024;

struct FrameCallbackData {
    tx: std::sync::mpsc::Sender<Event>,
}

/// State for the viewer-side streaming pipeline.
struct ViewerState {
    viewer: *mut mello_sys::MelloStreamView,
    peer: *mut mello_sys::MelloPeerConnection,
    host_id: String,
    _frame_cb_data: *mut FrameCallbackData,
    _ice_cb_data: *mut StreamIceCallbackData,
    got_keyframe: bool,
    recv_buf: Vec<u8>,
    stream_viewer: StreamViewer,
}

unsafe impl Send for ViewerState {}
unsafe impl Sync for ViewerState {}

impl Drop for ViewerState {
    fn drop(&mut self) {
        unsafe {
            if !self.viewer.is_null() {
                mello_sys::mello_stream_stop_viewer(self.viewer);
            }
            if !self.peer.is_null() {
                mello_sys::mello_peer_destroy(self.peer);
            }
            if !self._frame_cb_data.is_null() {
                drop(Box::from_raw(self._frame_cb_data));
            }
            if !self._ice_cb_data.is_null() {
                drop(Box::from_raw(self._ice_cb_data));
            }
        }
    }
}

struct StreamIceCallbackData {
    peer_id: String,
    #[allow(dead_code)]
    purpose: SignalPurpose,
    queue: std::sync::Arc<std::sync::Mutex<Vec<(String, SignalEnvelope)>>>,
}

struct StreamHostPeer {
    peer: *mut mello_sys::MelloPeerConnection,
    ice_cb_data: *mut StreamIceCallbackData,
}

unsafe impl Send for StreamHostPeer {}
unsafe impl Sync for StreamHostPeer {}

unsafe extern "C" fn on_viewer_frame(
    user_data: *mut std::ffi::c_void,
    rgba: *const u8,
    w: u32,
    h: u32,
    _ts: u64,
) {
    if user_data.is_null() || rgba.is_null() || w == 0 || h == 0 { return; }
    let data = &*(user_data as *const FrameCallbackData);
    let pixel_count = (w * h) as usize;
    let buf = std::slice::from_raw_parts(rgba, pixel_count * 4).to_vec();
    let _ = data.tx.send(Event::StreamFrame {
        width: w,
        height: h,
        rgba: buf,
    });
}

unsafe extern "C" fn stream_ice_callback(
    user_data: *mut std::ffi::c_void,
    candidate: *const mello_sys::MelloIceCandidate,
) {
    if user_data.is_null() || candidate.is_null() { return; }
    let data = &*(user_data as *const StreamIceCallbackData);
    let c = &*candidate;
    let cand = CStr::from_ptr(c.candidate).to_string_lossy().into_owned();
    let mid = CStr::from_ptr(c.sdp_mid).to_string_lossy().into_owned();
    let idx = c.sdp_mline_index;
    log::debug!("Stream ICE candidate gathered for peer {}: {}", data.peer_id, cand);
    if let Ok(mut queue) = data.queue.lock() {
        queue.push((
            data.peer_id.clone(),
            SignalEnvelope {
                purpose: SignalPurpose::Stream,
                message: SignalMessage::IceCandidate {
                    candidate: cand,
                    sdp_mid: mid,
                    sdp_mline_index: idx,
                },
            },
        ));
    }
}

pub struct Client {
    nakama: NakamaClient,
    voice: VoiceManager,
    event_tx: std::sync::mpsc::Sender<Event>,
    stream_session: Option<StreamSession>,
    stream_sink: Option<Arc<P2PFanoutSink>>,
    stream_host_peers: HashMap<String, StreamHostPeer>,
    viewer_state: Option<ViewerState>,
    stream_signal_queue: Arc<std::sync::Mutex<Vec<(String, SignalEnvelope)>>>,
    ice_servers: Vec<String>,
}

impl Client {
    pub fn new(config: Config, event_tx: std::sync::mpsc::Sender<Event>, loopback: bool) -> Self {
        Self {
            nakama: NakamaClient::new(config),
            voice: VoiceManager::new(event_tx.clone(), loopback),
            event_tx,
            stream_session: None,
            stream_sink: None,
            stream_host_peers: HashMap::new(),
            viewer_state: None,
            stream_signal_queue: Arc::new(std::sync::Mutex::new(Vec::new())),
            ice_servers: Vec::new(),
        }
    }

    pub async fn run(&mut self, mut cmd_rx: mpsc::Receiver<Command>) {
        log::info!("Mello client started, waiting for commands...");

        let mut signal_rx = self.nakama.take_signal_rx().unwrap();
        let mut presence_rx = self.nakama.take_presence_rx().unwrap();
        let mut voice_tick = tokio::time::interval(tokio::time::Duration::from_millis(20));
        // Refresh access token every 45 minutes (token lives 1 hour)
        let mut refresh_tick = tokio::time::interval(tokio::time::Duration::from_secs(45 * 60));
        refresh_tick.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(cmd) => self.handle_command(cmd).await,
                        None => break,
                    }
                }
                signal = signal_rx.recv() => {
                    if let Some(sig) = signal {
                        self.handle_signal(sig);
                    }
                }
                presence = presence_rx.recv() => {
                    if let Some(p) = presence {
                        self.handle_presence(p);
                    }
                }
                _ = voice_tick.tick() => {
                    self.voice_tick().await;
                    self.stream_tick().await;
                }
                _ = refresh_tick.tick() => {
                    self.refresh_token().await;
                }
            }
        }
        log::info!("Mello client shutting down");
    }

    fn handle_presence(&mut self, presence: InternalPresence) {
        if !self.voice.is_active() { return; }

        let local_id = match self.nakama.current_user_id() {
            Some(id) => id.to_string(),
            None => return,
        };

        match presence {
            InternalPresence::Joined { user_id } => {
                if user_id != local_id {
                    log::info!("Presence: member {} joined channel, adding to voice mesh", user_id);
                    self.voice.on_member_joined(&local_id, &user_id);
                }
            }
            InternalPresence::Left { user_id } => {
                if user_id != local_id {
                    log::info!("Presence: member {} left channel, removing from voice mesh", user_id);
                    self.voice.on_member_left(&user_id);
                }
            }
        }
    }

    fn handle_signal(&mut self, signal: InternalSignal) {
        match serde_json::from_str::<SignalEnvelope>(&signal.payload) {
            Ok(env) => {
                match env.purpose {
                    SignalPurpose::Voice => {
                        log::info!("Voice signal from {}: {:?}", signal.from, env.message);
                        self.voice.handle_signal(&signal.from, env.message);
                    }
                    SignalPurpose::Stream => {
                        log::info!("Stream signal from {}: {:?}", signal.from, env.message);
                        self.handle_stream_signal(&signal.from, env.message);
                    }
                }
            }
            Err(_) => {
                // Backward compat: try parsing as bare SignalMessage (no envelope)
                match serde_json::from_str::<SignalMessage>(&signal.payload) {
                    Ok(msg) => {
                        log::info!("Voice signal (legacy) from {}: {:?}", signal.from, msg);
                        self.voice.handle_signal(&signal.from, msg);
                    }
                    Err(e) => {
                        log::warn!("Failed to parse signal from {}: {}", signal.from, e);
                    }
                }
            }
        }
    }

    fn handle_stream_signal(&mut self, from: &str, message: SignalMessage) {
        // Host side: accept viewer offers, add peers to P2PFanoutSink
        if self.stream_session.is_some() {
            self.handle_stream_signal_as_host(from, message);
            return;
        }

        // Viewer side: handle answers and ICE from the host
        if self.viewer_state.is_some() {
            self.handle_stream_signal_as_viewer(from, message);
            return;
        }

        log::warn!("Stream signal from {} but not hosting or viewing — ignoring", from);
    }

    fn handle_stream_signal_as_host(&mut self, from: &str, message: SignalMessage) {
        let ctx = self.voice.mello_ctx();

        match message {
            SignalMessage::Offer { sdp } => {
                log::info!("Stream offer from viewer {}", from);

                if self.stream_host_peers.contains_key(from) {
                    log::warn!("Duplicate stream offer from {}, destroying old peer", from);
                    if let Some(old) = self.stream_host_peers.remove(from) {
                        if let Some(ref sink) = self.stream_sink {
                            sink.remove_viewer(from);
                        }
                        unsafe {
                            mello_sys::mello_peer_destroy(old.peer);
                            if !old.ice_cb_data.is_null() {
                                drop(Box::from_raw(old.ice_cb_data));
                            }
                        }
                    }
                }

                // Create peer for this viewer
                let peer_id_c = match CString::new(from) {
                    Ok(c) => c,
                    Err(_) => return,
                };
                let peer = unsafe { mello_sys::mello_peer_create(ctx, peer_id_c.as_ptr()) };
                if peer.is_null() {
                    log::error!("Failed to create peer for stream viewer {}", from);
                    return;
                }

                // Configure ICE servers
                let ice_cstrings: Vec<CString> = self.ice_servers.iter()
                    .filter_map(|u| CString::new(u.as_str()).ok())
                    .collect();
                if !ice_cstrings.is_empty() {
                    let ptrs: Vec<*const std::os::raw::c_char> =
                        ice_cstrings.iter().map(|s| s.as_ptr()).collect();
                    unsafe {
                        mello_sys::mello_peer_set_ice_servers(
                            peer,
                            ptrs.as_ptr() as *mut *const std::os::raw::c_char,
                            ptrs.len() as std::os::raw::c_int,
                        );
                    }
                }

                // ICE callback
                let ice_cb_data = Box::into_raw(Box::new(StreamIceCallbackData {
                    peer_id: from.to_string(),
                    purpose: SignalPurpose::Stream,
                    queue: Arc::clone(&self.stream_signal_queue),
                }));
                unsafe {
                    mello_sys::mello_peer_set_ice_callback(
                        peer,
                        Some(stream_ice_callback),
                        ice_cb_data as *mut std::ffi::c_void,
                    );
                }

                // Create answer
                let sdp_c = match CString::new(sdp) {
                    Ok(c) => c,
                    Err(_) => {
                        unsafe {
                            mello_sys::mello_peer_destroy(peer);
                            drop(Box::from_raw(ice_cb_data));
                        }
                        return;
                    }
                };
                let answer_ptr = unsafe {
                    mello_sys::mello_peer_create_answer(peer, sdp_c.as_ptr())
                };
                if answer_ptr.is_null() {
                    log::error!("Failed to create stream answer for viewer {}", from);
                    unsafe {
                        mello_sys::mello_peer_destroy(peer);
                        drop(Box::from_raw(ice_cb_data));
                    }
                    return;
                }
                let answer = unsafe { CStr::from_ptr(answer_ptr) }
                    .to_string_lossy()
                    .into_owned();
                log::info!("Created stream answer for viewer {}", from);

                // Queue answer for sending
                if let Ok(mut queue) = self.stream_signal_queue.lock() {
                    queue.push((
                        from.to_string(),
                        SignalEnvelope {
                            purpose: SignalPurpose::Stream,
                            message: SignalMessage::Answer { sdp: answer },
                        },
                    ));
                }

                // Add peer to P2PFanoutSink
                if let Some(ref sink) = self.stream_sink {
                    if let Err(e) = sink.add_viewer(from.to_string(), peer) {
                        log::error!("Failed to add viewer {} to sink: {}", from, e);
                        unsafe {
                            mello_sys::mello_peer_destroy(peer);
                            drop(Box::from_raw(ice_cb_data));
                        }
                        return;
                    }
                }

                self.stream_host_peers.insert(from.to_string(), StreamHostPeer {
                    peer,
                    ice_cb_data,
                });

                let _ = self.event_tx.send(Event::StreamViewerJoined {
                    viewer_id: from.to_string(),
                });
            }
            SignalMessage::IceCandidate { candidate, sdp_mid, sdp_mline_index } => {
                if let Some(hp) = self.stream_host_peers.get(from) {
                    let cand_c = match CString::new(candidate) {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let mid_c = match CString::new(sdp_mid) {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let ice = mello_sys::MelloIceCandidate {
                        candidate: cand_c.as_ptr(),
                        sdp_mid: mid_c.as_ptr(),
                        sdp_mline_index,
                    };
                    unsafe {
                        mello_sys::mello_peer_add_ice_candidate(hp.peer, &ice);
                    }
                    log::debug!("Added stream ICE candidate from viewer {}", from);
                } else {
                    log::warn!("Stream ICE candidate from unknown viewer {}", from);
                }
            }
            SignalMessage::Answer { .. } => {
                log::warn!("Unexpected stream Answer from {} while hosting — ignoring", from);
            }
        }
    }

    fn handle_stream_signal_as_viewer(&mut self, from: &str, message: SignalMessage) {
        let vs = match self.viewer_state.as_ref() {
            Some(vs) => vs,
            None => return,
        };

        if from != vs.host_id {
            log::warn!("Stream signal from {} but we're watching {} — ignoring", from, vs.host_id);
            return;
        }

        match message {
            SignalMessage::Answer { sdp } => {
                let sdp_c = match CString::new(sdp) {
                    Ok(c) => c,
                    Err(_) => return,
                };
                unsafe {
                    mello_sys::mello_peer_set_remote_description(vs.peer, sdp_c.as_ptr(), false);
                }
                log::info!("Set stream remote answer from host {}", from);
            }
            SignalMessage::IceCandidate { candidate, sdp_mid, sdp_mline_index } => {
                let cand_c = match CString::new(candidate) {
                    Ok(c) => c,
                    Err(_) => return,
                };
                let mid_c = match CString::new(sdp_mid) {
                    Ok(c) => c,
                    Err(_) => return,
                };
                let ice = mello_sys::MelloIceCandidate {
                    candidate: cand_c.as_ptr(),
                    sdp_mid: mid_c.as_ptr(),
                    sdp_mline_index,
                };
                unsafe {
                    mello_sys::mello_peer_add_ice_candidate(vs.peer, &ice);
                }
                log::debug!("Added stream ICE candidate from host {}", from);
            }
            SignalMessage::Offer { .. } => {
                log::warn!("Unexpected stream Offer from {} while viewing — ignoring", from);
            }
        }
    }

    async fn refresh_token(&mut self) {
        if let Some(rt) = self.nakama.refresh_token().map(String::from) {
            match self.nakama.refresh_session(&rt).await {
                Ok(user) => {
                    log::info!("Access token refreshed for {}", user.display_name);
                    if let Some(new_rt) = self.nakama.refresh_token() {
                        if let Err(e) = session::save(new_rt) {
                            log::warn!("Failed to save refreshed token: {}", e);
                        }
                    }
                }
                Err(e) => {
                    log::error!("Token refresh failed: {}", e);
                }
            }
        }
    }

    async fn voice_tick(&mut self) {
        self.voice.tick();

        // Send any pending signaling messages through Nakama
        let signals = self.voice.drain_signals();
        for (to, signal) in signals {
            let envelope = SignalEnvelope {
                purpose: SignalPurpose::Voice,
                message: signal,
            };
            let payload = match serde_json::to_string(&envelope) {
                Ok(p) => p,
                Err(e) => {
                    log::error!("Failed to serialize signal: {}", e);
                    continue;
                }
            };
            if let Err(e) = self.nakama.send_signal(&to, &payload).await {
                log::error!("Failed to send signal to {}: {}", to, e);
            }
        }
    }

    async fn stream_tick(&mut self) {
        // 1. Drain stream signal queue and send via Nakama
        let signals: Vec<(String, SignalEnvelope)> = {
            match self.stream_signal_queue.lock() {
                Ok(mut q) => std::mem::take(&mut *q),
                Err(_) => Vec::new(),
            }
        };
        for (to, envelope) in signals {
            let payload = match serde_json::to_string(&envelope) {
                Ok(p) => p,
                Err(e) => {
                    log::error!("Failed to serialize stream signal: {}", e);
                    continue;
                }
            };
            if let Err(e) = self.nakama.send_signal(&to, &payload).await {
                log::error!("Failed to send stream signal to {}: {}", to, e);
            }
        }

        // 2. Poll viewer for incoming stream packets
        if self.viewer_state.is_none() {
            return;
        }

        let vs = self.viewer_state.as_mut().unwrap();
        let peer = vs.peer;
        let viewer = vs.viewer;
        let mut fed_any = false;

        loop {
            let size = unsafe {
                mello_sys::mello_peer_recv(peer, vs.recv_buf.as_mut_ptr(), vs.recv_buf.len() as i32)
            };
            if size <= 0 { break; }

            let data = &vs.recv_buf[..size as usize];
            let results = vs.stream_viewer.feed_packet(data);

            for result in results {
                match result {
                    ViewerFeedResult::VideoPayload { data: payload, is_keyframe } |
                    ViewerFeedResult::RecoveredVideoPayload { data: payload, is_keyframe } => {
                        if !vs.got_keyframe {
                            if is_keyframe {
                                vs.got_keyframe = true;
                                log::info!("First keyframe received — stream decode starting");
                            } else {
                                continue;
                            }
                        }
                        unsafe {
                            mello_sys::mello_stream_feed_packet(
                                viewer,
                                payload.as_ptr(),
                                payload.len() as i32,
                                is_keyframe,
                            );
                        }
                        fed_any = true;
                    }
                    ViewerFeedResult::AudioPayload(payload) => {
                        unsafe {
                            mello_sys::mello_stream_feed_audio_packet(
                                viewer,
                                payload.as_ptr(),
                                payload.len() as i32,
                            );
                        }
                    }
                    ViewerFeedResult::Action(ViewerAction::SendControl(ctrl_data)) => {
                        let connected = unsafe { mello_sys::mello_peer_is_connected(peer) };
                        if connected {
                            unsafe {
                                mello_sys::mello_peer_send_unreliable(
                                    peer,
                                    ctrl_data.as_ptr(),
                                    ctrl_data.len() as i32,
                                );
                            }
                        }
                    }
                    ViewerFeedResult::None => {}
                }
            }
        }

        // Present the latest decoded frame (triggers the frame callback)
        if fed_any {
            unsafe { mello_sys::mello_stream_present_frame(viewer); }
        }
    }

    async fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::TryRestore => {
                self.handle_restore().await;
            }
            Command::DeviceAuth { device_id } => {
                self.handle_device_auth(&device_id).await;
            }
            Command::Login { email, password } => {
                self.handle_login(&email, &password).await;
            }
            Command::LinkEmail { email, password } => {
                self.handle_link_email(&email, &password).await;
            }
            Command::Logout => {
                self.handle_logout().await;
            }

            // Social auth
            Command::AuthSteam => {
                log::info!("[auth] Steam auth requested");
                // TODO: implemented by client/src/auth/steam.rs -> sends ticket to Nakama
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: "Steam auth not yet implemented".into(),
                });
            }
            Command::AuthGoogle => {
                log::info!("[auth] Google auth requested");
                self.handle_auth_google().await;
            }
            Command::AuthTwitch => {
                log::info!("[auth] Twitch auth requested");
                // TODO: OAuth2 PKCE flow -> access_token -> Nakama /authenticate/custom
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: "Twitch auth not yet implemented".into(),
                });
            }
            Command::AuthDiscord => {
                log::info!("[auth] Discord auth requested");
                self.handle_auth_discord().await;
            }
            Command::AuthApple => {
                log::info!("[auth] Apple auth requested");
                // TODO: Apple Sign In -> id_token -> Nakama /authenticate/apple
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: "Apple auth not yet implemented".into(),
                });
            }

            // Social link (onboarding — attaches identity to current device account)
            Command::LinkGoogle => {
                log::info!("[auth] Google link requested");
                self.handle_link_google().await;
            }
            Command::LinkDiscord => {
                log::info!("[auth] Discord link requested");
                self.handle_link_discord().await;
            }
            Command::DiscoverCrews => {
                self.handle_discover_crews().await;
            }
            Command::LoadMyCrews => {
                self.load_crews().await;
            }
            Command::JoinCrew { crew_id } => {
                self.handle_join_crew(&crew_id).await;
            }
            Command::CreateCrew { name } => {
                self.handle_create_crew(&name).await;
            }
            Command::SelectCrew { crew_id } => {
                self.handle_select_crew(&crew_id).await;
            }
            Command::LeaveCrew => {
                self.handle_leave_crew().await;
            }
            Command::SendMessage { content } => {
                self.handle_send_message(&content).await;
            }
            Command::JoinVoice { channel_id } => {
                self.handle_join_voice(&channel_id).await;
            }
            Command::LeaveVoice => {
                self.handle_leave_voice().await;
            }
            Command::SetMute { muted } => {
                self.voice.set_mute(muted);
            }
            Command::SetDeafen { deafened } => {
                self.voice.set_deafen(deafened);
            }
            Command::ListAudioDevices => {
                let capture = self.voice.list_capture_devices();
                let playback = self.voice.list_playback_devices();
                let _ = self.event_tx.send(Event::AudioDevicesListed { capture, playback });
            }
            Command::SetCaptureDevice { id } => {
                self.voice.set_capture_device(&id);
            }
            Command::SetPlaybackDevice { id } => {
                self.voice.set_playback_device(&id);
            }
            Command::SetLoopback { enabled } => {
                self.voice.set_loopback(enabled);
            }
            Command::SetDebugMode { enabled } => {
                self.voice.set_debug_mode(enabled);
            }
            Command::UpdateProfile { display_name } => {
                self.handle_update_profile(&display_name).await;
            }
            // --- Streaming ---
            Command::ListCaptureSources => {
                self.handle_list_capture_sources();
            }
            Command::StartStream { crew_id, title, capture_mode, monitor_index, hwnd, pid } => {
                self.handle_start_stream(&crew_id, &title, &capture_mode, monitor_index, hwnd, pid).await;
            }
            Command::StopStream => {
                self.handle_stop_stream().await;
            }
            Command::WatchStream { host_id } => {
                self.handle_watch_stream(&host_id);
            }
            Command::StopWatching => {
                self.handle_stop_watching();
            }

            // --- Voice channels CRUD ---
            Command::CreateVoiceChannel { crew_id, name } => {
                self.handle_create_voice_channel(&crew_id, &name).await;
            }
            Command::RenameVoiceChannel { crew_id, channel_id, name } => {
                self.handle_rename_voice_channel(&crew_id, &channel_id, &name).await;
            }
            Command::DeleteVoiceChannel { crew_id, channel_id } => {
                self.handle_delete_voice_channel(&crew_id, &channel_id).await;
            }

            // --- Presence & crew state ---
            Command::UpdatePresence { status, activity } => {
                if let Err(e) = self.nakama.presence_update(&status, activity.as_ref()).await {
                    log::error!("Failed to update presence: {}", e);
                }
            }
            Command::SetActiveCrew { crew_id } => {
                self.handle_set_active_crew(&crew_id).await;
            }
            Command::SubscribeSidebar { crew_ids } => {
                self.handle_subscribe_sidebar(&crew_ids).await;
            }
        }
    }

    async fn handle_device_auth(&mut self, device_id: &str) {
        match self.nakama.authenticate_device(device_id).await {
            Ok((user, created)) => {
                log::info!("Device auth succeeded for {} (created={})", user.id, created);
                if let Some(rt) = self.nakama.refresh_token() {
                    let _ = session::save(rt);
                }
                if let Err(e) = self.nakama.connect_ws(self.event_tx.clone()).await {
                    log::error!("WebSocket connect failed after device auth: {}", e);
                }
                self.on_connected().await;
                let _ = self.event_tx.send(Event::DeviceAuthed { user, created });
            }
            Err(e) => {
                log::error!("Device auth failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn handle_discover_crews(&self) {
        match self.nakama.list_groups(50).await {
            Ok(crews) => {
                let _ = self.event_tx.send(Event::DiscoverCrewsLoaded { crews });
            }
            Err(e) => {
                log::error!("Failed to discover crews: {}", e);
            }
        }
    }

    async fn handle_join_crew(&mut self, crew_id: &str) {
        if let Err(e) = self.nakama.join_group(crew_id).await {
            log::error!("Failed to join crew {}: {}", crew_id, e);
            let _ = self.event_tx.send(Event::Error {
                message: format!("Failed to join crew: {}", e),
            });
            return;
        }
        self.handle_select_crew(crew_id).await;
        self.load_crews().await;
    }

    async fn handle_link_email(&mut self, email: &str, password: &str) {
        match self.nakama.link_email(email, password).await {
            Ok(()) => {
                log::info!("Email linked successfully");
                let _ = self.event_tx.send(Event::EmailLinked);
            }
            Err(e) => {
                log::error!("Email link failed: {}", e);
                let _ = self.event_tx.send(Event::EmailLinkFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn handle_restore(&mut self) {
        let token = match session::load() {
            Some(t) => {
                log::info!("Found stored refresh token, attempting restore...");
                t
            }
            None => {
                log::info!("No stored session found");
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: String::new(),
                });
                return;
            }
        };

        let _ = self.event_tx.send(Event::Restoring);

        match self.nakama.refresh_session(&token).await {
            Ok(user) => {
                log::info!("Session restored for {}", user.display_name);

                if let Some(new_rt) = self.nakama.refresh_token() {
                    let _ = session::save(new_rt);
                }

                if let Err(e) = self.nakama.connect_ws(self.event_tx.clone()).await {
                    log::error!("WebSocket connect failed on restore: {}", e);
                    session::clear();
                    let _ = self.event_tx.send(Event::LoginFailed {
                        reason: format!("WebSocket failed: {}", e),
                    });
                    return;
                }

                self.on_connected().await;
                let _ = self.event_tx.send(Event::LoggedIn { user });
                self.load_crews().await;
            }
            Err(e) => {
                log::warn!("Session restore failed ({}), clearing", e);
                session::clear();
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: String::new(),
                });
            }
        }
    }

    async fn handle_login(&mut self, email: &str, password: &str) {
        match self.nakama.login_email(email, password).await {
            Ok(user) => {
                log::info!("Logged in as {} ({})", user.display_name, user.tag);

                match self.nakama.refresh_token() {
                    Some(rt) => {
                        log::info!("Saving refresh token to keyring");
                        if let Err(e) = session::save(rt) {
                            log::warn!("Failed to save session: {}", e);
                        }
                    }
                    None => {
                        log::warn!("No refresh token returned by server");
                    }
                }

                if let Err(e) = self.nakama.connect_ws(self.event_tx.clone()).await {
                    log::error!("WebSocket connect failed: {}", e);
                    let _ = self.event_tx.send(Event::LoginFailed {
                        reason: format!("WebSocket failed: {}", e),
                    });
                    return;
                }

                self.on_connected().await;
                let _ = self.event_tx.send(Event::LoggedIn { user });
                self.load_crews().await;
            }
            Err(e) => {
                log::error!("Login failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn handle_auth_google(&mut self) {
        let client_id = match self.nakama.config().google_client_id.clone() {
            Some(id) => id,
            None => {
                log::warn!("[auth] GOOGLE_CLIENT_ID not configured");
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: "Google login not configured".into(),
                });
                return;
            }
        };

        let oauth_result = tokio::task::spawn_blocking(move || {
            crate::auth_google::GoogleAuth::authenticate(&client_id)
        })
        .await;

        let (code, verifier) = match oauth_result {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                log::error!("[auth] Google OAuth flow failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: format!("Google sign-in failed: {}", e),
                });
                return;
            }
            Err(e) => {
                log::error!("[auth] Google OAuth task panicked: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: "Google sign-in failed unexpectedly".into(),
                });
                return;
            }
        };

        match self.nakama.authenticate_google(&code, &verifier).await {
            Ok(user) => self.on_social_login(user).await,
            Err(e) => {
                log::error!("[auth] Google Nakama auth failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn handle_auth_discord(&mut self) {
        let client_id = match self.nakama.config().discord_client_id.clone() {
            Some(id) => id,
            None => {
                log::warn!("[auth] DISCORD_CLIENT_ID not configured");
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: "Discord login not configured".into(),
                });
                return;
            }
        };

        let oauth_result = tokio::task::spawn_blocking(move || {
            crate::auth_discord::DiscordAuth::authenticate(&client_id)
        })
        .await;

        let token = match oauth_result {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                log::error!("[auth] Discord OAuth flow failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: format!("Discord sign-in failed: {}", e),
                });
                return;
            }
            Err(e) => {
                log::error!("[auth] Discord OAuth task panicked: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: "Discord sign-in failed unexpectedly".into(),
                });
                return;
            }
        };

        match self.nakama.authenticate_custom(&token, "discord").await {
            Ok(user) => self.on_social_login(user).await,
            Err(e) => {
                log::error!("[auth] Discord Nakama auth failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    /// Shared post-auth flow for social logins (same as handle_login success path).
    async fn on_social_login(&mut self, user: crate::events::User) {
        log::info!("[auth] Social login success: {} ({})", user.display_name, user.tag);

        match self.nakama.refresh_token() {
            Some(rt) => {
                if let Err(e) = session::save(rt) {
                    log::warn!("Failed to save session: {}", e);
                }
            }
            None => {
                log::warn!("No refresh token returned by server");
            }
        }

        if let Err(e) = self.nakama.connect_ws(self.event_tx.clone()).await {
            log::error!("WebSocket connect failed: {}", e);
            let _ = self.event_tx.send(Event::LoginFailed {
                reason: format!("WebSocket failed: {}", e),
            });
            return;
        }

        self.on_connected().await;
        let _ = self.event_tx.send(Event::LoggedIn { user });
        self.load_crews().await;
    }

    async fn handle_link_google(&mut self) {
        let client_id = match self.nakama.config().google_client_id.clone() {
            Some(id) => id,
            None => {
                log::warn!("[auth] GOOGLE_CLIENT_ID not configured");
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: "Google login not configured".into(),
                });
                return;
            }
        };

        let oauth_result = tokio::task::spawn_blocking(move || {
            crate::auth_google::GoogleAuth::authenticate(&client_id)
        })
        .await;

        let (code, verifier) = match oauth_result {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                log::error!("[auth] Google OAuth flow failed: {}", e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: format!("Google sign-in failed: {}", e),
                });
                return;
            }
            Err(e) => {
                log::error!("[auth] Google OAuth task panicked: {}", e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: "Google sign-in failed unexpectedly".into(),
                });
                return;
            }
        };

        match self.nakama.link_google(&code, &verifier).await {
            Ok(()) => {
                log::info!("[auth] Google identity linked to device account");
                let _ = self.event_tx.send(Event::SocialLinked);
            }
            Err(e) => {
                log::error!("[auth] Google link failed: {}", e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn handle_link_discord(&mut self) {
        let client_id = match self.nakama.config().discord_client_id.clone() {
            Some(id) => id,
            None => {
                log::warn!("[auth] DISCORD_CLIENT_ID not configured");
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: "Discord login not configured".into(),
                });
                return;
            }
        };

        let oauth_result = tokio::task::spawn_blocking(move || {
            crate::auth_discord::DiscordAuth::authenticate(&client_id)
        })
        .await;

        let token = match oauth_result {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                log::error!("[auth] Discord OAuth flow failed: {}", e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: format!("Discord sign-in failed: {}", e),
                });
                return;
            }
            Err(e) => {
                log::error!("[auth] Discord OAuth task panicked: {}", e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: "Discord sign-in failed unexpectedly".into(),
                });
                return;
            }
        };

        match self.nakama.link_custom(&token, "discord").await {
            Ok(()) => {
                log::info!("[auth] Discord identity linked to device account");
                let _ = self.event_tx.send(Event::SocialLinked);
            }
            Err(e) => {
                log::error!("[auth] Discord link failed: {}", e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn handle_update_profile(&self, display_name: &str) {
        match self.nakama.update_account(display_name).await {
            Ok(()) => {
                log::info!("Profile updated: display_name={}", display_name);
            }
            Err(e) => {
                log::error!("Failed to update profile: {}", e);
            }
        }
    }

    async fn handle_logout(&mut self) {
        // Notify server we're going offline
        if let Err(e) = self.nakama.presence_update(&PresenceStatus::Offline, None).await {
            log::warn!("Failed to set offline presence on logout: {}", e);
        }

        // Leave voice (local + server-side)
        if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
            if let Err(e) = self.nakama.voice_leave(&crew_id).await {
                log::warn!("Failed to voice_leave RPC on logout: {}", e);
            }
        }
        self.voice.leave_voice();
        let _ = self.event_tx.send(Event::VoiceStateChanged { in_call: false });

        session::clear();
        if let Err(e) = self.nakama.leave_crew_channel().await {
            log::warn!("Leave channel on logout: {}", e);
        }
        log::info!("Logged out, session cleared");
    }

    async fn load_crews(&self) {
        match self.nakama.list_user_groups().await {
            Ok(crews) => {
                // Subscribe sidebar for all crews
                let crew_ids: Vec<String> = crews.iter().map(|c| c.id.clone()).collect();
                if !crew_ids.is_empty() {
                    self.handle_subscribe_sidebar(&crew_ids).await;
                }
                let _ = self.event_tx.send(Event::CrewsLoaded { crews });
            }
            Err(e) => {
                log::error!("Failed to load crews: {}", e);
            }
        }
    }

    async fn handle_create_crew(&mut self, name: &str) {
        match self.nakama.create_crew(name).await {
            Ok(crew) => {
                let crew_id = crew.id.clone();
                let _ = self.event_tx.send(Event::CrewCreated { crew });
                self.handle_select_crew(&crew_id).await;
                self.load_crews().await;
            }
            Err(e) => {
                log::error!("Failed to create crew: {}", e);
                let _ = self.event_tx.send(Event::CrewCreateFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn handle_select_crew(&mut self, crew_id: &str) {
        self.voice.leave_voice();
        let _ = self.event_tx.send(Event::VoiceStateChanged { in_call: false });

        if let Err(e) = self.nakama.leave_crew_channel().await {
            log::warn!("Failed to leave previous channel: {}", e);
        }

        if let Err(e) = self.nakama.join_crew_channel(crew_id).await {
            log::error!("Failed to join crew channel: {}", e);
            return;
        }

        let _ = self.event_tx.send(Event::CrewJoined {
            crew_id: crew_id.to_string(),
        });

        // Tell the server this is our active crew (registers subscription + returns state)
        let local_user_id = self.nakama.current_user_id().map(String::from).unwrap_or_default();
        let voice_channel_id = match self.nakama.set_active_crew(crew_id).await {
            Ok(state) => {
                // Check if user is already in a channel (server remembers from last session)
                let already_in = state
                    .voice_channels
                    .iter()
                    .find(|ch| ch.members.iter().any(|m| m.user_id == local_user_id))
                    .map(|ch| ch.id.clone());
                // Fall back to default channel
                let target = already_in.or_else(|| {
                    state.voice_channels.iter()
                        .find(|ch| ch.is_default)
                        .or_else(|| state.voice_channels.first())
                        .map(|ch| ch.id.clone())
                });
                let _ = self.event_tx.send(Event::CrewStateLoaded { state });
                target
            }
            Err(e) => {
                log::warn!("set_active_crew RPC failed: {}", e);
                None
            }
        };

        // Wait for WS reader to set channel_id (up to 2s)
        let channel_id = self.wait_for_channel_id().await;
        if let Some(ch_id) = channel_id {
            match self.nakama.list_channel_messages(&ch_id, 50).await {
                Ok(mut messages) => {
                    messages.reverse();
                    let _ = self.event_tx.send(Event::MessagesLoaded { messages });
                }
                Err(e) => log::error!("Failed to fetch message history: {}", e),
            }
        }

        if let Ok(members) = self.nakama.list_group_users(crew_id).await {
            let user_ids: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
            if let Err(e) = self.nakama.follow_users(&user_ids).await {
                log::warn!("Failed to follow users: {}", e);
            }

            // Auto-join voice (last-used channel, or default if first time)
            if let Some(ch_id) = &voice_channel_id {
                self.handle_join_voice(ch_id).await;
            }
        }
    }

    /// Called after successful auth + WS connect. Sets online presence and fetches ICE config.
    async fn on_connected(&mut self) {
        if let Err(e) = self.nakama.presence_update(&PresenceStatus::Online, None).await {
            log::warn!("Failed to set online presence: {}", e);
        }

        self.check_protocol_version().await;

        match self.nakama.get_ice_servers().await {
            Ok(urls) => {
                log::info!("Fetched {} ICE server(s) from backend", urls.len());
                self.ice_servers = urls.clone();
                self.voice.set_ice_servers(urls);
            }
            Err(e) => {
                log::warn!("Failed to fetch ICE servers, using defaults: {}", e);
            }
        }
    }

    async fn check_protocol_version(&self) {
        match self.nakama.health_check().await {
            Ok(health) => {
                log::info!(
                    "Server health: status={} version={} protocol={}",
                    health.status, health.version,
                    health.protocol_version.unwrap_or(0),
                );

                if let Some(min_client) = health.min_client_protocol {
                    if crate::PROTOCOL_VERSION < min_client {
                        let msg = format!(
                            "Server requires protocol {} but client speaks {}. Please update Mello.",
                            min_client, crate::PROTOCOL_VERSION,
                        );
                        log::warn!("{}", msg);
                        let _ = self.event_tx.send(Event::ProtocolMismatch {
                            message: msg,
                            client_outdated: true,
                        });
                    }
                }

                if let Some(server_proto) = health.protocol_version {
                    if server_proto < crate::MIN_SERVER_PROTOCOL {
                        let msg = format!(
                            "Client requires server protocol {} but server speaks {}. Server needs updating.",
                            crate::MIN_SERVER_PROTOCOL, server_proto,
                        );
                        log::warn!("{}", msg);
                        let _ = self.event_tx.send(Event::ProtocolMismatch {
                            message: msg,
                            client_outdated: false,
                        });
                    }
                }
            }
            Err(e) => {
                log::warn!("Health check failed (server may be old): {}", e);
            }
        }
    }

    /// Tell the server which crew is active and get full state back.
    async fn handle_set_active_crew(&self, crew_id: &str) {
        match self.nakama.set_active_crew(crew_id).await {
            Ok(state) => {
                let _ = self.event_tx.send(Event::CrewStateLoaded { state });
            }
            Err(e) => {
                log::error!("set_active_crew failed: {}", e);
            }
        }
    }

    /// Subscribe to sidebar updates for the given crews.
    async fn handle_subscribe_sidebar(&self, crew_ids: &[String]) {
        match self.nakama.subscribe_sidebar(crew_ids).await {
            Ok(crews) => {
                let _ = self.event_tx.send(Event::SidebarUpdated { crews });
            }
            Err(e) => {
                log::warn!("subscribe_sidebar failed: {}", e);
            }
        }
    }

    async fn wait_for_channel_id(&self) -> Option<String> {
        for _ in 0..20 {
            if let Some(id) = self.nakama.channel_id().await {
                return Some(id);
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        log::warn!("Timed out waiting for channel_id");
        None
    }

    async fn handle_join_voice(&mut self, channel_id: &str) {
        let crew_id = match self.nakama.active_crew_id().map(String::from) {
            Some(id) => id,
            None => return,
        };

        // RPC returns the authoritative channel state after join
        let resp = match self.nakama.voice_join(&crew_id, channel_id).await {
            Ok(r) => r,
            Err(e) => {
                log::error!("voice_join RPC failed: {}", e);
                return;
            }
        };

        // Rejoin local voice mesh with the members from the response
        self.voice.leave_voice();
        if let Some(local_id) = self.nakama.current_user_id().map(String::from) {
            let peer_ids: Vec<String> = resp.voice_state.members.iter()
                .filter(|m| m.user_id != local_id)
                .map(|m| m.user_id.clone())
                .collect();
            self.voice.join_voice(&local_id, &peer_ids);
            let _ = self.event_tx.send(Event::VoiceStateChanged { in_call: true });
        }

        // Emit authoritative state so the UI can update members + active channel
        let _ = self.event_tx.send(Event::VoiceJoined {
            crew_id,
            channel_id: resp.channel_id,
            members: resp.voice_state.members,
        });
    }

    async fn handle_leave_voice(&mut self) {
        // Notify server
        if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
            if let Err(e) = self.nakama.voice_leave(&crew_id).await {
                log::warn!("voice_leave RPC failed: {}", e);
            }
        }
        self.voice.leave_voice();
        let _ = self.event_tx.send(Event::VoiceStateChanged { in_call: false });
    }

    async fn handle_create_voice_channel(&self, crew_id: &str, name: &str) {
        match self.nakama.channel_create(crew_id, name).await {
            Ok(channel) => {
                let _ = self.event_tx.send(Event::VoiceChannelCreated {
                    crew_id: crew_id.to_string(),
                    channel,
                });
            }
            Err(e) => {
                log::error!("channel_create RPC failed: {}", e);
                let _ = self.event_tx.send(Event::Error {
                    message: format!("Failed to create voice channel: {}", e),
                });
            }
        }
    }

    async fn handle_rename_voice_channel(&self, crew_id: &str, channel_id: &str, name: &str) {
        match self.nakama.channel_rename(crew_id, channel_id, name).await {
            Ok(()) => {
                let _ = self.event_tx.send(Event::VoiceChannelRenamed {
                    crew_id: crew_id.to_string(),
                    channel_id: channel_id.to_string(),
                    name: name.to_string(),
                });
            }
            Err(e) => {
                log::error!("channel_rename RPC failed: {}", e);
                let _ = self.event_tx.send(Event::Error {
                    message: format!("Failed to rename voice channel: {}", e),
                });
            }
        }
    }

    async fn handle_delete_voice_channel(&self, crew_id: &str, channel_id: &str) {
        match self.nakama.channel_delete(crew_id, channel_id).await {
            Ok(()) => {
                let _ = self.event_tx.send(Event::VoiceChannelDeleted {
                    crew_id: crew_id.to_string(),
                    channel_id: channel_id.to_string(),
                });
            }
            Err(e) => {
                log::error!("channel_delete RPC failed: {}", e);
                let _ = self.event_tx.send(Event::Error {
                    message: format!("Failed to delete voice channel: {}", e),
                });
            }
        }
    }

    async fn handle_leave_crew(&mut self) {
        // Leave voice (local + server)
        if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
            if let Err(e) = self.nakama.voice_leave(&crew_id).await {
                log::warn!("voice_leave RPC on crew leave: {}", e);
            }
        }
        self.voice.leave_voice();
        let _ = self.event_tx.send(Event::VoiceStateChanged { in_call: false });
        let crew_id = self.nakama.active_crew_id().map(String::from);
        if let Err(e) = self.nakama.leave_crew_channel().await {
            log::error!("Failed to leave crew: {}", e);
        }
        if let Some(id) = crew_id {
            let _ = self.event_tx.send(Event::CrewLeft { crew_id: id });
        }
    }

    async fn handle_send_message(&self, content: &str) {
        if let Err(e) = self.nakama.send_chat_message(content).await {
            log::error!("Failed to send message: {}", e);
        }
    }

    // --- Streaming ---

    fn handle_list_capture_sources(&self) {
        let ctx = self.voice.mello_ctx();
        if ctx.is_null() {
            log::error!("Cannot enumerate capture sources: libmello not initialized");
            return;
        }

        let mut monitors = Vec::new();
        for i in 0..4u32 {
            monitors.push(crate::events::CaptureSource {
                id: format!("monitor-{}", i),
                name: format!("Display {}", i + 1),
                mode: "monitor".to_string(),
                monitor_index: Some(i),
                hwnd: None,
                pid: None,
                exe: String::new(),
                is_fullscreen: false,
                resolution: String::new(),
            });
        }

        let mut games_raw = vec![mello_sys::MelloGameProcess {
            pid: 0,
            name: [0i8; 128],
            exe: [0i8; 260],
            is_fullscreen: false,
        }; 32];
        let game_count = unsafe {
            mello_sys::mello_enumerate_games(ctx, games_raw.as_mut_ptr(), 32)
        };
        let mut games = Vec::new();
        for i in 0..game_count as usize {
            let name = unsafe { std::ffi::CStr::from_ptr(games_raw[i].name.as_ptr()) }
                .to_string_lossy()
                .to_string();
            let exe = unsafe { std::ffi::CStr::from_ptr(games_raw[i].exe.as_ptr()) }
                .to_string_lossy()
                .to_string();
            let pid = games_raw[i].pid;
            games.push(crate::events::CaptureSource {
                id: format!("game-{}", pid),
                name,
                mode: "process".to_string(),
                monitor_index: None,
                hwnd: None,
                pid: Some(pid),
                exe,
                is_fullscreen: games_raw[i].is_fullscreen,
                resolution: String::new(),
            });
        }

        let mut windows_raw = vec![mello_sys::MelloWindow {
            hwnd: std::ptr::null_mut(),
            title: [0i8; 256],
            pid: 0,
        }; 64];
        let win_count = unsafe {
            mello_sys::mello_enumerate_windows(ctx, windows_raw.as_mut_ptr(), 64)
        };
        let mut windows = Vec::new();
        for i in 0..win_count as usize {
            let title = unsafe { std::ffi::CStr::from_ptr(windows_raw[i].title.as_ptr()) }
                .to_string_lossy()
                .to_string();
            let hwnd = windows_raw[i].hwnd as u64;
            windows.push(crate::events::CaptureSource {
                id: format!("window-{}", hwnd),
                name: title,
                mode: "window".to_string(),
                monitor_index: None,
                hwnd: Some(hwnd),
                pid: Some(windows_raw[i].pid),
                exe: String::new(),
                is_fullscreen: false,
                resolution: String::new(),
            });
        }

        log::info!(
            "Enumerated capture sources: {} monitors, {} games, {} windows",
            monitors.len(), games.len(), windows.len()
        );
        let _ = self.event_tx.send(Event::CaptureSourcesListed {
            monitors,
            games,
            windows,
        });
    }

    async fn handle_start_stream(
        &mut self,
        crew_id: &str,
        _title: &str,
        capture_mode: &str,
        monitor_index: Option<u32>,
        hwnd: Option<u64>,
        pid: Option<u32>,
    ) {
        if self.stream_session.is_some() {
            let _ = self.event_tx.send(Event::StreamError {
                message: "Already streaming".to_string(),
            });
            return;
        }

        // Step 1: async RPC call (no raw pointers held across await)
        let resp = match crate::stream::host::request_start_stream(
            &self.nakama,
            crew_id,
            false, // supports_av1
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                log::error!("start_stream RPC failed: {}", e);
                let _ = self.event_tx.send(Event::StreamError {
                    message: e.to_string(),
                });
                return;
            }
        };

        // Step 2: sync FFI calls + session creation (raw pointers, no await)
        let config = crate::stream::StreamConfig::default();
        let ctx = self.voice.mello_ctx();

        if !crate::stream::encoder_available(ctx) {
            let msg = "Streaming requires a hardware encoder \
                       (NVIDIA, AMD, or Intel). None was found on this machine.";
            log::error!("{}", msg);
            let _ = self.event_tx.send(Event::StreamError {
                message: msg.to_string(),
            });
            return;
        }

        let mello_config = mello_sys::MelloStreamConfig {
            width: config.width,
            height: config.height,
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
        };

        let source = match capture_mode {
            "window" => mello_sys::MelloCaptureSource {
                mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_WINDOW,
                monitor_index: 0,
                hwnd: hwnd.unwrap_or(0) as *mut std::ffi::c_void,
                pid: 0,
            },
            "process" => mello_sys::MelloCaptureSource {
                mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_PROCESS,
                monitor_index: 0,
                hwnd: std::ptr::null_mut(),
                pid: pid.unwrap_or(0),
            },
            _ => mello_sys::MelloCaptureSource {
                mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_MONITOR,
                monitor_index: monitor_index.unwrap_or(0),
                hwnd: std::ptr::null_mut(),
                pid: 0,
            },
        };

        let (host, video_rx, audio_rx, resources) =
            match crate::stream::host::start_host(ctx, &source, &mello_config) {
                Ok(v) => v,
                Err(e) => {
                    let _ = self.event_tx.send(Event::StreamError {
                        message: e.to_string(),
                    });
                    return;
                }
            };

        // Start game-audio loopback capture (WASAPI)
        unsafe {
            mello_sys::mello_stream_start_audio(host);
        }

        let p2p_sink = Arc::new(P2PFanoutSink::new());
        let sink: Arc<dyn crate::stream::sink::PacketSink> = Arc::clone(&p2p_sink) as _;

        match crate::stream::host::create_stream_session(
            ctx, host, &resp, config, video_rx, audio_rx, resources, sink,
        ) {
            Ok(session) => {
                let _ = self.event_tx.send(Event::StreamStarted {
                    crew_id: crew_id.to_string(),
                    session_id: session.session_id.clone(),
                    mode: session.mode.clone(),
                });
                self.stream_sink = Some(p2p_sink);
                self.stream_session = Some(session);
            }
            Err(e) => {
                log::error!("Failed to create stream session: {}", e);
                unsafe {
                    mello_sys::mello_stream_stop_audio(host);
                    mello_sys::mello_stream_stop_host(host);
                }
                let _ = self.event_tx.send(Event::StreamError {
                    message: e.to_string(),
                });
            }
        }
    }

    async fn handle_stop_stream(&mut self) {
        if let Some(mut session) = self.stream_session.take() {
            session.stop();

            // Destroy all host-side stream peers
            for (id, hp) in self.stream_host_peers.drain() {
                unsafe {
                    mello_sys::mello_peer_destroy(hp.peer);
                    if !hp.ice_cb_data.is_null() {
                        drop(Box::from_raw(hp.ice_cb_data));
                    }
                }
                log::info!("Destroyed stream host peer {}", id);
            }
            self.stream_sink = None;

            if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
                let payload = serde_json::json!({ "crew_id": crew_id });
                if let Err(e) = self.nakama.rpc("stop_stream", &payload).await {
                    log::warn!("stop_stream RPC failed: {}", e);
                }
                let _ = self.event_tx.send(Event::StreamEnded { crew_id });
            }
        }
    }

    fn handle_watch_stream(&mut self, host_id: &str) {
        if self.viewer_state.is_some() {
            log::warn!("Already watching a stream, ignoring WatchStream");
            return;
        }

        log::info!("Starting stream viewer for host {}", host_id);
        let ctx = self.voice.mello_ctx();
        if ctx.is_null() {
            let _ = self.event_tx.send(Event::StreamError {
                message: "libmello context not initialized".to_string(),
            });
            return;
        }

        // Create peer connection for the host
        let peer_id_c = match CString::new(host_id) {
            Ok(c) => c,
            Err(_) => return,
        };
        let peer = unsafe { mello_sys::mello_peer_create(ctx, peer_id_c.as_ptr()) };
        if peer.is_null() {
            log::error!("Failed to create peer connection for stream viewer");
            let _ = self.event_tx.send(Event::StreamError {
                message: "Failed to create peer connection".to_string(),
            });
            return;
        }

        // Configure ICE servers
        let ice_cstrings: Vec<CString> = self.ice_servers.iter()
            .filter_map(|u| CString::new(u.as_str()).ok())
            .collect();
        if !ice_cstrings.is_empty() {
            let ptrs: Vec<*const std::os::raw::c_char> =
                ice_cstrings.iter().map(|s| s.as_ptr()).collect();
            unsafe {
                mello_sys::mello_peer_set_ice_servers(
                    peer,
                    ptrs.as_ptr() as *mut *const std::os::raw::c_char,
                    ptrs.len() as std::os::raw::c_int,
                );
            }
        }

        // ICE callback for stream signaling
        let ice_cb_data = Box::into_raw(Box::new(StreamIceCallbackData {
            peer_id: host_id.to_string(),
            purpose: SignalPurpose::Stream,
            queue: Arc::clone(&self.stream_signal_queue),
        }));
        unsafe {
            mello_sys::mello_peer_set_ice_callback(
                peer,
                Some(stream_ice_callback),
                ice_cb_data as *mut std::ffi::c_void,
            );
        }

        // Create offer and queue for sending
        let sdp_ptr = unsafe { mello_sys::mello_peer_create_offer(peer) };
        if sdp_ptr.is_null() {
            log::error!("Failed to create stream offer");
            unsafe {
                mello_sys::mello_peer_destroy(peer);
                drop(Box::from_raw(ice_cb_data));
            }
            let _ = self.event_tx.send(Event::StreamError {
                message: "Failed to create stream offer".to_string(),
            });
            return;
        }
        let sdp = unsafe { CStr::from_ptr(sdp_ptr) }.to_string_lossy().into_owned();
        log::info!("Created stream offer for host {}", host_id);

        if let Ok(mut queue) = self.stream_signal_queue.lock() {
            queue.push((
                host_id.to_string(),
                SignalEnvelope {
                    purpose: SignalPurpose::Stream,
                    message: SignalMessage::Offer { sdp },
                },
            ));
        }

        // Start C++ viewer pipeline with frame callback
        let config = crate::stream::StreamConfig::default();
        let mello_config = mello_sys::MelloStreamConfig {
            width: config.width,
            height: config.height,
            fps: config.fps,
            bitrate_kbps: 0,
        };

        let frame_cb_data = Box::into_raw(Box::new(FrameCallbackData {
            tx: self.event_tx.clone(),
        }));

        let viewer = unsafe {
            mello_sys::mello_stream_start_viewer(
                ctx,
                &mello_config,
                Some(on_viewer_frame),
                frame_cb_data as *mut std::ffi::c_void,
            )
        };
        if viewer.is_null() {
            log::error!("Failed to start stream viewer pipeline");
            unsafe {
                mello_sys::mello_peer_destroy(peer);
                drop(Box::from_raw(ice_cb_data));
                drop(Box::from_raw(frame_cb_data));
            }
            let _ = self.event_tx.send(Event::StreamError {
                message: "Failed to start video decoder".to_string(),
            });
            return;
        }

        let _ = self.event_tx.send(Event::StreamWatching {
            host_id: host_id.to_string(),
            width: config.width,
            height: config.height,
        });

        self.viewer_state = Some(ViewerState {
            viewer,
            peer,
            host_id: host_id.to_string(),
            _frame_cb_data: frame_cb_data,
            _ice_cb_data: ice_cb_data,
            got_keyframe: false,
            recv_buf: vec![0u8; VIEWER_RECV_BUF_SIZE],
            stream_viewer: StreamViewer::new(config.fec_n),
        });

        log::info!("Stream viewer initialized, waiting for WebRTC connection to {}", host_id);
    }

    fn handle_stop_watching(&mut self) {
        if let Some(vs) = self.viewer_state.take() {
            log::info!("Stopping stream viewer for host {}", vs.host_id);
            // ViewerState::Drop handles cleanup
            drop(vs);
            let _ = self.event_tx.send(Event::StreamWatchingStopped);
        }
    }
}
