use crate::game_sensing::{GameEvent, GameSensor};
use crate::presence::GamePresence;
use crate::telemetry::{self, TelemetryListener, TELEMETRY_PORT};

use super::Client;

impl Client {
    /// Start game process scanning and telemetry listener after auth.
    /// No-op when game sensing is disabled or already started.
    ///
    /// Deferring these to post-auth keeps startup lean (no process scanning,
    /// no loopback HTTP listener until the user is actually connected).
    /// `disabled_integrations` is seeded from settings before `run()`, so the
    /// config installs below honor it — games like CS2 only read their GSI
    /// config at launch, so installing here (before a game is likely open)
    /// preserves the no-restart-needed behavior. Idempotent; a missing game
    /// just logs at debug.
    pub(super) fn ensure_game_services(&mut self) {
        if !self.enable_game_sensor {
            return;
        }
        if self.game_sensor.is_some() {
            return;
        }

        let mello_ctx = self.voice.mello_ctx();
        let (sensor, game_event_rx) = GameSensor::start(mello_ctx, self.user_games.clone());
        self.game_sensor = Some(sensor);
        *self.game_event_rx.lock().unwrap() = Some(game_event_rx);
        log::info!("Game sensor started (post-auth)");

        let telemetry_token = telemetry::load_or_create_token();
        match TelemetryListener::start(
            self.telemetry_registry.clone(),
            telemetry_token.clone(),
            self.telemetry_event_tx.clone(),
        ) {
            Ok(listener) => self.telemetry_listener = Some(listener),
            Err(e) => {
                log::warn!("[telemetry] listener failed to bind on {TELEMETRY_PORT}: {e}");
            }
        }
        self.telemetry_token = Some(telemetry_token.clone());

        for adapter in self.telemetry_registry.all() {
            if self.disabled_integrations.contains(adapter.game_id()) {
                continue; // user opted out of this integration
            }
            let adapter = adapter.clone();
            let token = telemetry_token.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = adapter.ensure_installed(&token, TELEMETRY_PORT) {
                    log::debug!(
                        "[telemetry] startup install for {} skipped: {e}",
                        adapter.game_id()
                    );
                }
            });
        }
    }

    /// Push the currently-primary game into presence, so crewmates see what
    /// you are playing and `crew_state.active_games` can be computed.
    ///
    /// Only fires on change: the sensor emits an event every scan, and
    /// re-broadcasting identical presence would fan out to every crew member
    /// on a 15s heartbeat for no reason.
    pub(super) async fn sync_game_presence(&mut self) {
        let current = self.game_state.current_game().map(|g| GamePresence {
            game_name: g.game_name.clone(),
            game_id: g.game_id.clone(),
            started_at: crate::presence::to_rfc3339(g.started_at),
        });

        let changed = match (&self.published_game, &current) {
            (None, None) => false,
            (Some(a), Some(b)) => a.game_id != b.game_id,
            _ => true,
        };
        if !changed {
            return;
        }

        match self.nakama.presence_set_game(current.as_ref()).await {
            Ok(()) => {
                log::info!(
                    "[game-presence] published: {}",
                    current.as_ref().map_or("cleared", |g| g.game_name.as_str())
                );
                self.published_game = current;
            }
            Err(e) => {
                // Leave `published_game` alone so the next scan retries.
                log::warn!("[game-presence] failed to publish: {e}");
            }
        }
    }

    pub(super) fn drain_game_events(&self) -> Vec<GameEvent> {
        let Ok(guard) = self.game_event_rx.lock() else {
            return Vec::new();
        };
        let Some(rx) = guard.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    pub(super) fn drain_telemetry_events(&self) -> Vec<crate::telemetry::TelemetryEvent> {
        let Ok(guard) = self.telemetry_event_rx.lock() else {
            return Vec::new();
        };
        let Some(rx) = guard.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }
}
