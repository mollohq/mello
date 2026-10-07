use crate::crew_state::VoiceJoinResponse;
use crate::events::Event;
use crate::voice::{SignalEnvelope, SignalPurpose};

use super::sfu_voice_join::{JoinOutcome, JoinStep, SfuVoiceJoin};

/// The first SFU voice reconnect attempt starts this long after the drop
/// (spec 10 section 7.6).
pub(super) const SFU_RECONNECT_FIRST_DELAY: tokio::time::Duration =
    tokio::time::Duration::from_millis(500);
/// Reconnect attempts before the client gives up and leaves the call.
pub(super) const SFU_RECONNECT_MAX_ATTEMPTS: u32 = 5;
/// The life of the SFU voice token from `voice_join` (backend `sfu_auth.go`).
pub(super) const SFU_VOICE_TOKEN_LIFETIME: tokio::time::Duration =
    tokio::time::Duration::from_secs(5 * 60);
/// A token this close to its expiry counts as expired. The client's clock
/// for the token starts after the backend issued it, and the SFU checks the
/// expiry when the WebSocket connects.
pub(super) const SFU_VOICE_TOKEN_MARGIN: tokio::time::Duration =
    tokio::time::Duration::from_secs(15);

/// The delay before reconnect attempt `attempt` (0 is the first): 500 ms,
/// then 2, 4, 8 and 16 s.
pub(super) fn sfu_reconnect_delay(attempt: u32) -> tokio::time::Duration {
    if attempt == 0 {
        SFU_RECONNECT_FIRST_DELAY
    } else {
        tokio::time::Duration::from_secs(2u64.saturating_pow(attempt))
    }
}

/// The last `voice_join` answer of the joined channel. A reconnect reuses
/// its SFU endpoint and token while the token is valid, so it needs no
/// backend round trip.
#[derive(Debug, Clone)]
pub(crate) struct VoiceJoinGrant {
    pub crew_id: String,
    pub local_id: String,
    pub resp: VoiceJoinResponse,
    /// When the client received the token.
    pub issued_at: tokio::time::Instant,
}

impl VoiceJoinGrant {
    /// True while the token is valid at `now`, with the margin.
    pub fn is_fresh(&self, now: tokio::time::Instant) -> bool {
        now < self.issued_at + SFU_VOICE_TOKEN_LIFETIME - SFU_VOICE_TOKEN_MARGIN
    }
}

