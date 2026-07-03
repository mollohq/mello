use std::sync::Arc;

use crate::game_db::GameDatabase;
use crate::game_sensing::{GameEvent, GameSensor};
use crate::telemetry::{self, AdapterRegistry, TelemetryListener, TELEMETRY_PORT};

use super::Client;

impl Client {
    /// Start game process scanning and telemetry listener after auth.
    /// No-op when game sensing is disabled or already started.
    pub(super) fn ensure_game_services(&mut self) {
        if !self.enable_game_sensor {
            return;
        }
        if self.game_sensor.is_some() {
            return;
        }

        let game_db = GameDatabase::load_bundled();
        let mello_ctx = self.voice.mello_ctx();
        let (sensor, game_event_rx) = GameSensor::start(mello_ctx, game_db);
        self.game_sensor = Some(sensor);
        *self.game_event_rx.lock().unwrap() = Some(game_event_rx);
        log::info!("Game sensor started (post-auth)");

        let telemetry_registry = Arc::new(AdapterRegistry::with_defaults());
        let telemetry_token = telemetry::load_or_create_token();
        match TelemetryListener::start(telemetry_registry.clone(), telemetry_token.clone()) {
            Ok((listener, rx)) => {
                self.telemetry_listener = Some(listener);
                *self.telemetry_event_rx.lock().unwrap() = Some(rx);
            }
            Err(e) => {
                log::warn!("[telemetry] listener failed to bind on {TELEMETRY_PORT}: {e}");
            }
        }
        self.telemetry_registry = Some(telemetry_registry.clone());
        self.telemetry_token = Some(telemetry_token.clone());

        for adapter in telemetry_registry.all() {
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
