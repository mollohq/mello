//! Remote push: device registration (spec 23 §3) and the activity report the
//! server uses to hold phone pushes while the user is at this app (§6.2).
//! Best-effort: a failed call is logged, never surfaced, and retried later.

use crate::activity::{self, ActivityInputs};

impl super::Client {
    pub(super) async fn handle_register_push_token(
        &mut self,
        token: String,
        platform: &str,
        environment: Option<String>,
    ) {
        let environment = environment.unwrap_or_else(|| "production".to_string());
        match self
            .nakama
            .register_push_token(&token, platform, &environment)
            .await
        {
            Ok(()) => log::info!("[push] registered {} token ({})", platform, environment),
            Err(e) => log::warn!("[push] register_push_token failed: {}", e),
        }
        // Kept even when the RPC failed: logout still tries to remove it.
        self.push_token = Some(token);
    }

    pub(super) async fn unregister_push_token_on_logout(&mut self) {
        let Some(token) = self.push_token.take() else {
            return;
        };
        match self.nakama.unregister_push_token(&token).await {
            Ok(()) => log::info!("[push] unregistered token on logout"),
            Err(e) => log::warn!("[push] unregister_push_token failed: {}", e),
        }
    }

    fn activity_inputs(&self) -> ActivityInputs {
        ActivityInputs {
            foreground: self.window_foreground,
            input_idle_secs: self.input_idle_secs,
            in_voice: self.voice.is_active(),
            hosting_stream: self.stream_session.is_some(),
            game_running: self.game_state.current_game().is_some(),
        }
    }

    /// Sends the activity flag when it changed or the socket is new.
    pub(super) async fn report_activity_if_changed(&mut self) {
        if self.nakama.current_user_id().is_none() {
            return;
        }
        let active = activity::is_active(&self.activity_inputs());
        let generation = self.nakama.ws_generation();
        if self.reported_activity == Some((active, generation)) {
            return;
        }
        match self
            .nakama
            .set_session_activity(active, activity::platform())
            .await
        {
            Ok(()) => {
                log::info!("[push] activity reported: active={}", active);
                self.reported_activity = Some((active, generation));
            }
            // Not connected yet: the next tick tries again.
            Err(e) => log::debug!("[push] activity report deferred: {}", e),
        }
    }
}
