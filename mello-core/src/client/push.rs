//! Remote push registration (spec 23 §3). Best-effort in both directions: a
//! failed RPC is logged, never surfaced, and never blocks login or logout.

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
}
