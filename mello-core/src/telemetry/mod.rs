//! Game telemetry: a pluggable per-game layer *above* process sensing (spec 17)
//! that turns real in-game state into match-outcome events. CS2 Game State
//! Integration (GSI) is the first concrete adapter.
//!
//! See `specs/18-GAME-TELEMETRY.md`.

mod cs2_gsi;
mod listener;

use std::sync::Arc;

pub use cs2_gsi::Cs2GsiAdapter;
pub use listener::{TelemetryListener, TELEMETRY_PORT};

/// A decisive (or non-decisive) result of a single match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Win,
    Loss,
    Draw,
    /// Match ended without a clean result (crash, disconnect, abandon). Never
    /// counted as a loss so streaks aren't punished for things outside play.
    Incomplete,
}

impl Outcome {
    /// Only decisive results from ranked-ish modes move a streak.
    pub fn counts_toward_streak(&self) -> bool {
        matches!(self, Outcome::Win | Outcome::Loss)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Win => "win",
            Outcome::Loss => "loss",
            Outcome::Draw => "draw",
            Outcome::Incomplete => "incomplete",
        }
    }
}

/// The result of one match within a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchResult {
    pub game_id: String,
    pub mode: String,
    pub map: String,
    pub result: Outcome,
    pub rounds_won: u32,
    pub rounds_lost: u32,
    pub ts: i64,
}

/// An event produced by a telemetry adapter from inbound game state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryEvent {
    /// A new match started (warmup/live after a previous game over, or first seen).
    MatchStarted { mode: String, map: String },
    /// A round resolved. Carries the live score (for HUD / future auto-clip hooks).
    RoundEnded { ct_score: u32, t_score: u32 },
    /// A match ended with a derived outcome.
    MatchEnded(MatchResult),
}

/// A per-game integration that turns local game state into outcome events.
///
/// Implementations are shared across threads (held in an [`AdapterRegistry`] via
/// `Arc`) and must guard any internal state with interior mutability.
pub trait GameTelemetryAdapter: Send + Sync {
    /// Game DB id this adapter serves (e.g. `"counter-strike-2"`).
    fn game_id(&self) -> &str;

    /// Install or refresh whatever the game needs to emit telemetry. Idempotent;
    /// called lazily when the game is first detected.
    fn ensure_installed(&self, token: &str, port: u16) -> Result<(), TelemetryError>;

    /// Parse one inbound payload into telemetry events. `token` is the expected
    /// per-install auth token; payloads that don't carry it (or don't belong to
    /// this adapter) yield no events.
    fn parse(&self, body: &str, token: &str) -> Vec<TelemetryEvent>;

    /// Reset cross-payload state. Called when the game process exits so a fresh
    /// launch starts match tracking cleanly. Default: no-op.
    fn reset(&self) {}
}

/// Registry of available telemetry adapters, keyed by game id.
pub struct AdapterRegistry {
    adapters: Vec<Arc<dyn GameTelemetryAdapter>>,
}

impl AdapterRegistry {
    /// Build the default registry with all shipped adapters.
    pub fn with_defaults() -> Self {
        Self {
            adapters: vec![Arc::new(Cs2GsiAdapter::new())],
        }
    }

    /// Find the adapter for a given game id, if one is registered.
    pub fn get(&self, game_id: &str) -> Option<Arc<dyn GameTelemetryAdapter>> {
        self.adapters
            .iter()
            .find(|a| a.game_id() == game_id)
            .cloned()
    }

    /// All registered adapters (the listener tries each against an inbound payload).
    pub fn all(&self) -> &[Arc<dyn GameTelemetryAdapter>] {
        &self.adapters
    }
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("telemetry not supported on this platform")]
    Unsupported,

    #[error("could not locate game install: {0}")]
    GameNotFound(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Load the per-install telemetry auth token, generating and persisting one on
/// first use. The token is embedded in each adapter's config and required on
/// every inbound payload, so other local apps can't inject fake results.
///
/// Falls back to an ephemeral token if the token file can't be persisted (still
/// works as long as the client is running before the game launches).
pub fn load_or_create_token() -> String {
    if let Some(path) = token_path() {
        if let Ok(existing) = std::fs::read_to_string(&path) {
            let trimmed = existing.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        let token = generate_token();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, &token) {
            log::warn!("[telemetry] could not persist auth token: {e}");
        }
        return token;
    }
    generate_token()
}

fn generate_token() -> String {
    use rand::Rng;
    rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

fn token_path() -> Option<std::path::PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
            })
    }?;
    Some(base.join("mello").join("telemetry_token"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_streak_eligibility() {
        assert!(Outcome::Win.counts_toward_streak());
        assert!(Outcome::Loss.counts_toward_streak());
        assert!(!Outcome::Draw.counts_toward_streak());
        assert!(!Outcome::Incomplete.counts_toward_streak());
    }

    #[test]
    fn registry_finds_cs2() {
        let reg = AdapterRegistry::with_defaults();
        assert!(reg.get("counter-strike-2").is_some());
        assert!(reg.get("unknown-game").is_none());
        assert_eq!(reg.all().len(), 1);
    }

    #[test]
    fn generated_token_is_nonempty() {
        let t = generate_token();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
