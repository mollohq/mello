use crate::crew_state::VoiceJoinResponse;
use crate::events::Event;
use crate::voice::{SignalEnvelope, SignalPurpose};

use super::sfu_voice_join::{JoinOutcome, JoinStep, SfuVoiceJoin};

impl super::Client {
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
            && self.last_voice_channel.is_some()
            && self.voice.voice_mode() == crate::voice::VoiceMode::Disconnected
            && self.sfu_voice_reconnect.is_none()
        {
            let channel = self.last_voice_channel.clone().unwrap();
            let delay = tokio::time::Duration::from_secs(2);
            log::info!("SFU voice dropped, scheduling reconnect in {:?}", delay);
            self.sfu_voice_reconnect = Some((tokio::time::Instant::now() + delay, channel, 0));
        }

        if let Some((at, ref channel, attempt)) = self.sfu_voice_reconnect.clone() {
            if !join_pending && tokio::time::Instant::now() >= at {
                const MAX_RECONNECT_ATTEMPTS: u32 = 5;
                if attempt >= MAX_RECONNECT_ATTEMPTS {
                    log::warn!("SFU voice reconnect: giving up after {} attempts", attempt);
                    self.sfu_voice_reconnect = None;
                    self.last_voice_channel = None;
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
                    self.handle_join_voice(&ch).await;
                    // If still disconnected after rejoin, bump the attempt with backoff.
                    // An SFU join that still runs is disconnected too. Its outcome
                    // clears the next attempt if voice starts.
                    if self.voice.voice_mode() == crate::voice::VoiceMode::Disconnected {
                        let backoff = tokio::time::Duration::from_secs(2u64.pow(attempt + 1));
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

    pub(super) async fn handle_join_voice(&mut self, channel_id: &str) {
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

        self.last_voice_channel = Some(resp.channel_id.clone());
        self.sfu_voice_reconnect = None;

        // Emit authoritative state immediately so the UI shows the initial member list.
        // Must happen BEFORE the SFU connection (which can take seconds), otherwise
        // VoiceChannelsUpdated notifications that arrive during connection get overwritten.
        let _ = self.event_tx.send(Event::VoiceJoined {
            crew_id: crew_id.clone(),
            channel_id: resp.channel_id.clone(),
            members: resp.voice_state.members.clone(),
        });

        let local_id = self.nakama.current_user_id().map(String::from);
        self.start_voice_media(&crew_id, local_id.as_deref(), &resp)
            .await;
    }

    /// Stop the current voice media and start it for the joined channel.
    ///
    /// P2P starts here. An SFU join only starts here: its network steps run
    /// off the command loop, and `on_sfu_voice_join_step` finishes it. Voice
    /// ticks and other commands keep running while the SFU answers or fails.
    pub(super) async fn start_voice_media(
        &mut self,
        crew_id: &str,
        local_id: Option<&str>,
        resp: &VoiceJoinResponse,
    ) {
        self.sfu_leave_if_connected().await;
        self.voice.leave_voice();
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
                    unsafe { crate::transport::SfuConnection::create_peer(ctx) }
                };
                match peer_handle {
                    Ok(ph) => {
                        self.sfu_voice_joins.join_session(join, conn, ph);
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
            client.start_voice_media("crew-1", Some("me"), &resp),
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

    #[tokio::test]
    async fn leaving_voice_cancels_the_sfu_join() {
        let sfu = silent_sfu().await;
        let (mut client, _events) = client_without_audio();
        let resp = sfu_join_response("ch-1", &sfu.endpoint);
        tokio::time::timeout(
            Duration::from_secs(2),
            client.start_voice_media("crew-1", Some("me"), &resp),
        )
        .await
        .expect("the join handler waited for the SFU connect");
        assert!(client.sfu_voice_joins.is_pending());

        client.sfu_leave_if_connected().await;
        assert!(!client.sfu_voice_joins.is_pending());
    }
}
