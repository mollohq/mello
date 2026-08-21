mod auth;
mod chat;
mod clip;
mod connection;
mod crew;
mod diagnostics;
mod game_services;
mod presence;
mod reconnect;
mod stats_emit;
mod stream_ffi;
mod streaming;
mod tick_gating;
mod voice;
pub mod waveform;

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
use crate::stream::pacer::PacingTelemetry;
use crate::stream::sink::PacketSink;
use crate::stream::sink_p2p::P2PFanoutSink;
use crate::telemetry::{AdapterRegistry, TelemetryListener, TELEMETRY_PORT};
use crate::transport::SfuConnection;
use crate::voice::{SignalEnvelope, SignalMessage, SignalPurpose, VoiceManager};

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

pub use stream_ffi::feed_viewer_audio_packet;
use stream_ffi::{StreamHostPeer, StreamPeerDisconnect, ViewerState};

/// Shared single-slot buffer for decoded stream frames. The C++ callback
/// overwrites the latest frame; the UI timer reads and takes it. This avoids
/// unbounded queue buildup that occurs when sending ~11 MB frames through a
/// channel at 30+ fps.
pub type FrameSlot = Arc<std::sync::Mutex<Option<(u32, u32, Vec<u8>)>>>;
/// Typed metadata for a native GPU frame surface shared from libmello.
///
/// This is the canonical frame-surface contract at the Rust/UI boundary.
/// It intentionally carries only descriptor metadata; texture ownership stays
/// in native code and consumers import the shared handle on demand.
#[derive(Debug, Clone, Copy)]
pub struct NativeSurfaceFrame {
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub shared_handle: usize,
    pub format: u32,
    pub uv_y_offset: u32,
    pub timestamp: u64,
}

/// Shared single-slot for latest native GPU frame surface descriptor.
pub type NativeFrameSlot = Arc<std::sync::Mutex<Option<NativeSurfaceFrame>>>;
/// Shared lifecycle state for viewer frame ownership handoff.
pub type FrameLifecycleSlot = Arc<std::sync::atomic::AtomicU8>;

pub const FRAME_STATE_IDLE: u8 = 0;
pub const FRAME_STATE_READY: u8 = 1;
pub const FRAME_STATE_LATCHED: u8 = 2;
pub const FRAME_STATE_PRESENTED: u8 = 3;

pub struct Client {
    nakama: NakamaClient,
    voice: VoiceManager,
    event_tx: std::sync::mpsc::Sender<Event>,
    frame_slot: FrameSlot,
    native_frame_slot: NativeFrameSlot,
    frame_consumed: Arc<std::sync::atomic::AtomicBool>,
    frame_lifecycle: FrameLifecycleSlot,
    surface_frame_seq: Arc<std::sync::atomic::AtomicU64>,
    stream_session: Option<StreamSession>,
    stream_host_sink: Option<Arc<dyn PacketSink>>,
    stream_sfu_connection: Option<Arc<SfuConnection>>,
    stream_sink: Option<Arc<P2PFanoutSink>>,
    stream_host_peers: HashMap<String, StreamHostPeer>,
    viewer_state: Option<ViewerState>,
    stream_signal_queue: Arc<std::sync::Mutex<Vec<(String, SignalEnvelope)>>>,
    /// Terminal P2P stream peer callbacks queued for main-loop cleanup.
    stream_disconnect_queue: Arc<std::sync::Mutex<VecDeque<StreamPeerDisconnect>>>,
    /// ICE candidates received before the peer was created (host side).
    pending_remote_ice: HashMap<String, Vec<SignalMessage>>,
    ice_servers: Vec<String>,
    /// Actual encode resolution (set after host pipeline starts).
    stream_encode_width: u32,
    stream_encode_height: u32,
    stream_bitrate_kbps: u32,
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
    /// Shared with the sensor thread so user-confirmed custom games apply
    /// live without a restart.
    game_db: Arc<std::sync::RwLock<GameDatabase>>,
    /// User-confirmed games outside the bundled DB (settings-persisted client
    /// side; the DB overlay is rebuilt from this list on every change).
    custom_games: Vec<crate::game_db::CustomGame>,
    /// Keeps the telemetry listener thread alive for the client's lifetime.
    #[allow(dead_code)]
    telemetry_listener: Option<TelemetryListener>,
    telemetry_registry: Arc<AdapterRegistry>,
    /// Game integrations the user switched off (Games settings page). Disabled
    /// ids skip config installs and active transports.
    disabled_integrations: std::collections::HashSet<String>,
    enable_game_sensor: bool,
    emit_process_stats: bool,
    game_event_rx:
        std::sync::Mutex<Option<std::sync::mpsc::Receiver<crate::game_sensing::GameEvent>>>,
    /// Shared telemetry channel: the loopback listener and active-source
    /// adapters all send into this. Created at construction so the deferred
    /// listener (post-auth, see `game_services.rs`) and the game-start
    /// `adapter.start()` calls share one sender.
    telemetry_event_tx: std::sync::mpsc::Sender<crate::telemetry::TelemetryEvent>,
    telemetry_event_rx:
        std::sync::Mutex<Option<std::sync::mpsc::Receiver<crate::telemetry::TelemetryEvent>>>,
    /// Token for adapter config installs; loaded post-auth in `ensure_game_services`.
    telemetry_token: Option<String>,
    clip_was_playing: bool,
    clip_tick_counter: u8,
    host_pacing_last: Option<PacingTelemetry>,
    host_pacing_last_at: Instant,
    /// Stream-tick counter driving the ~2s control-channel ping on the SFU
    /// host connection (125 ticks at the 16ms stream tick).
    host_sfu_ping_ticks: u64,
    /// Realtime WS reconnect/liveness state machine (backoff, edge detection,
    /// sleep/wake gap, heartbeat cadence). Pure decision logic, unit-tested in
    /// `reconnect.rs`; `connection_tick` is its IO adapter.
    reconnect: reconnect::ReconnectSupervisor,
}

