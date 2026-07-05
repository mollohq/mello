//! Game telemetry: a pluggable per-game layer *above* process sensing (spec 17)
//! that turns real in-game state into match-outcome events. CS2 Game State
//! Integration (GSI) is the first concrete adapter.
//!
//! See `specs/18-GAME-TELEMETRY.md`.

mod cs2_gsi;
mod dota2_gsi;
mod hearthstone_log;
mod listener;
mod log_tail;
mod lol_live;
mod lor_local;
mod minecraft_stats;
mod poe_log;
mod rocket_league;
mod sc2_client;
#[cfg(windows)]
mod steam;

use std::sync::Arc;

pub use cs2_gsi::Cs2GsiAdapter;
pub use dota2_gsi::Dota2GsiAdapter;
pub use hearthstone_log::HearthstoneAdapter;
pub use listener::{TelemetryListener, TELEMETRY_PORT};
pub use lol_live::LolLiveAdapter;
pub use lor_local::LorAdapter;
pub use minecraft_stats::MinecraftStatsAdapter;
pub use poe_log::PoeLogAdapter;
pub use rocket_league::RocketLeagueAdapter;
pub use sc2_client::Sc2ClientAdapter;

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

/// How confident downstream surfaces can be in a result, by where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceQuality {
    /// Captured live from the game while it ran (GSI, local API, websocket).
    #[default]
    Live,
    /// Fetched from a post-match source (web API) after the fact.
    PostMatch,
    /// Parsed from a replay/run file written by the game.
    Replay,
    /// Self-reported by the user (the manual post-game tap).
    Manual,
}

/// Per-player performance for one match. Every field is optional: adapters
/// fill what their game actually provides, and surfaces render only what's
/// present (spec 19 §3.5 — never show empty stat slots).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Performance {
    pub kills: Option<u32>,
    pub deaths: Option<u32>,
    pub assists: Option<u32>,
    pub mvps: Option<u32>,
    pub score: Option<u32>,
    pub damage: Option<u64>,
    pub healing: Option<u64>,
    pub goals: Option<u32>,
    pub saves: Option<u32>,
    pub shots: Option<u32>,
    /// Creep score / farm (MOBAs).
    pub cs: Option<u32>,
}

/// What the player brought/made: hero, deck, loadout, build order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildInfo {
    /// Hero / champion / class / legend / race.
    pub character: Option<String>,
    /// Deck code or equivalent shareable build identifier.
    pub deck_code: Option<String>,
    /// Notable items/cards, adapter-defined granularity.
    pub items: Vec<String>,
}

/// Run summary for roguelike/run-based games.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunInfo {
    /// How far the run got (floor/act/zone), adapter-defined.
    pub stage_reached: Option<String>,
    pub difficulty: Option<String>,
    pub duration_sec: Option<u32>,
}

/// The result of one match within a session.
///
/// The always-present fields (`mode`, `map`, `result`, scores) are the
/// scoreline; `performance`/`build`/`run` are optional stat slots filled per
/// game so shooters, card games, and roguelikes flow through one pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchResult {
    pub game_id: String,
    pub mode: String,
    pub map: String,
    pub result: Outcome,
    /// Whether this result may move a streak (ranked-ish mode, per adapter).
    /// `Outcome::counts_toward_streak()` gates on top of this.
    pub streak_eligible: bool,
    /// Player-perspective score: rounds/goals/points won vs lost.
    pub own_score: u32,
    pub opp_score: u32,
    pub performance: Option<Performance>,
    pub build: Option<BuildInfo>,
    pub run: Option<RunInfo>,
    pub source: SourceQuality,
    pub ts: i64,
}

/// An event produced by a telemetry adapter from inbound game state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryEvent {
    /// A new match started (warmup/live after a previous game over, or first seen).
    MatchStarted { mode: String, map: String },
    /// The live score changed, player perspective (HUD / future auto-clip hooks).
    ScoreChanged { own: u32, opp: u32 },
    /// A match ended with a derived outcome. Boxed: `MatchResult` carries the
    /// optional stat slots and dwarfs the other variants.
    MatchEnded(Box<MatchResult>),
    /// The adapter needs a one-time user action before it can capture outcomes
    /// (e.g. Dota 2's `-gamestateintegration` launch option). Surfaced as a
    /// hint in the "now playing" card; telemetry silently stays off until done.
    SetupRequired { game_id: String, hint: String },
}

