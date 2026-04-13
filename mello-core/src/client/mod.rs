mod auth;
mod chat;
mod clip;
mod connection;
mod crew;
mod presence;
mod stream_ffi;
mod streaming;
mod voice;

use tokio::sync::mpsc;

use crate::command::Command;
use crate::config::Config;
use crate::events::Event;
use crate::game_db::GameDatabase;
use crate::game_sensing::GameSensor;
use crate::game_state::GameStateManager;
use crate::giphy::GiphyClient;
use crate::nakama::NakamaClient;
use crate::nakama::{InternalPresence, InternalSignal};
use crate::stream::manager::StreamSession;
use crate::stream::sink_p2p::P2PFanoutSink;
use crate::voice::{SignalEnvelope, SignalMessage, SignalPurpose, VoiceManager};

use std::collections::HashMap;
use std::sync::Arc;

use stream_ffi::{StreamHostPeer, ViewerState};

/// Individual chunks are at most ~60KB + 6 byte header.
const VIEWER_RECV_BUF_SIZE: usize = 64 * 1024;

/// Shared single-slot buffer for decoded stream frames. The C++ callback
/// overwrites the latest frame; the UI timer reads and takes it. This avoids
/// unbounded queue buildup that occurs when sending ~11 MB frames through a
/// channel at 30+ fps.
pub type FrameSlot = Arc<std::sync::Mutex<Option<(u32, u32, Vec<u8>)>>>;

pub struct Client {
    nakama: NakamaClient,
    voice: VoiceManager,
    event_tx: std::sync::mpsc::Sender<Event>,
    frame_slot: FrameSlot,
    frame_consumed: Arc<std::sync::atomic::AtomicBool>,
    stream_session: Option<StreamSession>,
    stream_sink: Option<Arc<P2PFanoutSink>>,
    stream_host_peers: HashMap<String, StreamHostPeer>,
    viewer_state: Option<ViewerState>,
    stream_signal_queue: Arc<std::sync::Mutex<Vec<(String, SignalEnvelope)>>>,
    /// ICE candidates received before the peer was created (host side).
    pending_remote_ice: HashMap<String, Vec<SignalMessage>>,
    ice_servers: Vec<String>,
    /// Actual encode resolution (set after host pipeline starts).
    stream_encode_width: u32,
    stream_encode_height: u32,
    /// Stop signal for the thumbnail refresh thread.
    thumbnail_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Cached list of windows for thumbnail refresh.
    cached_windows: Vec<(String, u64)>,
    history_cursor: Option<String>,
    giphy: GiphyClient,
    /// Pending SFU voice reconnect: (when, channel_id, attempt)
    sfu_voice_reconnect: Option<(tokio::time::Instant, String, u32)>,
    /// Last voice channel we joined (for reconnection)
    last_voice_channel: Option<String>,
    game_state: GameStateManager,
    #[allow(dead_code)]
    game_sensor: Option<GameSensor>,
    clip_was_playing: bool,
    clip_tick_counter: u8,
}

