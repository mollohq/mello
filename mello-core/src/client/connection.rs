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
