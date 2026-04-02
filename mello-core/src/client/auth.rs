use crate::events::Event;
use crate::presence::PresenceStatus;
use crate::session;

impl super::Client {
    pub(super) async fn handle_device_auth(&mut self, device_id: &str) {
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

    pub(super) async fn handle_restore(&mut self) {
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

    pub(super) async fn handle_login(&mut self, email: &str, password: &str) {
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

    pub(super) async fn handle_logout(&mut self) {
        // Notify server we're going offline
        if let Err(e) = self
            .nakama
            .presence_update(&PresenceStatus::Offline, None)
            .await
        {
            log::warn!("Failed to set offline presence on logout: {}", e);
        }

        // Leave voice (local + server-side)
        self.sfu_leave_if_connected().await;
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

    pub(super) async fn handle_auth_google(&mut self) {
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

    pub(super) async fn handle_auth_discord(&mut self) {
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
    pub(super) async fn on_social_login(&mut self, user: crate::events::User) {
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

    pub(super) async fn handle_link_email(&mut self, email: &str, password: &str) {
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

    pub(super) async fn handle_link_google(&mut self) {
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

    pub(super) async fn handle_link_discord(&mut self) {
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

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_finalize_onboarding(
        &mut self,
        crew_id: Option<String>,
        crew_name: Option<String>,
        crew_description: Option<String>,
        crew_open: Option<bool>,
        crew_avatar: Option<String>,
        display_name: &str,
        avatar_data: Option<String>,
        avatar_format: Option<String>,
        avatar_style: Option<String>,
        avatar_seed: Option<String>,
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

        if !display_name.is_empty() || avatar_data.is_some() {
            let avatar_url_value = if avatar_data.is_some() {
                Some(format!("/v2/storage/avatars/current/{}", user.id))
            } else {
                None
            };
            if let Err(e) = self
                .nakama
                .update_account_fields(
                    if display_name.is_empty() {
                        None
                    } else {
                        Some(display_name)
                    },
                    avatar_url_value.as_deref(),
                )
                .await
            {
                log::warn!("[onboarding] failed to update account: {}", e);
            }
        }

        if let Some(ref data) = avatar_data {
            let fmt = avatar_format.as_deref().unwrap_or("svg");
            let mut value = serde_json::json!({
                "format": fmt,
                "data": data,
            });
            if let Some(ref style) = avatar_style {
                value["style"] = serde_json::Value::String(style.clone());
            }
            if let Some(ref seed) = avatar_seed {
                value["seed"] = serde_json::Value::String(seed.clone());
            }
            let value_str = value.to_string();
            log::info!(
                "[onboarding] writing avatar to storage ({} format, {} bytes)",
                fmt,
                value_str.len()
            );
            if let Err(e) = self
                .nakama
                .write_storage("avatars", "current", &value_str, 2, 1)
                .await
            {
                log::warn!("[onboarding] failed to write avatar: {}", e);
            }
        }

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
            match self
                .nakama
                .create_crew(
                    &name,
                    crew_description.as_deref().unwrap_or(""),
                    crew_open.unwrap_or(true),
                    crew_avatar.as_deref(),
                    &[],
                )
                .await
            {
                Ok((crew, _invite_code)) => {
                    let id = crew.id.clone();
                    let _ = self.event_tx.send(Event::CrewCreated {
                        crew,
                        invite_code: None,
                    });
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
}
