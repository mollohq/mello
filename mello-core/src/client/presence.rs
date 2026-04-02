use crate::events::Event;

impl super::Client {
    pub(super) async fn handle_update_profile(&self, display_name: &str) {
        match self.nakama.update_account(display_name).await {
            Ok(()) => {
                log::info!("Profile updated: display_name={}", display_name);
            }
            Err(e) => {
                log::error!("Failed to update profile: {}", e);
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