impl Client {
    pub fn new(
        config: Config,
        event_tx: std::sync::mpsc::Sender<Event>,
        loopback: bool,
        frame_slot: FrameSlot,
        frame_consumed: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            nakama: NakamaClient::new(config),
            voice: VoiceManager::new(event_tx.clone(), loopback),
            event_tx,
            frame_slot,
            frame_consumed,
            stream_session: None,
            stream_sink: None,
            stream_host_peers: HashMap::new(),
            viewer_state: None,
            stream_signal_queue: Arc::new(std::sync::Mutex::new(Vec::new())),
            stream_encode_width: 0,
            stream_encode_height: 0,
            pending_remote_ice: HashMap::new(),
            ice_servers: Vec::new(),
            thumbnail_stop: None,
            cached_windows: Vec::new(),
            history_cursor: None,
            giphy: GiphyClient::new(),
            sfu_voice_reconnect: None,
            last_voice_channel: None,
            game_state: GameStateManager::new(),
            game_sensor: None,
            clip_was_playing: false,
            clip_tick_counter: 0,
        }
    }

    pub async fn run(&mut self, mut cmd_rx: mpsc::Receiver<Command>) {
        log::info!("Mello client started, waiting for commands...");

        // --- Game sensing ---
        let game_db = GameDatabase::load_bundled();
        let mello_ctx = self.voice.mello_ctx();
        let (sensor, game_event_rx) = GameSensor::start(mello_ctx, game_db);
        self.game_sensor = Some(sensor);
        log::info!("Game sensor started");

        let mut signal_rx = self.nakama.take_signal_rx().unwrap();
        let mut presence_rx = self.nakama.take_presence_rx().unwrap();
        let mut voice_tick = tokio::time::interval(tokio::time::Duration::from_millis(20));
        // Refresh access token every 45 minutes (token lives 1 hour)
        let mut refresh_tick = tokio::time::interval(tokio::time::Duration::from_secs(45 * 60));
        refresh_tick.tick().await; // consume the immediate first tick

        loop {
            // Drain game events (non-blocking) before entering select!
            while let Ok(game_event) = game_event_rx.try_recv() {
                let (ui_events, session_end) = self.game_state.handle_event(game_event);
                for ev in ui_events {
                    let _ = self.event_tx.send(ev);
                }
                if let Some(info) = session_end {
                    if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
                        self.handle_game_session_end(&crew_id, &info.game_name, info.duration_min)
                            .await;
                    }
                }
            }

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
                    self.clip_playback_tick();
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
        match serde_json::from_str::<SignalEnvelope>(&signal.payload) {
            Ok(env) => match env.purpose {
                SignalPurpose::Voice => {
                    log::info!("Voice signal from {}: {:?}", signal.from, env.message);
                    self.voice.handle_signal(&signal.from, env.message);
                }
                SignalPurpose::Stream => {
                    log::info!("Stream signal from {}: {:?}", signal.from, env.message);
                    self.handle_stream_signal(&signal.from, env);
                }
            },
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
            Command::DiscoverCrews { cursor } => {
                self.handle_discover_crews(cursor.as_deref()).await;
            }
            Command::FinalizeOnboarding {
                crew_id,
                crew_name,
                crew_description,
                crew_open,
                crew_avatar,
                display_name,
                avatar_data,
                avatar_format,
                avatar_style,
                avatar_seed,
            } => {
                self.handle_finalize_onboarding(
                    crew_id,
                    crew_name,
                    crew_description,
                    crew_open,
                    crew_avatar,
                    &display_name,
                    avatar_data,
                    avatar_format,
                    avatar_style,
                    avatar_seed,
                )
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
                invite_user_ids,
            } => {
                self.handle_create_crew(
                    &name,
                    &description,
                    open,
                    avatar.as_deref(),
                    &invite_user_ids,
                )
                .await;
            }
            Command::FetchCrewAvatars { crew_ids } => {
                self.handle_fetch_crew_avatars(&crew_ids).await;
            }
            Command::FetchUserAvatar { user_id } => {
                self.handle_fetch_user_avatar(&user_id).await;
            }
            Command::FetchUserAvatars { user_ids } => {
                self.handle_fetch_user_avatars(&user_ids).await;
            }
            Command::SearchUsers { query } => {
                self.handle_search_users(&query).await;
            }
            Command::JoinByInviteCode { code } => {
                self.handle_join_by_invite_code(&code).await;
            }
            Command::SelectCrew { crew_id } => {
                self.handle_select_crew(&crew_id).await;
            }
            Command::LeaveCrew => {
                self.handle_leave_crew().await;
            }
            Command::SendMessage { content, reply_to } => {
                self.handle_send_message(&content, reply_to.as_deref())
                    .await;
            }
            Command::SendGif { gif, body } => {
                self.handle_send_gif(gif, &body).await;
            }
            Command::EditMessage {
                message_id,
                new_body,
            } => {
                self.handle_edit_message(&message_id, &new_body).await;
            }
            Command::DeleteMessage { message_id } => {
                self.handle_delete_message(&message_id).await;
            }
            Command::LoadHistory { cursor } => {
                self.handle_load_history(cursor.as_deref()).await;
            }
            Command::SearchGifs { query } => {
                self.handle_search_gifs(&query).await;
            }
            Command::LoadTrendingGifs => {
                self.handle_trending_gifs().await;
            }
            Command::JoinVoice { channel_id } => {
                self.handle_join_voice(&channel_id).await;
            }
            Command::LeaveVoice => {
                self.handle_leave_voice().await;
            }
            Command::VoiceSpeaking { speaking } => {
                if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
                    log::debug!("voice_speaking RPC: crew={} speaking={}", crew_id, speaking);
                    if let Err(e) = self.nakama.voice_speaking(&crew_id, speaking).await {
                        log::warn!("voice_speaking RPC failed: {}", e);
                    }
                } else {
                    log::debug!("voice_speaking: no active crew");
                }
            }
            Command::SetMute { muted } => {
                self.voice.set_mute(muted);
            }
            Command::SetDeafen { deafened } => {
                self.voice.set_deafen(deafened);
            }
            Command::BroadcastMuteState { muted, deafened } => {
                if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
                    if let Err(e) = self
                        .nakama
                        .voice_mute_state(&crew_id, muted, deafened)
                        .await
                    {
                        log::debug!("voice_mute_state RPC failed: {}", e);
                    }
                }
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
            Command::SetEchoCancellation { enabled } => {
                self.voice.set_echo_cancellation(enabled);
            }
            Command::SetAgc { enabled } => {
                self.voice.set_agc(enabled);
            }
            Command::SetNoiseSuppression { enabled } => {
                self.voice.set_noise_suppression(enabled);
            }
            Command::SetInputVolume { volume } => {
                self.voice.set_input_volume(volume);
            }
            Command::SetOutputVolume { volume } => {
                self.voice.set_output_volume(volume);
            }
            Command::SetLoopback { enabled } => {
                self.voice.set_loopback(enabled);
            }
            Command::SetDebugMode { enabled } => {
                self.voice.set_debug_mode(enabled);
            }
            Command::UpdateProfile {
                display_name,
                avatar_data,
                avatar_format,
                avatar_style,
                avatar_seed,
            } => {
                self.handle_update_profile(
                    &display_name,
                    avatar_data.as_deref(),
                    avatar_format.as_deref(),
                    avatar_style.as_deref(),
                    avatar_seed.as_deref(),
                )
                .await;
            }
            // --- Streaming ---
            Command::ListCaptureSources => {
                self.handle_list_capture_sources();
            }
            Command::StartThumbnailRefresh => {
                self.start_thumbnail_refresh();
            }
            Command::StopThumbnailRefresh => {
                self.stop_thumbnail_refresh();
            }
            Command::StartStream {
                crew_id,
                title,
                capture_mode,
                monitor_index,
                hwnd,
                pid,
                preset,
            } => {
                self.handle_start_stream(
                    &crew_id,
                    &title,
                    &capture_mode,
                    monitor_index,
                    hwnd,
                    pid,
                    preset,
                )
                .await;
            }
            Command::StopStream => {
                self.handle_stop_stream().await;
            }
            Command::WatchStream {
                host_id,
                session_id,
                width,
                height,
            } => {
                self.handle_watch_stream(&host_id, &session_id, width, height)
                    .await;
            }
            Command::StopWatching => {
                self.handle_stop_watching().await;
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

            // --- Clips ---
            Command::StartClipBuffer => {
                self.handle_start_clip_buffer();
            }
            Command::StopClipBuffer => {
                self.handle_stop_clip_buffer();
            }
            Command::CaptureClip { seconds } => {
                self.handle_capture_clip(seconds);
            }
            Command::PostClip {
                crew_id,
                clip_id,
                duration_seconds,
                local_path,
            } => {
                self.handle_post_clip(&crew_id, &clip_id, duration_seconds, &local_path)
                    .await;
            }
            Command::UploadClip {
                crew_id,
                clip_id,
                wav_path,
            } => {
                self.handle_upload_clip(&crew_id, &clip_id, &wav_path).await;
            }
            Command::PlayClip { path } => {
                self.handle_play_clip(&path).await;
            }
            Command::PauseClip => {
                self.handle_pause_clip();
            }
            Command::ResumeClip => {
                self.handle_resume_clip();
            }
            Command::SeekClip { position_ms } => {
                self.handle_seek_clip(position_ms);
            }
            Command::StopClipPlayback => {
                self.handle_stop_clip_playback();
            }
            Command::LoadCrewTimeline { crew_id, cursor } => {
                self.handle_load_crew_timeline(&crew_id, cursor.as_deref())
                    .await;
            }

            // --- Crew events ---
            Command::CrewCatchup { crew_id, last_seen } => {
                self.handle_crew_catchup(&crew_id, last_seen).await;
            }
            Command::PostMoment {
                crew_id,
                sentiment,
                text,
                game_name,
            } => {
                self.handle_post_moment(&crew_id, &sentiment, &text, &game_name)
                    .await;
            }
            Command::GameSessionEnd {
                crew_id,
                game_name,
                duration_min,
            } => {
                self.handle_game_session_end(&crew_id, &game_name, duration_min)
                    .await;
            }
        }
    }
}
