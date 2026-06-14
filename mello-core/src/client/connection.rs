use crate::events::Event;
use crate::presence::PresenceStatus;
use crate::session;

impl super::Client {
    /// Called after successful auth + WS connect. Sets online presence and fetches ICE config.
    pub(super) async fn on_connected(&mut self) {
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
                self.ice_servers = urls.clone();
                self.voice.set_ice_servers(urls);
            }
            Err(e) => {
                log::warn!("Failed to fetch ICE servers, using defaults: {}", e);
            }
        }
    }

    pub(super) async fn check_protocol_version(&self) {
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
                            min_client,
                            crate::PROTOCOL_VERSION,
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
                            crate::MIN_SERVER_PROTOCOL,
                            server_proto,
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

    /// Supervises the realtime WebSocket: detects drops, half-open sockets, and
    /// sleep/wake gaps, then reconnects with backoff and resyncs state. Driven
    /// by `connection_tick` in the run loop (~every 3s).
    pub(super) async fn connection_tick(&mut self) {
        // Sleep/wake detection uses the wall clock (SystemTime), not a monotonic
        // clock: monotonic clocks freeze during suspend on macOS/Linux and would
        // hide the gap. `now` (monotonic) drives backoff timing.
        let now = tokio::time::Instant::now();
        let wall_now = std::time::SystemTime::now();

        // All timing/state decisions are made by the pure supervisor; this
        // function only performs the resulting IO. See `reconnect.rs`.
        let decision = self.reconnect.poll(
            now,
            wall_now,
            self.nakama.is_ws_connected(),
            self.nakama.has_session(),
        );

        if decision.woke_from_sleep {
            log::warn!("Detected wake from sleep; forcing realtime reconnect + resync");
            self.nakama.force_ws_disconnect();
            // Drop the (likely dead) SFU voice connection so the voice tick's
            // reconnect scheduler rebuilds it against fresh state.
            if self.voice.is_active() {
                self.sfu_leave_if_connected().await;
                self.voice.mark_disconnected();
            }
        }

        if decision.connection_lost_edge {
            log::warn!("Realtime WS connection lost");
            let _ = self.event_tx.send(Event::ConnectionStateChanged {
                connected: false,
                reconnecting: true,
            });
        }

        if decision.heartbeat_due {
            // Heartbeat presence ~every 60s so the server-side voice GC can use a
            // short staleness window without evicting idle-but-connected users.
            self.reconnect.record_heartbeat(now);
            if let Err(e) = self.nakama.presence_heartbeat().await {
                log::debug!("presence heartbeat failed: {}", e);
            }
        }

        if decision.attempt_reconnect {
            let attempt = self.reconnect.begin_reconnect_attempt();
            log::info!("Realtime WS reconnect attempt {}", attempt);

            // The access token may have expired while we were offline/asleep;
            // refresh it before reconnecting (no-op cost if still valid).
            self.refresh_token().await;

            match self.nakama.connect_ws(self.event_tx.clone()).await {
                Ok(()) => {
                    log::info!("Realtime WS reconnected (attempt {})", attempt);
                    self.reconnect.record_reconnect_result(now, true);
                    let _ = self.event_tx.send(Event::ConnectionStateChanged {
                        connected: true,
                        reconnecting: false,
                    });
                    self.resync_after_reconnect().await;
                }
                Err(e) => {
                    self.reconnect.record_reconnect_result(now, false);
                    log::warn!(
                        "Realtime WS reconnect attempt {} failed: {}; retrying with backoff",
                        attempt,
                        e
                    );
                }
            }
        }
    }

    /// After the realtime socket is rebuilt, restore the active crew's channel
    /// subscription and pull authoritative state (snapshot-wins resync). If we
    /// were in a voice channel, re-join so Nakama voice membership and the SFU
    /// session are re-established.
    pub(super) async fn resync_after_reconnect(&mut self) {
        let Some(crew_id) = self.nakama.active_crew_id().map(String::from) else {
            return;
        };
        if let Err(e) = self.nakama.join_crew_channel(&crew_id).await {
            log::warn!("Reconnect: failed to rejoin crew channel: {}", e);
        }
        self.handle_set_active_crew(&crew_id).await;
        if let Some(channel) = self.last_voice_channel.clone() {
            self.handle_join_voice(&channel).await;
        }
    }

    pub(super) async fn refresh_token(&mut self) {
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
}