impl Client {
    pub fn new(
        config: Config,
        event_tx: std::sync::mpsc::Sender<Event>,
        loopback: bool,
        frame_slot: FrameSlot,
        native_frame_slot: NativeFrameSlot,
        frame_consumed: Arc<std::sync::atomic::AtomicBool>,
        frame_lifecycle: FrameLifecycleSlot,
    ) -> Self {
        Self::new_with_game_sensor(
            config,
            event_tx,
            loopback,
            frame_slot,
            native_frame_slot,
            frame_consumed,
            frame_lifecycle,
            true,
            // emit_process_stats: 1 Hz MelloStats for the debug panel (spec 15)
            // and the perf harness. Cheap (one proc_pid_rusage/s).
            true,
        )
    }

    /// Sequence counter for native surface descriptors produced by callbacks.
    pub fn surface_frame_seq(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.surface_frame_seq.clone()
    }

    /// Seed the disabled-integration set from persisted client settings.
    /// Must be called before `run()` so the startup config installs honor it.
    pub fn set_disabled_integrations(&mut self, ids: impl IntoIterator<Item = String>) {
        self.disabled_integrations = ids.into_iter().collect();
    }

    /// Seed user-confirmed custom games from persisted client settings.
    /// Must be called before `run()` so the sensor recognizes them from the
    /// first scan.
    pub fn set_custom_games(&mut self, games: Vec<crate::game_db::CustomGame>) {
        self.custom_games = games;
        self.rebuild_game_db();
    }

    /// Rebuild the shared DB as bundled + custom overlay. Cheap (25 bundled
    /// entries); runs only on seed/confirm, never in the scan loop.
    fn rebuild_game_db(&self) {
        let mut db = GameDatabase::load_bundled();
        db.add_user_entries(&self.custom_games);
        *self.game_db.write().expect("game db lock poisoned") = db;
    }

    /// Shared frame lifecycle state used by stream_tick() and UI compositor.
    pub fn frame_lifecycle_slot(&self) -> FrameLifecycleSlot {
        self.frame_lifecycle.clone()
    }

