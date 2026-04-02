use crate::events::Event;
use crate::voice::{SignalEnvelope, SignalPurpose};

impl super::Client {
    pub(super) async fn voice_tick(&mut self) {
        self.voice.tick();

        // SFU voice reconnect: if voice mode went Disconnected but we still have a
        // last_voice_channel, schedule a reconnect with exponential backoff.
        if self.last_voice_channel.is_some()
            && self.voice.voice_mode() == crate::voice::VoiceMode::Disconnected
            && self.sfu_voice_reconnect.is_none()
        {
            let channel = self.last_voice_channel.clone().unwrap();
            let delay = tokio::time::Duration::from_secs(2);
            log::info!("SFU voice dropped, scheduling reconnect in {:?}", delay);
            self.sfu_voice_reconnect = Some((tokio::time::Instant::now() + delay, channel, 0));
        }

        if let Some((at, ref channel, attempt)) = self.sfu_voice_reconnect.clone() {
            if tokio::time::Instant::now() >= at {
                const MAX_RECONNECT_ATTEMPTS: u32 = 5;
                if attempt >= MAX_RECONNECT_ATTEMPTS {
                    log::warn!("SFU voice reconnect: giving up after {} attempts", attempt);
                    self.sfu_voice_reconnect = None;
                    self.last_voice_channel = None;
                    let _ = self
                        .event_tx
                        .send(Event::VoiceStateChanged { in_call: false });
                } else {
                    log::info!(
                        "SFU voice reconnect attempt {} for channel {}",
                        attempt + 1,
                        channel
                    );
                    let ch = channel.clone();
                    self.handle_join_voice(&ch).await;
                    // If still disconnected after rejoin, bump the attempt with backoff
                    if self.voice.voice_mode() == crate::voice::VoiceMode::Disconnected {
                        let backoff = tokio::time::Duration::from_secs(2u64.pow(attempt + 1));
                        self.sfu_voice_reconnect =
                            Some((tokio::time::Instant::now() + backoff, ch, attempt + 1));
                    }
                }
            }
        }

        // Send any pending signaling messages through Nakama
        let signals = self.voice.drain_signals();
        for (to, signal) in signals {
            let envelope = SignalEnvelope {
                purpose: SignalPurpose::Voice,
                stream_width: None,
                stream_height: None,
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

        let mode = resp.mode.as_deref().unwrap_or("p2p");

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

        self.sfu_leave_if_connected().await;
        self.voice.leave_voice();
        if let Some(local_id) = self.nakama.current_user_id().map(String::from) {
            match mode {
                "sfu" => {
                    let endpoint = resp.sfu_endpoint.as_deref().unwrap_or_default();
                    let token = resp.sfu_token.as_deref().unwrap_or_default();

                    let fallback_to_p2p =
                        |voice: &mut crate::voice::VoiceManager,
                         local_id: &str,
                         resp: &crate::crew_state::VoiceJoinResponse| {
                            let peer_ids: Vec<String> = resp
                                .voice_state
                                .members
                                .iter()
                                .filter(|m| m.user_id != local_id)
                                .map(|m| m.user_id.clone())
                                .collect();
                            voice.join_voice(local_id, &peer_ids);
                        };

                    match crate::transport::SfuConnection::connect(endpoint, token).await {
                        Ok(mut conn) => {
                            let peer_handle = {
                                let ctx = self.voice.mello_ctx();
                                unsafe { crate::transport::SfuConnection::create_peer(ctx) }
                            };
                            match peer_handle {
                                Ok(ph) => match conn.join_voice(ph, &crew_id, channel_id).await {
                                    Ok(_session) => {
                                        if let Err(e) = conn.wait_for_datachannel_open().await {
                                            log::error!(
                                                "SFU DataChannel failed to open: {}, falling back to P2P",
                                                e
                                            );
                                            fallback_to_p2p(&mut self.voice, &local_id, &resp);
                                        } else {
                                            let conn = std::sync::Arc::new(conn);
                                            self.voice.join_voice_sfu(&local_id, &crew_id, conn);
                                        }
                                    }
                                    Err(e) => {
                                        log::error!(
                                            "SFU voice join failed: {}, falling back to P2P",
                                            e
                                        );
                                        fallback_to_p2p(&mut self.voice, &local_id, &resp);
                                    }
                                },
                                Err(e) => {
                                    log::error!(
                                        "SFU peer creation failed: {}, falling back to P2P",
                                        e
                                    );
                                    fallback_to_p2p(&mut self.voice, &local_id, &resp);
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("SFU connect failed: {}, falling back to P2P", e);
                            fallback_to_p2p(&mut self.voice, &local_id, &resp);
                        }
                    }
                }
                _ => {
                    let peer_ids: Vec<String> = resp
                        .voice_state
                        .members
                        .iter()
                        .filter(|m| m.user_id != local_id)
                        .map(|m| m.user_id.clone())
                        .collect();
                    self.voice.join_voice(&local_id, &peer_ids);
                }
            }

            let _ = self
                .event_tx
                .send(Event::VoiceStateChanged { in_call: true });
        }

        // Note: VoiceJoined was already emitted above (before SFU/P2P connection)
        // to prevent race conditions with VoiceChannelsUpdated notifications.
    }

    pub(super) async fn sfu_leave_if_connected(&self) {
        if let Some(conn) = self.voice.sfu_connection() {
            conn.leave().await;
        }
    }

    pub(super) async fn handle_leave_voice(&mut self) {
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
        let _ = self
            .event_tx
            .send(Event::VoiceStateChanged { in_call: false });
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
