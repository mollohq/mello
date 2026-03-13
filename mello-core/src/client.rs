use tokio::sync::mpsc;

use crate::command::Command;
use crate::config::Config;
use crate::events::Event;
use crate::nakama::NakamaClient;
use crate::presence::PresenceStatus;
use crate::session;
use crate::nakama::{InternalPresence, InternalSignal};
use crate::stream::manager::StreamSession;
use crate::voice::{SignalMessage, VoiceManager};

pub struct Client {
    nakama: NakamaClient,
    voice: VoiceManager,
    event_tx: std::sync::mpsc::Sender<Event>,
    stream_session: Option<StreamSession>,
}

impl Client {
    pub fn new(config: Config, event_tx: std::sync::mpsc::Sender<Event>, loopback: bool) -> Self {
        Self {
            nakama: NakamaClient::new(config),
            voice: VoiceManager::new(event_tx.clone(), loopback),
            event_tx,
            stream_session: None,
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
        match serde_json::from_str::<SignalMessage>(&signal.payload) {
            Ok(msg) => {
                log::info!("Received signal from {}: {:?}", signal.from, msg);
                self.voice.handle_signal(&signal.from, msg);
            }
            Err(e) => {
                log::warn!("Failed to parse signal from {}: {}", signal.from, e);
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
            let payload = match serde_json::to_string(&signal) {
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
            Command::JoinVoice => {
                self.handle_join_voice().await;
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
            Command::StartStream { crew_id, title } => {
                self.handle_start_stream(&crew_id, &title).await;
            }
            Command::StopStream => {
                self.handle_stop_stream().await;
            }
            Command::WatchStream { host_id } => {
                log::info!("WatchStream requested for host {}", host_id);
                // TODO: viewer-side peer connection + stream viewing
            }
            Command::StopWatching => {
                log::info!("StopWatching requested");
                // TODO: tear down viewer-side stream
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
        match self.nakama.set_active_crew(crew_id).await {
            Ok(state) => {
                let _ = self.event_tx.send(Event::CrewStateLoaded { state });
            }
            Err(e) => {
                log::warn!("set_active_crew RPC failed: {}", e);
            }
        }

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

            // Auto-join voice with online members only
            if let Some(local_id) = self.nakama.current_user_id().map(String::from) {
                let other_ids: Vec<String> = members.iter()
                    .filter(|m| m.online && m.id != local_id)
                    .map(|m| m.id.clone())
                    .collect();
                self.voice.join_voice(&local_id, &other_ids);
                let _ = self.event_tx.send(Event::VoiceStateChanged { in_call: true });

                // Notify server we joined voice
                if let Err(e) = self.nakama.voice_join(crew_id).await {
                    log::warn!("voice_join RPC failed: {}", e);
                }
            }
        }
    }

    /// Called after successful auth + WS connect. Sets online presence and fetches ICE config.
    async fn on_connected(&mut self) {
        if let Err(e) = self.nakama.presence_update(&PresenceStatus::Online, None).await {
            log::warn!("Failed to set online presence: {}", e);
        }

        match self.nakama.get_ice_servers().await {
            Ok(urls) => {
                log::info!("Fetched {} ICE server(s) from backend", urls.len());
                self.voice.set_ice_servers(urls);
            }
            Err(e) => {
                log::warn!("Failed to fetch ICE servers, using defaults: {}", e);
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

    async fn handle_join_voice(&mut self) {
        if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
            if let Err(e) = self.nakama.voice_join(&crew_id).await {
                log::error!("voice_join RPC failed: {}", e);
            }
        }
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

    async fn handle_start_stream(&mut self, crew_id: &str, _title: &str) {
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
        let mello_config = mello_sys::MelloStreamConfig {
            width: config.width,
            height: config.height,
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
            encoder: mello_sys::MelloEncoderType_MELLO_ENCODER_AUTO,
        };

        let host = unsafe { mello_sys::mello_stream_start_host(ctx, &mello_config) };
        if host.is_null() {
            let _ = self.event_tx.send(Event::StreamError {
                message: "Failed to start stream host (libmello)".to_string(),
            });
            return;
        }

        match crate::stream::host::create_stream_session(ctx, host, &resp, config) {
            Ok(session) => {
                let _ = self.event_tx.send(Event::StreamStarted {
                    crew_id: crew_id.to_string(),
                    session_id: session.session_id.clone(),
                    mode: session.mode.clone(),
                });
                self.stream_session = Some(session);
            }
            Err(e) => {
                log::error!("Failed to create stream session: {}", e);
                unsafe {
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

            if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
                let payload = serde_json::json!({ "crew_id": crew_id });
                if let Err(e) = self.nakama.rpc("stop_stream", &payload).await {
                    log::warn!("stop_stream RPC failed: {}", e);
                }
                let _ = self.event_tx.send(Event::StreamEnded { crew_id });
            }
        }
    }
}
