use super::browser_flow::{Credential, FlowIntent, FlowOutcome, SocialProvider};
use crate::events::Event;
use crate::oauth::OAuthError;
use crate::presence::PresenceStatus;
use crate::session;

/// The reason the user reads when a browser flow ends without an identity.
fn flow_failure_reason(provider: SocialProvider, error: &OAuthError) -> String {
    let label = provider.label();
    match error {
        OAuthError::Cancelled => format!("You cancelled the {} sign-in.", label),
        other => format!("{} sign-in failed: {}", label, other),
    }
}

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
        // A browser flow that ends after logout must not sign in or link.
        self.browser_flows.cancel();

        // Needs the session, so it runs before anything clears it. Otherwise the
        // next user on this device receives this user's mention pushes.
        self.unregister_push_token_on_logout().await;

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
        // Drop client-level voice state so the voice tick's SFU reconnect
        // scheduler can't try to rejoin the old channel after logout.
        self.last_voice_channel = None;
        self.sfu_voice_reconnect = None;
        let _ = self
            .event_tx
            .send(Event::VoiceStateChanged { in_call: false });

        session::clear();
        if let Err(e) = self.nakama.leave_crew_channel().await {
            log::warn!("Leave channel on logout: {}", e);
        }
        // Close the realtime socket and drop in-memory auth so the reconnect
        // supervisor stays inert until the next login (otherwise it would
        // immediately rebuild the just-closed socket).
        self.nakama.clear_session();
        log::info!("Logged out, session cleared");
    }

    /// Delete the account server-side, then tear down the local session.
    ///
    /// Irreversible. The release smoke test drives this so each run removes the
    /// throwaway account it created; without it every release leaves one behind
    /// and `admin_dashboard_stats` slowly overstates `users_total`.
    ///
    /// On failure the session is left intact — the account still exists, so
    /// clearing it locally would only hide the leak.
    pub(super) async fn handle_delete_account(&mut self) {
        if let Err(e) = self.nakama.delete_account().await {
            log::error!("[auth] failed to delete account: {}", e);
            let _ = self.event_tx.send(Event::AccountDeleteFailed {
                reason: e.to_string(),
            });
            return;
        }

        // The account is gone; every socket and cached session below now refers
        // to a user the server no longer knows. Reuse the logout teardown so
        // the reconnect supervisor does not try to rebuild them.
        self.handle_logout().await;

        log::info!("[auth] account deleted");
        let _ = self.event_tx.send(Event::AccountDeleted);
    }

    /// Start a browser sign-in or link, and return at once (#88).
    ///
    /// The flow runs on its own thread. The loop keeps handling commands and
    /// voice ticks, and `on_browser_flow_finished` completes the flow. While
    /// a flow waits, a new request is ignored with a log line.
    pub(super) fn start_browser_flow(&mut self, provider: SocialProvider, intent: FlowIntent) {
        let label = provider.label();
        let Some(client_id) = provider.client_id(self.nakama.config()) else {
            log::warn!("[auth] {} client id not configured", label);
            self.send_flow_failure(intent, format!("{} login not configured", label));
            return;
        };
        self.browser_flows.start(provider, intent, move |cancel| {
            provider.run(&client_id, cancel)
        });
    }

    /// Complete a browser flow with its outcome: sign in, or link.
    pub(super) async fn on_browser_flow_finished(&mut self, outcome: FlowOutcome) {
        let FlowOutcome {
            provider,
            intent,
            result,
        } = outcome;
        let label = provider.label();

        let credential = match result {
            Ok(Ok(credential)) => credential,
            Ok(Err(OAuthError::Aborted)) => {
                // The app stopped it (logout, shutdown). Nobody waits for a reason.
                log::info!("[auth] {} browser flow stopped by the app", label);
                return;
            }
            Ok(Err(OAuthError::Cancelled)) => {
                log::info!("[auth] {} browser flow cancelled by the user", label);
                self.send_flow_failure(
                    intent,
                    flow_failure_reason(provider, &OAuthError::Cancelled),
                );
                return;
            }
            Ok(Err(e)) => {
                log::error!("[auth] {} browser flow failed: {}", label, e);
                self.send_flow_failure(intent, flow_failure_reason(provider, &e));
                return;
            }
            Err(e) => {
                log::error!("[auth] {} browser flow task panicked: {}", label, e);
                self.send_flow_failure(intent, format!("{} sign-in failed unexpectedly", label));
                return;
            }
        };

        // Google returns a code: exchange it for the id_token here.
        let token = match credential {
            Credential::GoogleCode { code, verifier } => {
                match self.nakama.google_exchange_code(&code, &verifier).await {
                    Ok(id_token) => id_token,
                    Err(e) => {
                        log::error!("[auth] Google token exchange failed: {}", e);
                        self.send_flow_failure(intent, e.to_string());
                        return;
                    }
                }
            }
            Credential::Token(token) => token,
        };

        match (intent, provider.custom_id()) {
            (FlowIntent::SignIn, None) => match self.nakama.authenticate_google(&token).await {
                Ok(user) => self.on_social_login(user).await,
                Err(e) => {
                    log::error!("[auth] Google Nakama auth failed: {}", e);
                    self.send_flow_failure(intent, e.to_string());
                }
            },
            (FlowIntent::SignIn, Some(custom_id)) => {
                match self.nakama.authenticate_custom(&token, custom_id).await {
                    Ok(user) => self.on_social_login(user).await,
                    Err(e) => {
                        log::error!("[auth] {} Nakama auth failed: {}", label, e);
                        self.send_flow_failure(intent, e.to_string());
                    }
                }
            }
            (FlowIntent::Link, None) => self.link_or_switch_google(&token).await,
            (FlowIntent::Link, Some(custom_id)) => {
                self.link_or_switch(&token, custom_id, label).await
            }
        }
    }

    /// Report a failed sign-in or link to the screen that started it.
    fn send_flow_failure(&self, intent: FlowIntent, reason: String) {
        let event = match intent {
            FlowIntent::SignIn => Event::LoginFailed { reason },
            FlowIntent::Link => Event::SocialLinkFailed { reason },
        };
        let _ = self.event_tx.send(event);
    }

    /// Authenticate with an Apple identity token captured natively on the client.
    /// Unlike Google/Discord there's no in-core browser flow — the token arrives
    /// in the command. An empty token means the platform has no native flow (desktop).
    pub(super) async fn handle_auth_apple(&mut self, identity_token: &str) {
        if identity_token.is_empty() {
            log::warn!("[auth] Apple auth: no identity token (unsupported on this platform)");
            let _ = self.event_tx.send(Event::LoginFailed {
                reason: "Apple sign-in isn't available here".into(),
            });
            return;
        }

        match self.nakama.authenticate_apple(identity_token).await {
            Ok(user) => self.on_social_login(user).await,
            Err(e) => {
                log::error!("[auth] Apple Nakama auth failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    /// Authenticate (login or create) with a Google id_token captured natively on
    /// the client (iOS login screen). Sign-in counterpart to `handle_link_google_token`.
    pub(super) async fn handle_auth_google_token(&mut self, id_token: &str) {
        if id_token.is_empty() {
            let _ = self.event_tx.send(Event::LoginFailed {
                reason: "Google sign-in returned no token".into(),
            });
            return;
        }
        match self.nakama.authenticate_google(id_token).await {
            Ok(user) => self.on_social_login(user).await,
            Err(e) => {
                log::error!("[auth] Google Nakama auth failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    /// Authenticate (login or create) with a custom-provider token (Discord/Twitch)
    /// captured natively on the client. Sign-in counterpart to `handle_link_custom_token`.
    pub(super) async fn handle_auth_custom_token(&mut self, token: &str, provider: &str) {
        if token.is_empty() {
            let _ = self.event_tx.send(Event::LoginFailed {
                reason: format!("{} sign-in returned no token", provider),
            });
            return;
        }
        match self.nakama.authenticate_custom(token, provider).await {
            Ok(user) => self.on_social_login(user).await,
            Err(e) => {
                log::error!("[auth] {} Nakama auth failed: {}", provider, e);
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

    /// Attach a Google identity (an id_token from the browser flow) to the
    /// current account. Falls back to signing in when that identity already
    /// belongs to another account, like `link_or_switch`.
    async fn link_or_switch_google(&mut self, id_token: &str) {
        match self.nakama.link_google(id_token).await {
            Ok(()) => {
                log::info!("[auth] Google identity linked to device account");
                let _ = self.event_tx.send(Event::SocialLinked);
            }
            Err(e) if e.to_string().contains("already in use") => {
                log::info!("[auth] Google already linked elsewhere, falling back to authenticate");
                match self.nakama.authenticate_google(id_token).await {
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

    /// Attach a custom-provider identity to the current account, falling back to
    /// signing in when that identity already belongs to another account.
    ///
    /// Shared by Discord, Steam and Twitch. The fallback matters: without it a returning user who reinstalled
    /// would be told their own identity is "already in use" and be stuck.
    async fn link_or_switch(&mut self, token: &str, provider: &str, label: &str) {
        match self.nakama.link_custom(token, provider).await {
            Ok(()) => {
                log::info!("[auth] {} identity linked to device account", label);
                let _ = self.event_tx.send(Event::SocialLinked);
            }
            Err(e) if e.to_string().contains("already in use") => {
                log::info!(
                    "[auth] {} already linked elsewhere, falling back to authenticate",
                    label
                );
                match self.nakama.authenticate_custom(token, provider).await {
                    Ok(user) => self.on_social_login(user).await,
                    Err(e2) => {
                        log::error!("[auth] {} authenticate fallback failed: {}", label, e2);
                        let _ = self.event_tx.send(Event::SocialLinkFailed {
                            reason: e2.to_string(),
                        });
                    }
                }
            }
            Err(e) => {
                log::error!("[auth] {} link failed: {}", label, e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    /// Link an Apple identity (native token) to the current session. Falls back to
    /// authenticate if the identity is already attached to another account — same
    /// shape as Google/Discord linking.
    pub(super) async fn handle_link_apple(&mut self, identity_token: &str) {
        if identity_token.is_empty() {
            log::warn!("[auth] Apple link: no identity token (unsupported on this platform)");
            let _ = self.event_tx.send(Event::SocialLinkFailed {
                reason: "Apple sign-in isn't available here".into(),
            });
            return;
        }

        match self.nakama.link_apple(identity_token).await {
            Ok(()) => {
                log::info!("[auth] Apple identity linked to account");
                let _ = self.event_tx.send(Event::SocialLinked);
            }
            Err(e) if e.to_string().contains("already in use") => {
                log::info!("[auth] Apple already linked elsewhere, falling back to authenticate");
                match self.nakama.authenticate_apple(identity_token).await {
                    Ok(user) => self.on_social_login(user).await,
                    Err(e2) => {
                        log::error!("[auth] Apple authenticate fallback failed: {}", e2);
                        let _ = self.event_tx.send(Event::SocialLinkFailed {
                            reason: e2.to_string(),
                        });
                    }
                }
            }
            Err(e) => {
                log::error!("[auth] Apple link failed: {}", e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    /// Link a Google identity from a natively-obtained id_token (iOS). Same shape
    /// as `handle_link_google` minus the in-core browser flow.
    pub(super) async fn handle_link_google_token(&mut self, id_token: &str) {
        if id_token.is_empty() {
            let _ = self.event_tx.send(Event::SocialLinkFailed {
                reason: "Google sign-in returned no token".into(),
            });
            return;
        }
        match self.nakama.link_google(id_token).await {
            Ok(()) => {
                log::info!("[auth] Google identity linked to account");
                let _ = self.event_tx.send(Event::SocialLinked);
            }
            Err(e) if e.to_string().contains("already in use") => {
                log::info!("[auth] Google already linked elsewhere, falling back to authenticate");
                match self.nakama.authenticate_google(id_token).await {
                    Ok(user) => self.on_social_login(user).await,
                    Err(e2) => {
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

    /// Link a custom-provider identity (Discord, Twitch) from a natively-obtained
    /// token (iOS). Same shape as `handle_link_discord` minus the browser flow.
    pub(super) async fn handle_link_custom_token(&mut self, token: &str, provider: &str) {
        if token.is_empty() {
            let _ = self.event_tx.send(Event::SocialLinkFailed {
                reason: format!("{} sign-in returned no token", provider),
            });
            return;
        }
        match self.nakama.link_custom(token, provider).await {
            Ok(()) => {
                log::info!("[auth] {} identity linked to account", provider);
                let _ = self.event_tx.send(Event::SocialLinked);
            }
            Err(e) if e.to_string().contains("already in use") => {
                log::info!(
                    "[auth] {} already linked elsewhere, falling back to authenticate",
                    provider
                );
                match self.nakama.authenticate_custom(token, provider).await {
                    Ok(user) => self.on_social_login(user).await,
                    Err(e2) => {
                        let _ = self.event_tx.send(Event::SocialLinkFailed {
                            reason: e2.to_string(),
                        });
                    }
                }
            }
            Err(e) => {
                log::error!("[auth] {} link failed: {}", provider, e);
                let _ = self.event_tx.send(Event::SocialLinkFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_finalize_onboarding(
        &mut self,
        device_id: &str,
        invite_code: Option<String>,
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
        log::info!(
            "[onboarding] finalizing — device auth with id={}",
            device_id
        );

        let (user, _created) = match self.nakama.authenticate_device(device_id).await {
            Ok(pair) => pair,
            Err(e) => {
                log::error!("[onboarding] device auth failed: {}", e);
                let _ = self.event_tx.send(Event::OnboardingFailed {
                    reason: "Couldn't create your account. Check your connection and try again."
                        .into(),
                    join_error: None,
                });
                return;
            }
        };

        // The session is saved only when finalize succeeds (below). Saved
        // here, a failed join left a session that the next launch restored
        // into the app with no crew and no identity step. Unsaved, a retry
        // device-auths into the same account.

        if let Err(e) = self.nakama.connect_ws(self.event_tx.clone()).await {
            log::error!("[onboarding] WebSocket connect failed: {}", e);
            let _ = self.event_tx.send(Event::OnboardingFailed {
                reason: "Couldn't connect. Check your connection and try again.".into(),
                join_error: None,
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

        let final_crew_id = if let Some(code) = invite_code {
            // The invite code is the authorization, so a private crew works
            // too. `join_by_invite_code` is idempotent: a retry after a later
            // step failed finds the user already a member.
            match self.nakama.join_by_invite_code(&code).await {
                Ok((id, name)) => {
                    log::info!(
                        "[onboarding] joined crew id={} name={:?} with the invite code",
                        id,
                        name
                    );
                    Some(id)
                }
                Err(e) => {
                    let error = crate::crew::InviteError::from_error(&e);
                    log::error!(
                        "[onboarding] failed to join the invited crew: {:?}: {}",
                        error,
                        e
                    );
                    let _ = self.event_tx.send(Event::OnboardingInviteFailed { error });
                    return;
                }
            }
        } else if let Some(id) = crew_id {
            if let Err(e) = self.nakama.join_group(&id).await {
                let error = crate::crew::InviteError::from_join_error(&e);
                log::error!(
                    "[onboarding] failed to join crew {}: {:?}: {}",
                    id,
                    error,
                    e
                );
                let _ = self.event_tx.send(Event::OnboardingFailed {
                    reason: join_failure_reason(error).into(),
                    join_error: Some(error),
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
                        reason: "Couldn't create the crew. Try again.".into(),
                        join_error: None,
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

        if let Some(rt) = self.nakama.refresh_token() {
            let _ = session::save(rt);
        }

        let mut updated_user = user;
        updated_user.display_name = display_name.to_string();
        let _ = self
            .event_tx
            .send(Event::OnboardingReady { user: updated_user });
    }
}

/// Plain text for a failed crew join during onboarding. Never the server text.
fn join_failure_reason(error: crate::crew::InviteError) -> &'static str {
    use crate::crew::InviteError;
    match error {
        InviteError::CrewFull => "That crew is full. Pick another one.",
        InviteError::InvalidCode => "That crew no longer exists. Pick another one.",
        InviteError::NotAllowed => "You can't join that crew. Pick another one.",
        InviteError::Failed => "Couldn't join the crew. Check your connection and try again.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_onboarding_join_never_shows_server_text() {
        use crate::crew::InviteError;
        for error in [
            InviteError::CrewFull,
            InviteError::InvalidCode,
            InviteError::NotAllowed,
            InviteError::Failed,
        ] {
            let reason = join_failure_reason(error);
            assert!(
                !reason.contains('{') && !reason.contains("code"),
                "{reason}"
            );
        }
        assert_eq!(
            join_failure_reason(InviteError::CrewFull),
            "That crew is full. Pick another one."
        );
    }

    #[test]
    fn a_refusal_at_the_provider_reads_as_a_cancel() {
        // #87: not "Timeout waiting for authentication".
        assert_eq!(
            flow_failure_reason(SocialProvider::Discord, &OAuthError::Cancelled),
            "You cancelled the Discord sign-in."
        );
        assert_eq!(
            flow_failure_reason(SocialProvider::Steam, &OAuthError::Cancelled),
            "You cancelled the Steam sign-in."
        );
    }

    #[test]
    fn other_flow_failures_name_the_provider_and_the_cause() {
        assert_eq!(
            flow_failure_reason(SocialProvider::Twitch, &OAuthError::Timeout),
            "Twitch sign-in failed: Timeout waiting for authentication"
        );
        assert_eq!(
            flow_failure_reason(
                SocialProvider::Google,
                &OAuthError::Provider("server_error".into())
            ),
            "Google sign-in failed: The provider returned an error: server_error"
        );
    }
}