/// A per-game integration that turns local game state into outcome events.
///
/// Two source styles share this trait:
///
/// - **Push** (CS2/Dota 2 GSI): the game POSTs to our loopback listener, which
///   routes payloads through [`parse`](Self::parse).
/// - **Active** (LoL local API poll, Rocket League websocket, log tails): the
///   adapter owns its transport. [`start`](Self::start) is called when the
///   game is detected; the adapter spawns its worker (a thread, mirroring the
///   listener pattern) and sends events into the shared channel until
///   [`reset`](Self::reset).
///
/// Implementations are shared across threads (held in an [`AdapterRegistry`] via
/// `Arc`) and must guard any internal state with interior mutability.
pub trait GameTelemetryAdapter: Send + Sync {
    /// Game DB id this adapter serves (e.g. `"counter-strike-2"`).
    fn game_id(&self) -> &str;

    /// Install or refresh whatever the game needs to emit telemetry (config
    /// files etc.). Idempotent; called eagerly at client startup and again when
    /// the game is detected. Adapters with no install step return `Ok(())`.
    fn ensure_installed(&self, token: &str, port: u16) -> Result<(), TelemetryError>;

    /// Parse one inbound payload into telemetry events (push sources only).
    /// `token` is the expected per-install auth token; payloads that don't
    /// carry it (or don't belong to this adapter) yield no events.
    ///
    /// **Routing contract:** the listener offers every inbound payload to every
    /// registered adapter (no per-game routing). Implementations MUST strictly
    /// verify the payload is their own game's — e.g. by provider app id — and
    /// return no events otherwise. Different Valve GSI games produce
    /// near-identical payload shapes, so shape alone is not sufficient.
    ///
    /// Default: not a push source; yields no events.
    fn parse(&self, body: &str, token: &str) -> Vec<TelemetryEvent> {
        let _ = (body, token);
        Vec::new()
    }

    /// Start the adapter's own transport (active sources only): spawn a worker
    /// that polls/subscribes/tails and sends [`TelemetryEvent`]s into `tx`
    /// until [`reset`](Self::reset) is called. Called when the adapter's game
    /// is detected. Must be idempotent (a second call while running is a no-op).
    ///
    /// Default: no-op for pure push sources.
    fn start(&self, tx: std::sync::mpsc::Sender<TelemetryEvent>) {
        let _ = tx;
    }

    /// Stop any running transport and clear cross-payload state. Called when
    /// the game process exits so a fresh launch starts tracking cleanly.
    /// Default: no-op.
    fn reset(&self) {}

    /// Static description for the Games settings page.
    fn info(&self) -> AdapterInfo;

    /// Whether the game looks installed on this machine (cheap filesystem or
    /// registry probe; called off the UI thread). `None` = can't tell on this
    /// platform / for this game.
    fn detect_install(&self) -> Option<bool> {
        None
    }
}

/// Static, user-facing description of an adapter for the Games settings page.
#[derive(Debug, Clone)]
pub struct AdapterInfo {
    pub game_name: &'static str,
    /// True when `ensure_installed` writes a file (config/ini/log toggle) for
    /// the game to pick up — the thing the per-game consent toggle gates.
    pub writes_files: bool,
    /// One-line, user-facing description of how the integration works.
    pub note: &'static str,
    /// Account-link provider that unlocks server-verified results ("riot"),
    /// if the game has one.
    pub account_link: Option<&'static str>,
}

/// Registry of available telemetry adapters, keyed by game id.
pub struct AdapterRegistry {
    adapters: Vec<Arc<dyn GameTelemetryAdapter>>,
}

impl AdapterRegistry {
    /// Build the default registry with all shipped adapters.
    pub fn with_defaults() -> Self {
        Self {
            adapters: vec![
                Arc::new(Cs2GsiAdapter::new()),
                Arc::new(Dota2GsiAdapter::new()),
                Arc::new(LolLiveAdapter::new()),
                Arc::new(RocketLeagueAdapter::new()),
                Arc::new(LorAdapter::new()),
                Arc::new(HearthstoneAdapter::new()),
                Arc::new(MinecraftStatsAdapter::new()),
                Arc::new(PoeLogAdapter::new()),
                Arc::new(Sc2ClientAdapter::new()),
            ],
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
    fn registry_finds_shipped_adapters() {
        let reg = AdapterRegistry::with_defaults();
        assert!(reg.get("counter-strike-2").is_some());
        assert!(reg.get("dota-2").is_some());
        assert!(reg.get("league-of-legends").is_some());
        assert!(reg.get("rocket-league").is_some());
        assert!(reg.get("legends-of-runeterra").is_some());
        assert!(reg.get("hearthstone").is_some());
        assert!(reg.get("minecraft").is_some());
        assert!(reg.get("path-of-exile").is_some());
        assert!(reg.get("starcraft-2").is_some());
        assert!(reg.get("unknown-game").is_none());
        assert_eq!(reg.all().len(), 9);
    }

    #[test]
    fn generated_token_is_nonempty() {
        let t = generate_token();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
