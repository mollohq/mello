use crate::events::Event;

impl super::Client {
    pub(super) async fn handle_discover_crews(&self, cursor: Option<&str>) {
        match self.nakama.discover_crews_public(50, cursor).await {
            Ok((crews, next_cursor)) => {
                log::info!(
                    "[discover] loaded {} crews, has_more={}",
                    crews.len(),
                    next_cursor.is_some()
                );
                let _ = self.event_tx.send(Event::DiscoverCrewsLoaded {
                    crews,
                    cursor: next_cursor,
                });
            }
            Err(e) => {
                log::error!("Failed to discover crews: {}", e);
            }
        }
    }

    pub(super) async fn handle_join_crew(&mut self, crew_id: &str) {
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

    pub(super) async fn load_crews(&self) {
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

    pub(super) async fn handle_create_crew(
        &mut self,
        name: &str,
        description: &str,
        open: bool,
        avatar: Option<&str>,
        invite_user_ids: &[String],
    ) {
        log::info!(
            "[crew] creating crew name={:?} open={} has_avatar={} invite_count={}",
            name,
            open,
            avatar.is_some(),
            invite_user_ids.len()
        );
        if let Some(a) = avatar {
            log::info!("[crew] avatar payload: {} bytes base64", a.len());
        }
        match self
            .nakama
            .create_crew(name, description, open, avatar, invite_user_ids)
            .await
        {
            Ok((crew, invite_code)) => {
                log::info!(
                    "[crew] created crew id={} name={:?} invite_code={:?}",
                    crew.id,
                    crew.name,
                    invite_code
                );
                let crew_id = crew.id.clone();
                let _ = self.event_tx.send(Event::CrewCreated { crew, invite_code });
                self.handle_select_crew(&crew_id).await;
                self.load_crews().await;
            }
            Err(e) => {
                log::error!("[crew] failed to create crew: {}", e);
                let _ = self.event_tx.send(Event::CrewCreateFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    pub(super) async fn handle_search_users(&self, query: &str) {
        log::debug!("[search] searching users query={:?}", query);
        match self.nakama.search_users(query).await {
            Ok(users) => {
                log::debug!("[search] found {} users for query={:?}", users.len(), query);
                let _ = self.event_tx.send(Event::UserSearchResults { users });
            }
            Err(e) => {
                log::warn!("[search] user search failed for query={:?}: {}", query, e);
                let _ = self
                    .event_tx
                    .send(Event::UserSearchResults { users: vec![] });
            }
        }
    }

    pub(super) async fn handle_resolve_crew_invite(&self, code: &str) {
        log::info!("[invite] resolving crew invite code={:?}", code);
        match self.nakama.resolve_crew_invite(code).await {
            Ok(invite) => {
                log::info!(
                    "[invite] resolved invite: crew={:?} id={}",
                    invite.crew_name,
                    invite.crew_id,
                );
                let _ = self.event_tx.send(Event::CrewInviteResolved {
                    code: code.to_string(),
                    invite,
                });
            }
            Err(e) => {
                log::error!("[invite] failed to resolve invite code: {}", e);
                let _ = self.event_tx.send(Event::CrewInviteResolveFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    pub(super) async fn handle_join_by_invite_code(&mut self, code: &str) {
        log::info!("[invite] joining crew by invite code={:?}", code);
        match self.nakama.join_by_invite_code(code).await {
            Ok((crew_id, name)) => {
                log::info!(
                    "[invite] joined crew id={} name={:?} via invite code",
                    crew_id,
                    name
                );
                let _ = self.event_tx.send(Event::CrewJoined {
                    crew_id: crew_id.clone(),
                });
                self.handle_select_crew(&crew_id).await;
                self.load_crews().await;
            }
            Err(e) => {
                log::error!("[invite] failed to join by invite code: {}", e);
                let _ = self.event_tx.send(Event::Error {
                    message: format!("Invalid invite code: {}", e),
                });
            }
        }
    }

    pub(super) async fn handle_fetch_crew_avatars(&self, crew_ids: &[String]) {
        log::info!("[avatar] fetching avatars for {} crews", crew_ids.len());
        for crew_id in crew_ids {
            match self.nakama.get_crew_avatar(crew_id).await {
                Ok(raw) if !raw.is_empty() => {
                    // RPC returns the storage value JSON: {"data":"base64..."}
                    let data = serde_json::from_str::<serde_json::Value>(&raw)
                        .ok()
                        .and_then(|v| v.get("data")?.as_str().map(String::from))
                        .unwrap_or(raw);
                    log::info!(
                        "[avatar] loaded avatar for crew {} ({} bytes)",
                        crew_id,
                        data.len()
                    );
                    let _ = self.event_tx.send(Event::CrewAvatarLoaded {
                        crew_id: crew_id.clone(),
                        data,
                    });
                }
                Ok(_) => {
                    log::debug!("[avatar] no avatar data for crew {}", crew_id);
                }
                Err(e) => {
                    log::warn!(
                        "[avatar] failed to fetch avatar for crew {}: {}",
                        crew_id,
                        e
                    );
                }
            }
        }
    }

    pub(super) async fn handle_fetch_user_avatar(&self, user_id: &str) {
        log::info!("[avatar] fetching avatar for user {}", user_id);
        match self
            .nakama
            .read_storage("avatars", "current", user_id)
            .await
        {
            Ok(raw) if !raw.is_empty() => {
                let parsed: serde_json::Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!("[avatar] failed to parse user avatar JSON: {}", e);
                        return;
                    }
                };
                let data = parsed
                    .get("data")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if !data.is_empty() {
                    log::info!(
                        "[avatar] loaded avatar for user {} ({} bytes)",
                        user_id,
                        data.len()
                    );
                    let _ = self.event_tx.send(Event::UserAvatarLoaded {
                        user_id: user_id.to_string(),
                        data,
                    });
                } else {
                    log::debug!("[avatar] no avatar data in storage for user {}", user_id);
                }
            }
            Ok(_) => {
                log::debug!("[avatar] no avatar stored for user {}", user_id);
            }
            Err(e) => {
                log::warn!(
                    "[avatar] failed to fetch avatar for user {}: {}",
                    user_id,
                    e
                );
            }
        }
    }

    pub(super) async fn handle_fetch_user_avatars(&self, user_ids: &[String]) {
        if user_ids.is_empty() {
            return;
        }
        log::info!(
            "[avatar] batch-fetching avatars for {} users",
            user_ids.len()
        );
        match self
            .nakama
            .read_storage_batch("avatars", "current", user_ids)
            .await
        {
            Ok(results) => {
                for (uid, raw) in results {
                    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
                        Ok(v) => v,
                        Err(e) => {
                            log::warn!("[avatar] failed to parse avatar JSON for {}: {}", uid, e);
                            continue;
                        }
                    };
                    let data = parsed
                        .get("data")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if !data.is_empty() {
                        log::debug!(
                            "[avatar] loaded avatar for user {} ({} bytes)",
                            uid,
                            data.len()
                        );
                        let _ = self
                            .event_tx
                            .send(Event::UserAvatarLoaded { user_id: uid, data });
                    }
                }
            }
            Err(e) => {
                log::warn!("[avatar] batch fetch failed: {}", e);
            }
        }
    }

    pub(super) async fn handle_select_crew(&mut self, crew_id: &str) {
        self.sfu_leave_if_connected().await;
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

        // Fetch catch-up BEFORE set_active_crew, because set_active_crew
        // updates last_seen to now (which would make catch-up think we
        // were just here and skip the event ledger).
        self.handle_crew_catchup(crew_id, 0).await;

        // Load the crew feed timeline
        self.handle_load_crew_timeline(crew_id, None).await;

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

        // Fetch members first so the display name cache is populated for chat messages
        if let Ok(members) = self.nakama.list_group_users(crew_id).await {
            let user_ids: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
            if let Err(e) = self.nakama.follow_users(&user_ids).await {
                log::warn!("Failed to follow users: {}", e);
            }
        }

        // Wait for WS reader to set channel_id (up to 2s)
        let channel_id = self.wait_for_channel_id().await;
        if let Some(ch_id) = channel_id {
            match self
                .nakama
                .list_channel_messages_with_cursor(&ch_id, 50, None)
                .await
            {
                Ok((mut messages, cursor)) => {
                    messages.reverse();
                    self.history_cursor = cursor;
                    let _ = self.event_tx.send(Event::MessagesLoaded { messages });
                }
                Err(e) => log::error!("Failed to fetch message history: {}", e),
            }
        }

        // Auto-join voice (last-used channel, or default if first time)
        if let Some(ch_id) = &voice_channel_id {
            self.handle_join_voice(ch_id).await;
        }
    }

    pub(super) async fn handle_leave_crew(&mut self) {
        // Leave voice (local + server)
        self.sfu_leave_if_connected().await;
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

    /// Tell the server which crew is active and get full state back.
    pub(super) async fn handle_set_active_crew(&self, crew_id: &str) {
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
    pub(super) async fn handle_subscribe_sidebar(&self, crew_ids: &[String]) {
        match self.nakama.subscribe_sidebar(crew_ids).await {
            Ok(crews) => {
                let _ = self.event_tx.send(Event::SidebarUpdated { crews });
            }
            Err(e) => {
                log::warn!("subscribe_sidebar failed: {}", e);
            }
        }
    }
}