    /// Construct a client and optionally disable game sensing. Voice-only tools
    /// should turn this off to avoid unrelated process scanning overhead.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_game_sensor(
        config: Config,
        event_tx: std::sync::mpsc::Sender<Event>,
        loopback: bool,
        frame_slot: FrameSlot,
        native_frame_slot: NativeFrameSlot,
        frame_consumed: Arc<std::sync::atomic::AtomicBool>,
        frame_lifecycle: FrameLifecycleSlot,
        enable_game_sensor: bool,
        emit_process_stats: bool,
    ) -> Self {
        let (telemetry_event_tx, telemetry_event_rx) =
            std::sync::mpsc::channel::<crate::telemetry::TelemetryEvent>();
        Self {
            nakama: NakamaClient::new(config),
            voice: VoiceManager::new(event_tx.clone(), loopback),
            event_tx,
            frame_slot,
            native_frame_slot,
            frame_consumed,
            frame_lifecycle,
            surface_frame_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            stream_session: None,
            stream_host_sink: None,
            stream_sfu_connection: None,
            stream_sink: None,
            stream_host_peers: HashMap::new(),
            viewer_state: None,
            stream_signal_queue: Arc::new(std::sync::Mutex::new(Vec::new())),
            stream_disconnect_queue: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            stream_encode_width: 0,
            stream_encode_height: 0,
            stream_bitrate_kbps: 0,
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
            game_db: Arc::new(std::sync::RwLock::new(GameDatabase::load_bundled())),
            custom_games: Vec::new(),
            telemetry_listener: None,
            telemetry_registry: Arc::new(AdapterRegistry::with_defaults()),
            disabled_integrations: std::collections::HashSet::new(),
            enable_game_sensor,
            emit_process_stats,
            game_event_rx: std::sync::Mutex::new(None),
            telemetry_event_tx,
            telemetry_event_rx: std::sync::Mutex::new(Some(telemetry_event_rx)),
            telemetry_token: None,
            clip_was_playing: false,
            clip_tick_counter: 0,
            host_pacing_last: None,
            host_pacing_last_at: Instant::now(),
            host_sfu_ping_ticks: 0,
            reconnect: reconnect::ReconnectSupervisor::new(),
        }
    }

    pub async fn run(&mut self, mut cmd_rx: mpsc::UnboundedReceiver<Command>) {
        log::info!("Mello client started, waiting for commands...");
        // Game sensing + telemetry (listener, adapter config installs) are
        // deferred to `ensure_game_services()` on post-auth connect — see
        // `game_services.rs`. Keeps startup lean; `disabled_integrations` is
        // seeded from settings before `run()`, so the deferred installs honor it.
        if self.enable_game_sensor {
            log::info!("Game sensor deferred until post-auth connect");
        } else {
            log::info!("Game sensor disabled");
        }

        let mut signal_rx = self.nakama.take_signal_rx().unwrap();
        let mut presence_rx = self.nakama.take_presence_rx().unwrap();
        let mut voice_tick = tokio::time::interval(tokio::time::Duration::from_millis(20));
        voice_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut stream_tick = tokio::time::interval(tokio::time::Duration::from_millis(16));
        stream_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Refresh access token every 45 minutes (token lives 1 hour)
        let mut refresh_tick = tokio::time::interval(tokio::time::Duration::from_secs(45 * 60));
        refresh_tick.tick().await; // consume the immediate first tick
                                   // Supervise the realtime WS: detect drops/half-open/sleep-wake and reconnect.
        let mut connection_tick = tokio::time::interval(tokio::time::Duration::from_secs(3));
        connection_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut stats_tick = tokio::time::interval(tokio::time::Duration::from_secs(1));
        stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        stats_tick.tick().await;

        loop {
            for game_event in self.drain_game_events() {
                // Unknown-game candidates go straight to the UI for the
                // "track it?" confirm prompt; they never touch game state,
                // presence, or the backend unless the user confirms.
                if let crate::game_sensing::GameEvent::UnknownCandidate {
                    exe,
                    path,
                    window_title,
                } = game_event
                {
                    let _ = self.event_tx.send(Event::UnknownGameCandidate {
                        exe,
                        path,
                        window_title,
                    });
                    continue;
                }

                // Telemetry adapter side-effects on game start/stop: install the
                // game's config on first detection, and reset adapter state on exit.
                match &game_event {
                    crate::game_sensing::GameEvent::Started(game) => {
                        if self.disabled_integrations.contains(&game.game_id) {
                            log::info!(
                                "[telemetry] integration for {} disabled by user, skipping",
                                game.game_id
                            );
                        } else if let Some(adapter) = self.telemetry_registry.get(&game.game_id) {
                            // Active sources start their transport now; pure
                            // push adapters no-op.
                            adapter.start(self.telemetry_event_tx.clone());
                            let token = self.telemetry_token.clone().unwrap_or_default();
                            tokio::task::spawn_blocking(move || {
                                if let Err(e) = adapter.ensure_installed(&token, TELEMETRY_PORT) {
                                    log::warn!("[telemetry] ensure_installed failed: {e}");
                                }
                            });
                        }
                    }
                    crate::game_sensing::GameEvent::Stopped(game) => {
                        if let Some(adapter) = self.telemetry_registry.get(&game.game_id) {
                            adapter.reset();
                            // reset() may flush a final result (file-based
                            // adapters); fold everything pending into the
                            // session before the stop is processed below.
                            for tev in self.drain_telemetry_events() {
                                for ev in self.game_state.handle_telemetry(tev) {
                                    let _ = self.event_tx.send(ev);
                                }
                            }
                        }
                    }
                    // Forwarded to the UI above; unreachable here.
                    crate::game_sensing::GameEvent::UnknownCandidate { .. } => {}
                }

                let (ui_events, session_end) = self.game_state.handle_event(game_event);
                for ev in ui_events {
                    let _ = self.event_tx.send(ev);
                }
                if let Some(summary) = session_end {
                    if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
                        self.handle_game_session_end(
                            &crew_id,
                            &summary.game_name,
                            &summary.game_id,
                            summary.duration_min,
                            summary.wins,
                            summary.losses,
                            summary.draws,
                        )
                        .await;
                    }
                }
            }

            for tev in self.drain_telemetry_events() {
                for ev in self.game_state.handle_telemetry(tev) {
                    let _ = self.event_tx.send(ev);
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
                _ = voice_tick.tick(), if self.needs_voice_tick() => {
                    self.voice_tick().await;
                    if self.clip_was_playing {
                        self.clip_playback_tick();
                    }
                }
                _ = stream_tick.tick(), if self.needs_stream_tick() => {
                    self.stream_tick().await;
                }
                _ = refresh_tick.tick() => {
                    self.refresh_token().await;
                }
                _ = connection_tick.tick() => {
                    self.connection_tick().await;
                }
                _ = stats_tick.tick(), if self.emit_process_stats => {
                    self.emit_stats_tick();
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
            Command::DeleteAccount => {
                self.handle_delete_account().await;
            }

            // Social auth
            Command::AuthSteam => {
                log::info!("[auth] Steam auth requested");
                self.handle_auth_steam().await;
            }
            Command::AuthGoogle => {
                log::info!("[auth] Google auth requested");
                self.handle_auth_google().await;
            }
            Command::AuthTwitch => {
                log::info!("[auth] Twitch auth requested");
                self.handle_auth_twitch().await;
            }
            Command::AuthDiscord => {
                log::info!("[auth] Discord auth requested");
                self.handle_auth_discord().await;
            }
            Command::AuthApple { identity_token } => {
                log::info!("[auth] Apple auth requested");
                self.handle_auth_apple(&identity_token).await;
            }
            Command::AuthGoogleToken { id_token } => {
                log::info!("[auth] Google token auth requested");
                self.handle_auth_google_token(&id_token).await;
            }
            Command::AuthCustomToken { token, provider } => {
                log::info!("[auth] {} token auth requested", provider);
                self.handle_auth_custom_token(&token, &provider).await;
            }

            // Social link (onboarding — attaches identity to current device account)
            Command::LinkGoogle => {
                log::info!("[auth] Google link requested");
                self.handle_link_google().await;
            }
            Command::LinkSteam => {
                log::info!("[auth] Steam link requested");
                self.handle_link_steam().await;
            }
            Command::LinkTwitch => {
                log::info!("[auth] Twitch link requested");
                self.handle_link_twitch().await;
            }
            Command::LinkDiscord => {
                log::info!("[auth] Discord link requested");
                self.handle_link_discord().await;
            }
            Command::LinkApple { identity_token } => {
                log::info!("[auth] Apple link requested");
                self.handle_link_apple(&identity_token).await;
            }
            Command::LinkGoogleToken { id_token } => {
                log::info!("[auth] Google token link requested");
                self.handle_link_google_token(&id_token).await;
            }
            Command::LinkCustomToken { token, provider } => {
                log::info!("[auth] {} token link requested", provider);
                self.handle_link_custom_token(&token, &provider).await;
            }
            Command::DiscoverCrews { cursor } => {
                self.handle_discover_crews(cursor.as_deref()).await;
            }
            Command::FinalizeOnboarding {
                device_id,
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
                    &device_id,
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
            Command::ResolveCrewInvite { code } => {
                self.handle_resolve_crew_invite(&code).await;
            }
            Command::CreateInviteCode { crew_id } => {
                self.handle_create_invite_code(&crew_id).await;
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
            Command::SetPushToTalk { enabled } => {
                self.voice.set_push_to_talk(enabled);
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
                let fell_back = self.voice.set_capture_device(&id);
                if fell_back {
                    let _ = self.event_tx.send(Event::AudioDeviceFallback {
                        capture_fell_back: true,
                        playback_fell_back: false,
                    });
                }
            }
            Command::SetPlaybackDevice { id } => {
                let fell_back = self.voice.set_playback_device(&id);
                if fell_back {
                    let _ = self.event_tx.send(Event::AudioDeviceFallback {
                        capture_fell_back: false,
                        playback_fell_back: true,
                    });
                }
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
            Command::SetNsMode { mode } => {
                self.voice.set_ns_mode(mode);
            }
            Command::SetTransientSuppression { enabled } => {
                self.voice.set_transient_suppression(enabled);
            }
            Command::SetHighPassFilter { enabled } => {
                self.voice.set_high_pass_filter(enabled);
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
            Command::StartVoiceCaptureInject => {
                self.voice.start_capture_inject();
            }
            Command::InjectCaptureFrame { samples } => {
                self.voice.inject_capture_frame(&samples);
            }
            Command::StopVoiceCaptureInject => {
                self.voice.stop_capture_inject();
            }
            Command::SetDebugMode { enabled } => {
                self.voice.set_debug_mode(enabled);
            }
            Command::SetDiagnosticCapture { enabled } => {
                self.voice.set_diagnostic_capture(enabled);
            }
            Command::UploadDiagnosticLog {
                local_path,
                capture_id,
            } => {
                self.handle_upload_diagnostic_log(&local_path, &capture_id)
                    .await;
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

            // --- Crew admin ---
            Command::UpdateCrew {
                crew_id,
                name,
                description,
                avatar,
                open,
                invite_policy,
            } => {
                self.handle_update_crew(
                    &crew_id,
                    name.as_deref(),
                    description.as_deref(),
                    avatar.as_deref(),
                    open,
                    invite_policy.as_deref(),
                )
                .await;
            }
            Command::DeleteCrew { crew_id } => {
                self.handle_delete_crew(&crew_id).await;
            }
            Command::KickCrewMember { crew_id, user_id } => {
                self.handle_kick_crew_member(&crew_id, &user_id).await;
            }
            Command::ChangeCrewRole {
                crew_id,
                user_id,
                new_role,
            } => {
                self.handle_change_crew_role(&crew_id, &user_id, new_role)
                    .await;
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
                waveform,
            } => {
                self.handle_post_clip(&crew_id, &clip_id, duration_seconds, &local_path, &waveform)
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
            Command::LoadCrewFeed { crew_id } => {
                self.handle_load_crew_feed(&crew_id).await;
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
                game_id,
                duration_min,
                wins,
                losses,
                draws,
            } => {
                self.handle_game_session_end(
                    &crew_id,
                    &game_name,
                    &game_id,
                    duration_min,
                    wins,
                    losses,
                    draws,
                )
                .await;
            }
            Command::SetCustomGames { games } => {
                self.custom_games = games;
                self.rebuild_game_db();
            }
            Command::AddCustomGame { game } => {
                log::info!(
                    "[game-sensor] custom game confirmed: {} ({} -> {})",
                    game.name,
                    game.exe,
                    game.id
                );
                self.custom_games.push(game);
                self.rebuild_game_db();
            }
            Command::UploadGameIcon { game_id, png } => {
                use base64::Engine as _;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
                match self.nakama.game_icon_set(&game_id, &b64).await {
                    Ok(()) => log::info!(
                        "[game-icon] uploaded icon for {game_id} ({} bytes)",
                        png.len()
                    ),
                    // Best-effort: sharing failing must never break tracking.
                    Err(e) => log::warn!("[game-icon] upload for {game_id} failed: {e}"),
                }
            }
            Command::FetchGameIcon { game_id } => {
                use base64::Engine as _;
                match self.nakama.game_icon_get(&game_id).await {
                    Ok(b64) if !b64.is_empty() => {
                        match base64::engine::general_purpose::STANDARD.decode(&b64) {
                            Ok(png) => {
                                let _ = self.event_tx.send(Event::GameIconLoaded { game_id, png });
                            }
                            Err(e) => {
                                log::warn!("[game-icon] bad icon payload for {game_id}: {e}")
                            }
                        }
                    }
                    // Not found (or empty) is the common quiet case.
                    Ok(_) => {}
                    Err(e) => log::debug!("[game-icon] fetch for {game_id}: {e}"),
                }
            }
            Command::GetUserGameStats => {
                self.refresh_user_game_stats().await;
            }

            Command::LoadGamesSettings => {
                // Install detection touches the filesystem/registry: off-thread.
                let registry = self.telemetry_registry.clone();
                let event_tx = self.event_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let game_db = GameDatabase::load_bundled();
                    let games = registry
                        .all()
                        .iter()
                        .map(|adapter| {
                            let info = adapter.info();
                            // Badge styling comes from the game DB entry so the
                            // settings rows match the now-playing card.
                            let db_entry = game_db.lookup_by_id(adapter.game_id());
                            crate::events::GameIntegrationStatus {
                                game_id: adapter.game_id().to_string(),
                                name: info.game_name.to_string(),
                                short_name: db_entry
                                    .map(|e| e.short_name.clone())
                                    .unwrap_or_else(|| info.game_name.to_string()),
                                color: db_entry.and_then(|e| e.color.clone()).unwrap_or_default(),
                                installed: adapter.detect_install(),
                                writes_files: info.writes_files,
                                note: info.note.to_string(),
                                account_link: info.account_link.map(str::to_string),
                            }
                        })
                        .collect();
                    let _ = event_tx.send(Event::GamesSettings { games });
                });
                self.send_riot_status().await;
            }
            Command::SetGameIntegrations { disabled } => {
                log::info!("[telemetry] disabled integrations set to {disabled:?}");
                self.disabled_integrations = disabled.into_iter().collect();
            }
            Command::RiotLink { riot_id, region } => {
                match self.nakama.riot_link(&riot_id, &region).await {
                    Ok(status) => {
                        let _ = self.event_tx.send(Event::RiotStatus {
                            available: true,
                            linked: true,
                            riot_id: status.riot_id.unwrap_or(riot_id),
                            region: status.region.unwrap_or(region),
                        });
                    }
                    Err(e) => {
                        log::warn!("riot_link failed: {e}");
                        let _ = self.event_tx.send(Event::RiotLinkFailed {
                            reason: e.to_string(),
                        });
                    }
                }
            }
            Command::RiotUnlink => match self.nakama.riot_unlink().await {
                Ok(()) => {
                    let _ = self.event_tx.send(Event::RiotStatus {
                        available: true,
                        linked: false,
                        riot_id: String::new(),
                        region: String::new(),
                    });
                }
                Err(e) => log::warn!("riot_unlink failed: {e}"),
            },
            Command::LoadRiotStatus => {
                self.send_riot_status().await;
            }

            #[cfg(feature = "test-faults")]
            Command::FaultNakamaDisconnect => {
                log::warn!("test-fault: forcing Nakama WS disconnect");
                self.nakama.force_ws_disconnect();
            }
            #[cfg(feature = "test-faults")]
            Command::FaultSfuDisconnect => {
                log::warn!("test-fault: forcing SFU voice disconnect");
                self.voice.mark_disconnected_with_reason("test_fault");
            }
            #[cfg(feature = "test-faults")]
            Command::FaultSimulateSuspend => {
                log::warn!("test-fault: simulating suspend (backdating liveness clock)");
                self.reconnect.backdate_liveness();
            }
        }
    }
}
