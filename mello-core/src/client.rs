use tokio::sync::mpsc;

use crate::command::Command;
use crate::config::Config;
use crate::events::Event;
use crate::nakama::NakamaClient;
use crate::nakama::{InternalPresence, InternalSignal};
use crate::presence::PresenceStatus;
use crate::session;
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
        if !self.voice.is_active() {
            return;
        }

        let local_id = match self.nakama.current_user_id() {
            Some(id) => id.to_string(),
            None => return,
        };

        match presence {
            InternalPresence::Joined { user_id } => {
                if user_id != local_id {
                    log::info!(
                        "Presence: member {} joined channel, adding to voice mesh",
                        user_id
                    );
                    self.voice.on_member_joined(&local_id, &user_id);
                }
            }
            InternalPresence::Left { user_id } => {
                if user_id != local_id {
                    log::info!(
                        "Presence: member {} left channel, removing from voice mesh",
                        user_id
                    );
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
            Command::FinalizeOnboarding {
                crew_id,
                crew_name,
                display_name,
                avatar,
            } => {
                self.handle_finalize_onboarding(crew_id, crew_name, &display_name, avatar)
                    .await;
            }
            Command::LoadMyCrews => {
                self.load_crews().await;
            }
            Command::JoinCrew { crew_id } => {
                self.handle_join_crew(&crew_id).await;
            }
            Command::CreateCrew {
                name,
                description,
                open,
                avatar,
            } => {
                self.handle_create_crew(&name, &description, open, avatar.as_deref())
                    .await;
            }
            Command::FetchCrewAvatars { crew_ids } => {
                self.handle_fetch_crew_avatars(&crew_ids).await;
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
            Command::CheckMicPermission => {
                let status = unsafe { mello_sys::mello_mic_permission_status() };
                let granted = status == mello_sys::MelloMicPermission_MELLO_MIC_GRANTED;
                let denied = status == mello_sys::MelloMicPermission_MELLO_MIC_DENIED;
                let _ = self
                    .event_tx
                    .send(Event::MicPermissionChanged { granted, denied });
            }
            Command::RequestMicPermission => {
                let tx = self.event_tx.clone();
                unsafe extern "C" fn on_result(user_data: *mut std::ffi::c_void, granted: bool) {
                    let tx = Box::from_raw(user_data as *mut std::sync::mpsc::Sender<Event>);
                    let _ = tx.send(Event::MicPermissionChanged {
                        granted,
                        denied: !granted,
                    });
                }
                let tx_box = Box::new(tx);
                unsafe {
                    mello_sys::mello_mic_request_permission(
                        Some(on_result),
                        Box::into_raw(tx_box) as *mut std::ffi::c_void,
                    );
                }
            }
            Command::ListAudioDevices => {
                let capture = self.voice.list_capture_devices();
                let playback = self.voice.list_playback_devices();
                let _ = self
                    .event_tx
                    .send(Event::AudioDevicesListed { capture, playback });
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

            // --- Voice channels CRUD ---
            Command::CreateVoiceChannel { crew_id, name } => {
                self.handle_create_voice_channel(&crew_id, &name).await;
            }
            Command::RenameVoiceChannel {
                crew_id,
                channel_id,
                name,
            } => {
                self.handle_rename_voice_channel(&crew_id, &channel_id, &name)
                    .await;
            }
            Command::DeleteVoiceChannel {
                crew_id,
                channel_id,
            } => {
                self.handle_delete_voice_channel(&crew_id, &channel_id)
                    .await;
            }

            // --- Presence & crew state ---
            Command::UpdatePresence { status, activity } => {
                if let Err(e) = self
                    .nakama
                    .presence_update(&status, activity.as_ref())
                    .await
                {
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
                log::info!(
                    "Device auth succeeded for {} (created={})",
                    user.id,
                    created
                );
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
        match self.nakama.discover_crews_public(50).await {
            Ok(crews) => {
                let _ = self.event_tx.send(Event::DiscoverCrewsLoaded { crews });
            }
            Err(e) => {
                log::error!("Failed to discover crews: {}", e);
            }
        }
    }

    async fn handle_finalize_onboarding(
        &mut self,
        crew_id: Option<String>,
        crew_name: Option<String>,
        display_name: &str,
        _avatar: u8,
    ) {
        let device_id = {
            use rand::Rng;
            let bytes: [u8; 16] = rand::thread_rng().gen();
            bytes
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        };
        log::info!(
            "[onboarding] finalizing — device auth with id={}",
            device_id
        );

        let (user, _created) = match self.nakama.authenticate_device(&device_id).await {
            Ok(pair) => pair,
            Err(e) => {
                log::error!("[onboarding] device auth failed: {}", e);
                let _ = self.event_tx.send(Event::OnboardingFailed {
                    reason: format!("Account creation failed: {}", e),
                });
                return;
            }
        };

        if let Some(rt) = self.nakama.refresh_token() {
            let _ = session::save(rt);
        }

        if let Err(e) = self.nakama.connect_ws(self.event_tx.clone()).await {
            log::error!("[onboarding] WebSocket connect failed: {}", e);
            let _ = self.event_tx.send(Event::OnboardingFailed {
                reason: format!("Connection failed: {}", e),
            });
            return;
        }

        self.on_connected().await;

        if !display_name.is_empty() {
            if let Err(e) = self.nakama.update_account(display_name).await {
                log::warn!("[onboarding] failed to set display name: {}", e);
            }
        }

        // TODO: persist avatar in user metadata once supported

        let final_crew_id = if let Some(id) = crew_id {
            if let Err(e) = self.nakama.join_group(&id).await {
                log::error!("[onboarding] failed to join crew {}: {}", id, e);
                let _ = self.event_tx.send(Event::OnboardingFailed {
                    reason: format!("Failed to join crew: {}", e),
                });
                return;
            }
            Some(id)
        } else if let Some(name) = crew_name {
            match self.nakama.create_crew(&name, "", true, None).await {
                Ok(crew) => {
                    let id = crew.id.clone();
                    let _ = self.event_tx.send(Event::CrewCreated { crew });
                    Some(id)
                }
                Err(e) => {
                    log::error!("[onboarding] failed to create crew: {}", e);
                    let _ = self.event_tx.send(Event::OnboardingFailed {
                        reason: format!("Failed to create crew: {}", e),
                    });
                    return;
                }
            }
        } else {
            None
        };

        if let Some(ref cid) = final_crew_id {
            self.handle_select_crew(cid).await;
        }

        let mut updated_user = user;
        updated_user.display_name = display_name.to_string();
        let _ = self
            .event_tx
            .send(Event::OnboardingReady { user: updated_user });
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

        let id_token = match self.nakama.google_exchange_code(&code, &verifier).await {
            Ok(t) => t,
            Err(e) => {
                log::error!("[auth] Google token exchange failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: e.to_string(),
                });
                return;
            }
        };

        match self.nakama.authenticate_google(&id_token).await {
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
        log::info!(
            "[auth] Social login success: {} ({})",
            user.display_name,
            user.tag
        );

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

        let id_token = match self.nakama.google_exchange_code(&code, &verifier).await {
            Ok(t) => t,
            Err(e) => {
                log::error!("[auth] Google token exchange failed: {}", e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: e.to_string(),
                });
                return;
            }
        };

        match self.nakama.link_google(&id_token).await {
            Ok(()) => {
                log::info!("[auth] Google identity linked to device account");
                let _ = self.event_tx.send(Event::SocialLinked);
            }
            Err(e) if e.to_string().contains("already in use") => {
                log::info!("[auth] Google already linked elsewhere, falling back to authenticate");
                match self.nakama.authenticate_google(&id_token).await {
                    Ok(user) => self.on_social_login(user).await,
                    Err(e2) => {
                        log::error!("[auth] Google authenticate fallback failed: {}", e2);
                        let _ = self.event_tx.send(Event::SocialLinkFailed {
                            reason: e2.to_string(),
                        });
                    }
                }
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
            Err(e) if e.to_string().contains("already in use") => {
                log::info!("[auth] Discord already linked elsewhere, falling back to authenticate");
                match self.nakama.authenticate_custom(&token, "discord").await {
                    Ok(user) => self.on_social_login(user).await,
                    Err(e2) => {
                        log::error!("[auth] Discord authenticate fallback failed: {}", e2);
                        let _ = self.event_tx.send(Event::SocialLinkFailed {
                            reason: e2.to_string(),
                        });
                    }
                }
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
        if let Err(e) = self
            .nakama
            .presence_update(&PresenceStatus::Offline, None)
            .await
        {
            log::warn!("Failed to set offline presence on logout: {}", e);
        }

        // Leave voice (local + server-side)
        if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
            if let Err(e) = self.nakama.voice_leave(&crew_id).await {
                log::warn!("Failed to voice_leave RPC on logout: {}", e);
            }
        }
        self.voice.leave_voice();
        let _ = self
            .event_tx
            .send(Event::VoiceStateChanged { in_call: false });

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

    async fn handle_create_crew(
        &mut self,
        name: &str,
        description: &str,
        open: bool,
        avatar: Option<&str>,
    ) {
        match self
            .nakama
            .create_crew(name, description, open, avatar)
            .await
        {
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

    async fn handle_fetch_crew_avatars(&self, crew_ids: &[String]) {
        for crew_id in crew_ids {
            match self
                .nakama
                .read_storage(
                    "crew_avatars",
                    crew_id,
                    "00000000-0000-0000-0000-000000000000",
                )
                .await
            {
                Ok(data) if !data.is_empty() => {
                    let _ = self.event_tx.send(Event::CrewAvatarLoaded {
                        crew_id: crew_id.clone(),
                        data,
                    });
                }
                Ok(_) => {}
                Err(e) => {
                    log::debug!("Failed to fetch avatar for crew {}: {}", crew_id, e);
                }
            }
        }
    }

    async fn handle_select_crew(&mut self, crew_id: &str) {
        self.voice.leave_voice();
        let _ = self
            .event_tx
            .send(Event::VoiceStateChanged { in_call: false });

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
        let local_user_id = self
            .nakama
            .current_user_id()
            .map(String::from)
            .unwrap_or_default();
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
                    state
                        .voice_channels
                        .iter()
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
        if let Err(e) = self
            .nakama
            .presence_update(&PresenceStatus::Online, None)
            .await
        {
            log::warn!("Failed to set online presence: {}", e);
        }

        self.check_protocol_version().await;

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

    async fn check_protocol_version(&self) {
        match self.nakama.health_check().await {
            Ok(health) => {
                log::info!(
                    "Server health: status={} version={} protocol={}",
                    health.status,
                    health.version,
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
            let peer_ids: Vec<String> = resp
                .voice_state
                .members
                .iter()
                .filter(|m| m.user_id != local_id)
                .map(|m| m.user_id.clone())
                .collect();
            self.voice.join_voice(&local_id, &peer_ids);
            let _ = self
                .event_tx
                .send(Event::VoiceStateChanged { in_call: true });
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
        let _ = self
            .event_tx
            .send(Event::VoiceStateChanged { in_call: false });
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
        let _ = self
            .event_tx
            .send(Event::VoiceStateChanged { in_call: false });
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

        if !unsafe { crate::stream::encoder_available(ctx) } {
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

        // TODO: let the user pick a capture source; default to primary monitor for now
        let source = mello_sys::MelloCaptureSource {
            mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_MONITOR,
            monitor_index: 0,
            hwnd: std::ptr::null_mut(),
            pid: 0,
        };

        let (host, video_rx, audio_rx, resources) =
            match unsafe { crate::stream::host::start_host(ctx, &source, &mello_config) } {
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

        match crate::stream::host::create_stream_session(
            ctx, host, &resp, config, video_rx, audio_rx, resources,
        ) {
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
