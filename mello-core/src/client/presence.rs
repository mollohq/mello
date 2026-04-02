use crate::events::Event;

impl super::Client {
    pub(super) async fn handle_update_profile(
        &self,
        display_name: &str,
        avatar_data: Option<&str>,
        avatar_format: Option<&str>,
        avatar_style: Option<&str>,
        avatar_seed: Option<&str>,
    ) {
        let user_id = self.nakama.current_user_id().unwrap_or_default();
        let has_avatar = avatar_data.is_some();

        if let Some(data) = avatar_data {
            let mut obj = serde_json::json!({
                "format": avatar_format.unwrap_or("svg"),
                "data": data,
            });
            if let Some(style) = avatar_style {
                obj["style"] = serde_json::Value::String(style.to_string());
            }
            if let Some(seed) = avatar_seed {
                obj["seed"] = serde_json::Value::String(seed.to_string());
            }
            let value_str = obj.to_string();
            if let Err(e) = self
                .nakama
                .write_storage("avatars", "current", &value_str, 2, 1)
                .await
            {
                log::error!("[profile] failed to write avatar: {}", e);
                return;
            }
            log::info!("[profile] avatar saved to storage");
        }

        let avatar_url = if has_avatar {
            Some(format!("/v2/storage/avatars/current/{}", user_id))
        } else {
            None
        };

        match self
            .nakama
            .update_account_fields(Some(display_name), avatar_url.as_deref())
            .await
        {
            Ok(()) => {
                log::info!("[profile] updated: display_name={}", display_name);
                let _ = self.event_tx.send(Event::ProfileUpdated {
                    display_name: display_name.to_string(),
                    avatar_data: avatar_data.map(|s| s.to_string()),
                });
            }
            Err(e) => {
                log::error!("[profile] failed to update account: {}", e);
            }
        }
    }

    pub(super) async fn handle_crew_catchup(&self, crew_id: &str, last_seen: i64) {
        match self.nakama.crew_catchup(crew_id, last_seen).await {
            Ok(response) => {
                let _ = self.event_tx.send(Event::CatchupLoaded { response });
            }
            Err(e) => {
                log::warn!("crew_catchup failed: {}", e);
            }
        }
    }

    pub(super) async fn handle_post_moment(
        &self,
        crew_id: &str,
        sentiment: &str,
        text: &str,
        game_name: &str,
    ) {
        let req = crate::crew_events::PostMomentRequest {
            crew_id: crew_id.to_string(),
            sentiment: sentiment.to_string(),
            text: text.to_string(),
            game_name: game_name.to_string(),
        };
        match self.nakama.post_moment(&req).await {
            Ok(resp) => {
                let _ = self.event_tx.send(Event::MomentPosted {
                    event_id: resp.event_id,
                });
            }
            Err(e) => {
                log::warn!("post_moment failed: {}", e);
                let _ = self.event_tx.send(Event::MomentPostFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    pub(super) async fn handle_game_session_end(
        &self,
        crew_id: &str,
        game_name: &str,
        duration_min: u32,
    ) {
        let req = crate::crew_events::GameSessionEndRequest {
            crew_id: crew_id.to_string(),
            game_name: game_name.to_string(),
            duration_min,
        };
        if let Err(e) = self.nakama.game_session_end(&req).await {
            log::warn!("game_session_end failed: {}", e);
        }
    }
}