impl super::Client {
    /// The `voice_speaking` presence RPC. Crew members outside the channel
    /// read it from the backend's `voice_update` push. In an SFU call the
    /// caller rate-limits it to one change per second.
    pub(super) async fn send_speaking_presence(&self, speaking: bool) {
        if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
            log::debug!("voice_speaking RPC: crew={} speaking={}", crew_id, speaking);
            if let Err(e) = self.nakama.voice_speaking(&crew_id, speaking).await {
                log::warn!("voice_speaking RPC failed: {}", e);
            }
        } else {
            log::debug!("voice_speaking: no active crew");
        }
    }

    pub(super) async fn voice_tick(&mut self) {
        let sfu_mode = self.voice.voice_mode() == crate::voice::VoiceMode::SFU;
        let mut steps = super::loop_watchdog::TickSteps::start();
        self.voice.tick(&mut steps);

        // An SFU voice join that runs decides the voice mode when it ends.
        // Until then, Disconnected is not a drop, and no reconnect may start.
        let join_pending = self.sfu_voice_joins.is_pending();

        // SFU voice reconnect: if voice mode went Disconnected but we still have a
        // last_voice_channel, schedule a reconnect with exponential backoff.
        if !join_pending
            && self.voice.voice_mode() == crate::voice::VoiceMode::Disconnected
            && self.sfu_voice_reconnect.is_none()
        {
            if let Some(channel) = self.last_voice_channel.clone() {
                let delay = sfu_reconnect_delay(0);
                log::info!("SFU voice dropped, scheduling reconnect in {:?}", delay);
                self.sfu_voice_reconnect = Some((tokio::time::Instant::now() + delay, channel, 0));
            }
        }

        if let Some((at, ref channel, attempt)) = self.sfu_voice_reconnect.clone() {
            if !join_pending && tokio::time::Instant::now() >= at {
                if attempt >= SFU_RECONNECT_MAX_ATTEMPTS {
                    log::warn!("SFU voice reconnect: giving up after {} attempts", attempt);
                    self.sfu_voice_reconnect = None;
                    self.last_voice_channel = None;
                    self.voice_join_grant = None;
                    let _ = self.event_tx.send(Event::VoiceStateChanged {
                        in_call: false,
                        transport: crate::voice::VoiceMode::Disconnected,
                    });
                } else {
                    log::info!(
                        "SFU voice reconnect attempt {} for channel {}",
                        attempt + 1,
                        channel
                    );
                    let ch = channel.clone();
                    self.reconnect_voice(&ch).await;
                    // If still disconnected after rejoin, bump the attempt with backoff.
                    // An SFU join that still runs is disconnected too. Its outcome
                    // clears the next attempt if voice starts.
                    if self.voice.voice_mode() == crate::voice::VoiceMode::Disconnected {
                        let backoff = sfu_reconnect_delay(attempt + 1);
                        self.sfu_voice_reconnect =
                            Some((tokio::time::Instant::now() + backoff, ch, attempt + 1));
                    }
                }
            }
        }

        steps.mark("sfu_reconnect");

        // Send any pending signaling messages through Nakama
        let signals = self.voice.drain_signals();
        for (to, signal) in signals {
            let envelope = SignalEnvelope {
                purpose: SignalPurpose::Voice,
                stream_width: None,
                stream_height: None,
                stream_bitrate_kbps: None,
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
        steps.mark("signal_send");
        self.voice_tick_budget
            .check(sfu_mode, &steps, std::time::Instant::now());
    }

    pub(super) async fn wait_for_channel_id(&self) -> Option<String> {
        for _ in 0..20 {
            if let Some(id) = self.nakama.channel_id().await {
                return Some(id);
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        log::warn!("Timed out waiting for channel_id");
        None
    }

    /// One SFU voice reconnect attempt for `channel_id`. While the voice
    /// token of the last join is valid, the attempt goes straight to the SFU
    /// with it. Else it asks the backend for a new token (`voice_join`).
    /// A failed attempt leaves voice disconnected for the next one: it does
    /// not fall back to P2P, because the other members stay on the SFU.
    pub(super) async fn reconnect_voice(&mut self, channel_id: &str) {
        let now = tokio::time::Instant::now();
        let grant = self
            .voice_join_grant
            .as_ref()
            .filter(|g| g.resp.channel_id == channel_id && g.is_fresh(now))
            .cloned();
        match grant {
            Some(grant) => {
                log::info!(
                    "SFU voice reconnect reuses the voice token ({} ms old)",
                    now.duration_since(grant.issued_at).as_millis()
                );
                self.start_voice_media(&grant.crew_id, Some(&grant.local_id), &grant.resp, true)
                    .await;
            }
            None => {
                log::info!("SFU voice reconnect: no valid voice token, asking for a new one");
                self.join_voice_channel(channel_id, true).await;
            }
        }
    }

    pub(super) async fn handle_join_voice(&mut self, channel_id: &str) {
        self.join_voice_channel(channel_id, false).await;
    }

    /// `voice_join`, then start the media. `reconnect` is true for an
    /// attempt of the reconnect scheduler.
    async fn join_voice_channel(&mut self, channel_id: &str, reconnect: bool) {
        let crew_id = match self.nakama.active_crew_id().map(String::from) {
            Some(id) => id,
            None => return,
        };

        let preferred_region = crate::transport::sfu_region::preferred_region(&self.sfu_regions);
        // RPC returns the authoritative channel state after join
        let resp = match self
            .nakama
            .voice_join(&crew_id, channel_id, preferred_region.as_deref())
            .await
        {
            Ok(r) => r,
            Err(e) => {
                log::error!("voice_join RPC failed: {}", e);
                return;
            }
        };

        self.last_voice_channel = Some(resp.channel_id.clone());
        if !reconnect {
            self.sfu_voice_reconnect = None;
        }

        // Emit authoritative state immediately so the UI shows the initial member list.
        // Must happen BEFORE the SFU connection (which can take seconds), otherwise
        // VoiceChannelsUpdated notifications that arrive during connection get overwritten.
        let _ = self.event_tx.send(Event::VoiceJoined {
            crew_id: crew_id.clone(),
            channel_id: resp.channel_id.clone(),
            members: resp.voice_state.members.clone(),
        });

        let local_id = self.nakama.current_user_id().map(String::from);
        self.store_voice_join_grant(&crew_id, local_id.as_deref(), &resp);
        self.start_voice_media(&crew_id, local_id.as_deref(), &resp, reconnect)
            .await;
    }

    /// Keep a `voice_join` answer for the reconnect scheduler.
    pub(super) fn store_voice_join_grant(
        &mut self,
        crew_id: &str,
        local_id: Option<&str>,
        resp: &VoiceJoinResponse,
    ) {
        self.voice_join_grant = local_id.map(|local_id| VoiceJoinGrant {
            crew_id: crew_id.to_string(),
            local_id: local_id.to_string(),
            resp: resp.clone(),
            issued_at: tokio::time::Instant::now(),
        });
    }

    /// Stop the current voice media and start it for the joined channel.
    ///
    /// P2P starts here. An SFU join only starts here: its network steps run
    /// off the command loop, and `on_sfu_voice_join_step` finishes it. Voice
    /// ticks and other commands keep running while the SFU answers or fails.
    ///
    /// `reconnect` is true for an attempt of the reconnect scheduler. A
    /// failed SFU reconnect does not fall back to P2P.
    pub(super) async fn start_voice_media(
        &mut self,
        crew_id: &str,
        local_id: Option<&str>,
        resp: &VoiceJoinResponse,
        reconnect: bool,
    ) {
        self.sfu_leave_if_connected().await;
        self.voice.leave_voice();
        // The channel bitrate is the top rung of the encoder ladder; set it
        // before capture starts (spec 10 section 4.6).
        self.voice.set_channel_bitrate(
            resp.bitrate_kbps
                .unwrap_or(crate::crew_state::DEFAULT_CHANNEL_BITRATE_KBPS),
        );
        let Some(local_id) = local_id else {
            return;
        };

        let p2p_peer_ids: Vec<String> = resp
            .voice_state
            .members
            .iter()
            .filter(|m| m.user_id != local_id)
            .map(|m| m.user_id.clone())
            .collect();

        if resp.mode.as_deref().unwrap_or("p2p") == "sfu" {
            let join = SfuVoiceJoin {
                crew_id: crew_id.to_string(),
                channel_id: resp.channel_id.clone(),
                local_id: local_id.to_string(),
                p2p_peer_ids,
                reconnect,
            };
            let endpoint = resp.sfu_endpoint.as_deref().unwrap_or_default();
            let token = resp.sfu_token.as_deref().unwrap_or_default();
            self.sfu_voice_joins.connect(join, endpoint, token);
            return;
        }

        self.voice.join_voice(local_id, &p2p_peer_ids);
        self.on_voice_media_started();
    }

    /// Continue the SFU voice join with the outcome of its last network step.
    /// Runs on the loop. A failed step falls back to P2P.
    pub(super) fn on_sfu_voice_join_step(&mut self, outcome: JoinOutcome) {
        let JoinOutcome { join, step, result } = outcome;
        match result {
            Ok(JoinStep::Connected(conn)) => {
                let peer_handle = {
                    let ctx = self.voice.mello_ctx();
                    // SAFETY: the voice manager owns the live context.
                    unsafe { crate::transport::SfuConnection::create_peer(ctx, &self.ice_servers) }
                };
                match peer_handle {
                    Ok(ph) => {
                        self.sfu_voice_joins.join_session(join, conn, ph);
                        return;
                    }
                    Err(e) if join.reconnect => {
                        log::warn!("SFU reconnect: peer creation failed: {}", e);
                        return;
                    }
                    Err(e) => {
                        log::error!("SFU peer creation failed: {}, falling back to P2P", e);
                        self.voice.join_voice(&join.local_id, &join.p2p_peer_ids);
                    }
                }
            }
            Ok(JoinStep::Ready(conn)) => {
                let conn = std::sync::Arc::new(conn);
                self.voice
                    .join_voice_sfu(&join.local_id, &join.crew_id, conn);
            }
            Err(e) if join.reconnect => {
                // The other members stay on the SFU: P2P would reach none of
                // them. Voice stays disconnected; the scheduler tries again.
                log::warn!(
                    "SFU reconnect: {} failed: {}; the next attempt follows",
                    step,
                    e
                );
                return;
            }
            Err(e) => {
                log::error!("SFU {} failed: {}, falling back to P2P", step, e);
                self.voice.join_voice(&join.local_id, &join.p2p_peer_ids);
            }
        }
        self.on_voice_media_started();
    }

    /// The voice media for the joined channel started, or tried to start.
    fn on_voice_media_started(&mut self) {
        // A reconnect attempt keeps its next attempt only when voice did not start.
        if self.voice.voice_mode() != crate::voice::VoiceMode::Disconnected {
            self.sfu_voice_reconnect = None;
        }

        // The transport is the one that started: the SFU, or P2P after an
        // SFU join failed.
        let _ = self.event_tx.send(Event::VoiceStateChanged {
            in_call: true,
            transport: self.voice.voice_mode(),
        });

        // Auto-start clip buffer for voice clip capture
        self.handle_start_clip_buffer();
    }

    /// Leave the SFU voice session, if any, and cancel an SFU voice join that
    /// runs. Every voice teardown calls this before `VoiceManager::leave_voice`.
    pub(super) async fn sfu_leave_if_connected(&mut self) {
        self.sfu_voice_joins.cancel();
        if let Some(conn) = self.voice.sfu_connection() {
            conn.leave().await;
        }
    }

    pub(super) async fn handle_leave_voice(&mut self) {
        // Stop clip buffer before tearing down voice
        self.handle_stop_clip_buffer();

        self.sfu_leave_if_connected().await;
        self.last_voice_channel = None;
        self.sfu_voice_reconnect = None;
        self.voice_join_grant = None;
        // Notify server
        if let Some(crew_id) = self.nakama.active_crew_id().map(String::from) {
            if let Err(e) = self.nakama.voice_leave(&crew_id).await {
                log::warn!("voice_leave RPC failed: {}", e);
            }
        }
        self.voice.leave_voice();
        let _ = self.event_tx.send(Event::VoiceStateChanged {
            in_call: false,
            transport: crate::voice::VoiceMode::Disconnected,
        });
    }

    pub(super) async fn handle_create_voice_channel(&self, crew_id: &str, name: &str) {
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

    pub(super) async fn handle_rename_voice_channel(
        &self,
        crew_id: &str,
        channel_id: &str,
        name: &str,
    ) {
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

    pub(super) async fn handle_update_voice_channel_bitrate(
        &self,
        crew_id: &str,
        channel_id: &str,
        bitrate_kbps: u32,
    ) {
        match self
            .nakama
            .channel_update(crew_id, channel_id, bitrate_kbps)
            .await
        {
            Ok(()) => {
                log::info!(
                    "Voice channel {} bitrate set to {} kbps",
                    channel_id,
                    bitrate_kbps
                );
                let _ = self.event_tx.send(Event::VoiceChannelBitrateChanged {
                    crew_id: crew_id.to_string(),
                    channel_id: channel_id.to_string(),
                    bitrate_kbps,
                });
            }
            Err(e) => {
                log::error!("channel_update RPC failed: {}", e);
                let _ = self.event_tx.send(Event::Error {
                    message: format!("Failed to change the voice channel bitrate: {}", e),
                });
            }
        }
    }

    pub(super) async fn handle_delete_voice_channel(&self, crew_id: &str, channel_id: &str) {
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
}

#[cfg(test)]
mod tests {
    use super::super::sfu_voice_join::tests::{join_for, silent_sfu};
    use super::super::Client;
    use crate::command::Command;
    use crate::config::Config;
    use crate::crew_state::VoiceJoinResponse;
    use crate::events::Event;
    use crate::voice::VoiceManager;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    const LIMIT: Duration = Duration::from_secs(10);

    /// A client whose voice manager has no libmello context, so the loop runs
    /// without audio devices. It has no session, so no tick calls Nakama.
    fn client_without_audio() -> (Client, mpsc::Receiver<Event>) {
        let (event_tx, events) = mpsc::channel();
        let voice = VoiceManager::without_audio(event_tx.clone());
        let client = Client::with_voice(
            Config::default(),
            event_tx,
            voice,
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(std::sync::atomic::AtomicU8::new(0)),
            false,
            false,
        );
        (client, events)
    }

    fn sfu_join_response(channel_id: &str, endpoint: &str) -> VoiceJoinResponse {
        serde_json::from_value(serde_json::json!({
            "channel_id": channel_id,
            "voice_state": {
                "channel_id": channel_id,
                "members": [{ "user_id": "me" }, { "user_id": "peer-1" }],
            },
            "mode": "sfu",
            "sfu_endpoint": endpoint,
            "sfu_token": "token",
        }))
        .expect("valid voice_join response")
    }

    /// Wait for the first event that `pick` accepts. Returns the events
    /// before it. Polls, because the loop runs on this test's thread.
    async fn wait_for_event(
        events: &mpsc::Receiver<Event>,
        what: &str,
        pick: impl Fn(&Event) -> bool,
    ) -> Vec<Event> {
        let deadline = tokio::time::Instant::now() + LIMIT;
        let mut before = Vec::new();
        loop {
            while let Ok(ev) = events.try_recv() {
                if pick(&ev) {
                    return before;
                }
                before.push(ev);
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no {what} event; saw {before:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn is_in_call(ev: &Event) -> bool {
        matches!(ev, Event::VoiceStateChanged { in_call: true, .. })
    }

    #[tokio::test]
    async fn the_loop_handles_commands_while_the_sfu_connect_waits_then_falls_back() {
        // The SFU takes the connection and does not answer: the connect waits.
        let sfu = silent_sfu().await;
        let (mut client, events) = client_without_audio();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();

        // The JoinVoice handler must return while the SFU connect waits.
        let resp = sfu_join_response("ch-1", &sfu.endpoint);
        tokio::time::timeout(
            Duration::from_secs(2),
            client.start_voice_media("crew-1", Some("me"), &resp, false),
        )
        .await
        .expect("the join handler waited for the SFU connect");
        assert!(client.sfu_voice_joins.is_pending());

        let driver = async {
            tokio::time::timeout(LIMIT, sfu.accepted)
                .await
                .expect("the connect reaches the SFU")
                .expect("accept task runs");

            // The connect still waits. The loop must handle another command.
            cmd_tx
                .send(Command::ListAudioDevices)
                .expect("the loop listens");
            let before = wait_for_event(&events, "AudioDevicesListed", |ev| {
                matches!(ev, Event::AudioDevicesListed { .. })
            })
            .await;
            assert!(
                !before.iter().any(is_in_call),
                "voice started before the SFU connect ended: {before:?}"
            );

            // The connect fails. The loop falls back to P2P and reports the call.
            drop(sfu.release);
            wait_for_event(&events, "VoiceStateChanged { in_call: true }", is_in_call).await;

            drop(cmd_tx);
        };
        tokio::time::timeout(LIMIT, async { tokio::join!(client.run(cmd_rx), driver) })
            .await
            .expect("the loop ends when the command channel closes");

        assert!(!client.sfu_voice_joins.is_pending());
    }

    #[tokio::test]
    async fn the_voice_tick_does_not_reconnect_while_an_sfu_join_runs() {
        // Voice is Disconnected while the join runs. That is not a drop.
        let sfu = silent_sfu().await;
        let (mut client, _events) = client_without_audio();
        client.last_voice_channel = Some("ch-1".into());
        client
            .sfu_voice_joins
            .connect(join_for("ch-1"), &sfu.endpoint, "token");

        client.voice_tick().await;
        assert!(
            client.sfu_voice_reconnect.is_none(),
            "a reconnect was scheduled while the SFU join runs"
        );

        // A scheduled attempt that is due must also wait for the join.
        client.sfu_voice_reconnect = Some((tokio::time::Instant::now(), "ch-1".into(), 0));
        client.voice_tick().await;
        assert!(
            client.sfu_voice_joins.is_pending(),
            "the reconnect replaced the running join"
        );
    }

    /// Spec 10 section 8: every voice control reaches libmello. The command
    /// runs through the real handler into a libmello context on the
    /// device-free backend, and libmello reports the new setting.
    #[tokio::test]
    async fn input_sensitivity_command_reaches_libmello() {
        let (event_tx, _events) = mpsc::channel();
        let voice = VoiceManager::with_test_audio_backend(event_tx.clone());
        let mut client = Client::with_voice(
            Config::default(),
            event_tx,
            voice,
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(std::sync::atomic::AtomicU8::new(0)),
            false,
            false,
        );
        let stats = |client: &Client| {
            // SAFETY: zeroed is a valid MelloDebugStats; the context is live
            // while the client holds its voice manager.
            unsafe {
                let mut s: mello_sys::MelloDebugStats = std::mem::zeroed();
                mello_sys::mello_get_debug_stats(client.voice.mello_ctx(), &mut s);
                s
            }
        };
        assert!(stats(&client).input_sensitivity_auto, "auto is the default");

        client
            .handle_command(Command::SetInputSensitivity {
                auto: false,
                db: -33.0,
            })
            .await;
        let s = stats(&client);
        assert!(!s.input_sensitivity_auto);
        assert_eq!(s.input_sensitivity_db, -33.0);

        client
            .handle_command(Command::SetInputSensitivity {
                auto: true,
                db: -12.0,
            })
            .await;
        let s = stats(&client);
        assert!(s.input_sensitivity_auto);
        assert_eq!(s.input_sensitivity_db, -12.0);
    }

    /// Spec 10 section 8: per-member volume reaches libmello, also for a
    /// member that has not sent audio yet, and libmello clamps it.
    #[tokio::test]
    async fn peer_volume_command_reaches_libmello() {
        let (event_tx, _events) = mpsc::channel();
        let voice = VoiceManager::with_test_audio_backend(event_tx.clone());
        let mut client = Client::with_voice(
            Config::default(),
            event_tx,
            voice,
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(std::sync::atomic::AtomicU8::new(0)),
            false,
            false,
        );
        let volume = |client: &Client, peer: &std::ffi::CStr| {
            // SAFETY: the context is live while the client holds its voice
            // manager; the peer id is a valid C string.
            unsafe {
                mello_sys::mello_voice_get_peer_volume(client.voice.mello_ctx(), peer.as_ptr())
            }
        };
        assert_eq!(volume(&client, c"user-7"), 1.0, "unity is the default");

        client
            .handle_command(Command::SetPeerVolume {
                peer_id: "user-7".into(),
                volume: 0.25,
            })
            .await;
        assert_eq!(volume(&client, c"user-7"), 0.25);
        assert_eq!(volume(&client, c"user-8"), 1.0, "other members keep unity");

        client
            .handle_command(Command::SetPeerVolume {
                peer_id: "user-7".into(),
                volume: 9.0,
            })
            .await;
        assert_eq!(volume(&client, c"user-7"), 2.0, "libmello clamps to 2.0");
    }

    /// Spec 10 section 4.6: the channel bitrate reaches libmello as the top
    /// rung of the encoder ladder, from a join and from the command.
    #[tokio::test]
    async fn channel_bitrate_reaches_libmello() {
        let (event_tx, _events) = mpsc::channel();
        let voice = VoiceManager::with_test_audio_backend(event_tx.clone());
        let mut client = Client::with_voice(
            Config::default(),
            event_tx,
            voice,
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(std::sync::atomic::AtomicU8::new(0)),
            false,
            false,
        );
        let top = |client: &Client| {
            // SAFETY: zeroed is a valid MelloDebugStats; the context is live
            // while the client holds its voice manager.
            unsafe {
                let mut s: mello_sys::MelloDebugStats = std::mem::zeroed();
                mello_sys::mello_get_debug_stats(client.voice.mello_ctx(), &mut s);
                s.tx_max_bitrate_kbps
            }
        };
        assert_eq!(top(&client), 64, "64 kbps is the default");

        // A join applies the bitrate of the voice_join response.
        let resp: VoiceJoinResponse = serde_json::from_value(serde_json::json!({
            "channel_id": "ch-1",
            "voice_state": { "channel_id": "ch-1", "members": [{ "user_id": "me" }] },
            "mode": "p2p",
            "bitrate_kbps": 32,
        }))
        .expect("valid voice_join response");
        client
            .start_voice_media("crew-1", Some("me"), &resp, false)
            .await;
        assert_eq!(top(&client), 32);

        // A channel update while in the channel.
        client
            .handle_command(Command::SetVoiceChannelBitrate { kbps: 96 })
            .await;
        assert_eq!(top(&client), 96);

        // A backend before the field: the default.
        let old: VoiceJoinResponse = serde_json::from_value(serde_json::json!({
            "channel_id": "ch-1",
            "voice_state": { "channel_id": "ch-1", "members": [{ "user_id": "me" }] },
            "mode": "p2p",
        }))
        .expect("valid voice_join response");
        client
            .start_voice_media("crew-1", Some("me"), &old, false)
            .await;
        assert_eq!(top(&client), 64);
        client.voice.leave_voice();
    }

    /// Spec 10 section 7.6: the first reconnect attempt starts 500 ms after
    /// the drop, then 2, 4, 8 and 16 s, five attempts.
    #[tokio::test]
    async fn the_first_sfu_reconnect_attempt_starts_after_500_ms() {
        let (mut client, _events) = client_without_audio();
        client.last_voice_channel = Some("ch-1".into());

        let before = tokio::time::Instant::now();
        client.voice_tick().await;
        let after = tokio::time::Instant::now();
        let (at, channel, attempt) = client
            .sfu_voice_reconnect
            .clone()
            .expect("a dropped call schedules a reconnect");
        assert_eq!(channel, "ch-1");
        assert_eq!(attempt, 0);
        assert!(
            at >= before + Duration::from_millis(500) && at <= after + Duration::from_millis(500),
            "first attempt {:?} after the drop, want 500 ms",
            at - before
        );

        let delays: Vec<u64> = (0..super::SFU_RECONNECT_MAX_ATTEMPTS)
            .map(|a| super::sfu_reconnect_delay(a).as_millis() as u64)
            .collect();
        assert_eq!(delays, [500, 2_000, 4_000, 8_000, 16_000]);
    }

    fn grant_for(endpoint: &str, issued_at: tokio::time::Instant) -> super::VoiceJoinGrant {
        super::VoiceJoinGrant {
            crew_id: "crew-1".into(),
            local_id: "me".into(),
            resp: sfu_join_response("ch-1", endpoint),
            issued_at,
        }
    }

    /// A reconnect reuses the voice token of the last join while it is
    /// valid: it goes to the SFU with no backend round trip.
    #[tokio::test]
    async fn a_reconnect_reuses_a_valid_voice_token() {
        let sfu = silent_sfu().await;
        let (mut client, _events) = client_without_audio();
        client.last_voice_channel = Some("ch-1".into());
        client.voice_join_grant = Some(grant_for(
            &sfu.endpoint,
            tokio::time::Instant::now() - Duration::from_secs(4 * 60),
        ));

        client.reconnect_voice("ch-1").await;
        assert!(
            client.sfu_voice_joins.is_pending(),
            "the reconnect went to the SFU with the kept token"
        );
        tokio::time::timeout(LIMIT, sfu.accepted)
            .await
            .expect("the SFU gets the connection")
            .expect("accept task runs");
    }

    /// An expired token is not reused: the reconnect asks the backend for a
    /// new one. This client has no session, so the backend step does nothing.
    #[tokio::test]
    async fn a_reconnect_does_not_reuse_an_expired_voice_token() {
        let sfu = silent_sfu().await;
        let (mut client, _events) = client_without_audio();
        client.last_voice_channel = Some("ch-1".into());
        let lifetime = super::SFU_VOICE_TOKEN_LIFETIME;
        client.voice_join_grant = Some(grant_for(
            &sfu.endpoint,
            tokio::time::Instant::now() - lifetime,
        ));

        client.reconnect_voice("ch-1").await;
        assert!(
            !client.sfu_voice_joins.is_pending(),
            "an expired token must not reach the SFU"
        );
    }

    /// A failed SFU reconnect leaves voice disconnected for the next
    /// attempt. It does not fall back to P2P: the other members are on the
    /// SFU, and P2P would end the reconnect.
    #[tokio::test]
    async fn a_failed_sfu_reconnect_does_not_fall_back_to_p2p() {
        let (mut client, events) = client_without_audio();
        client.last_voice_channel = Some("ch-1".into());
        let mut join = join_for("ch-1");
        join.reconnect = true;
        client.on_sfu_voice_join_step(super::super::sfu_voice_join::JoinOutcome {
            join,
            step: "connect",
            result: Err("SFU connection failed: refused".into()),
        });
        assert_eq!(
            client.voice.voice_mode(),
            crate::voice::VoiceMode::Disconnected
        );
        assert!(
            !events.try_iter().any(|ev| is_in_call(&ev)),
            "a failed reconnect reported a call"
        );
        assert_eq!(client.last_voice_channel.as_deref(), Some("ch-1"));
    }

    #[tokio::test]
    async fn leaving_voice_cancels_the_sfu_join() {
        let sfu = silent_sfu().await;
        let (mut client, _events) = client_without_audio();
        let resp = sfu_join_response("ch-1", &sfu.endpoint);
        tokio::time::timeout(
            Duration::from_secs(2),
            client.start_voice_media("crew-1", Some("me"), &resp, false),
        )
        .await
        .expect("the join handler waited for the SFU connect");
        assert!(client.sfu_voice_joins.is_pending());

        client.sfu_leave_if_connected().await;
        assert!(!client.sfu_voice_joins.is_pending());
    }
}
