//! Dota 2 Game State Integration adapter.
//!
//! Same push mechanism as CS2 (spec 18 §3): the game POSTs JSON snapshots to
//! our loopback listener. Two Dota-specific differences:
//!
//! - The cfg lives in `game/dota/cfg/gamestate_integration/` and only takes
//!   effect with the `-gamestateintegration` launch option. We never edit
//!   Steam's `localconfig.vdf` to set it; instead `start()` emits a
//!   [`TelemetryEvent::SetupRequired`] hint when the flag looks absent.
//! - Match state comes from `map.game_state` (`DOTA_GAMERULES_STATE_*`) and
//!   the winner from `map.win_team`; the kill score is `radiant_score` /
//!   `dire_score` oriented by `player.team_name`.

use std::sync::Mutex;

use serde::Deserialize;

use super::{
    BuildInfo, GameTelemetryAdapter, MatchResult, Outcome, Performance, SourceQuality,
    TelemetryError, TelemetryEvent,
};

const GAME_ID: &str = "dota-2";
const DOTA2_APPID: i64 = 570;
#[cfg(windows)]
const LAUNCH_OPTION: &str = "-gamestateintegration";

/// In-progress-ish game states that mean a real match is running or loading.
const ACTIVE_STATES: [&str; 5] = [
    "DOTA_GAMERULES_STATE_HERO_SELECTION",
    "DOTA_GAMERULES_STATE_STRATEGY_TIME",
    "DOTA_GAMERULES_STATE_TEAM_SHOWCASE",
    "DOTA_GAMERULES_STATE_PRE_GAME",
    "DOTA_GAMERULES_STATE_GAME_IN_PROGRESS",
];

#[derive(Default)]
struct Dota2State {
    match_active: bool,
    last_own: u32,
    last_opp: u32,
    /// Map captured at match start, used to finalize an abandoned match.
    map_name: String,
}

pub struct Dota2GsiAdapter {
    state: Mutex<Dota2State>,
}

impl Dota2GsiAdapter {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(Dota2State::default()),
        }
    }
}

impl Default for Dota2GsiAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GameTelemetryAdapter for Dota2GsiAdapter {
    fn game_id(&self) -> &str {
        GAME_ID
    }

    fn info(&self) -> super::AdapterInfo {
        super::AdapterInfo {
            game_name: "Dota 2",
            writes_files: true,
            note: "Adds a Game State Integration config to the Dota 2 folder. Needs the -gamestateintegration launch option in Steam.",
            account_link: None,
        }
    }

    fn detect_install(&self) -> Option<bool> {
        #[cfg(windows)]
        {
            Some(super::steam::find_app_subdir("dota 2 beta", &["game", "dota", "cfg"], "").is_ok())
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    fn ensure_installed(&self, token: &str, port: u16) -> Result<(), TelemetryError> {
        install_cfg(token, port)
    }

    fn start(&self, tx: std::sync::mpsc::Sender<TelemetryEvent>) {
        // Push source: no transport to spawn. But GSI is dead without the
        // launch option, so surface the one-time setup step when it looks
        // absent (read-only heuristic against Steam's localconfig.vdf).
        #[cfg(windows)]
        if !super::steam::any_localconfig_contains(LAUNCH_OPTION) {
            let _ = tx.send(TelemetryEvent::SetupRequired {
                game_id: GAME_ID.to_string(),
                hint: format!("Add {LAUNCH_OPTION} to Dota 2's Steam launch options to track wins"),
            });
        }
        #[cfg(not(windows))]
        let _ = tx;
    }

    fn reset(&self) {
        *self.state.lock().expect("dota2 telemetry state poisoned") = Dota2State::default();
    }

    fn parse(&self, body: &str, token: &str) -> Vec<TelemetryEvent> {
        let payload: GsiPayload = match serde_json::from_str(body) {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };

        // Reject payloads that don't carry our per-install token.
        match &payload.auth {
            Some(a) if a.token == token && !token.is_empty() => {}
            _ => return Vec::new(),
        }

        // Routing contract (see GameTelemetryAdapter::parse): only payloads
        // that positively identify as Dota 2 are ours.
        match &payload.provider {
            Some(p) if p.appid == DOTA2_APPID => {}
            _ => return Vec::new(),
        }

        let map = match &payload.map {
            Some(m) => m,
            None => {
                // No map block = menus. An in-flight match was abandoned or
                // disconnected: finalize as Incomplete (never a loss).
                let mut st = self.state.lock().expect("dota2 telemetry state poisoned");
                if st.match_active {
                    let ended = TelemetryEvent::MatchEnded(Box::new(MatchResult {
                        game_id: GAME_ID.to_string(),
                        mode: "match".to_string(),
                        map: std::mem::take(&mut st.map_name),
                        result: Outcome::Incomplete,
                        streak_eligible: true,
                        own_score: 0,
                        opp_score: 0,
                        performance: None,
                        build: None,
                        run: None,
                        source: SourceQuality::Live,
                        ts: now_ms(),
                    }));
                    *st = Dota2State::default();
                    return vec![ended];
                }
                return Vec::new();
            }
        };

        let mut st = self.state.lock().expect("dota2 telemetry state poisoned");

        // Custom games (arcade) have arbitrary rules; track nothing.
        if !map.customgamename.is_empty() {
            *st = Dota2State::default();
            return Vec::new();
        }

        let mut events = Vec::new();
        let game_state = map.game_state.as_str();
        let player_team = payload
            .player
            .as_ref()
            .map(|p| p.team_name.as_str())
            .unwrap_or("");
        let (own, opp) = split_scores(player_team, map.radiant_score, map.dire_score);

        // Match start: any active game state while no match is tracked (covers
        // a fresh match and reconnecting mid-game after a client restart).
        if ACTIVE_STATES.contains(&game_state) && !st.match_active {
            st.match_active = true;
            st.last_own = 0;
            st.last_opp = 0;
            st.map_name = map.name.clone();
            events.push(TelemetryEvent::MatchStarted {
                mode: "match".to_string(),
                map: map.name.clone(),
            });
        }

        // Kill score changed.
        if st.match_active && (own != st.last_own || opp != st.last_opp) {
            st.last_own = own;
            st.last_opp = opp;
            events.push(TelemetryEvent::ScoreChanged { own, opp });
        }

        // Post-game with a declared winner → derive the outcome.
        if game_state == "DOTA_GAMERULES_STATE_POST_GAME" && st.match_active {
            st.match_active = false;
            let result = derive_outcome(player_team, &map.win_team);
            let performance = payload.player.as_ref().map(|p| Performance {
                kills: Some(p.kills.max(0) as u32),
                deaths: Some(p.deaths.max(0) as u32),
                assists: Some(p.assists.max(0) as u32),
                cs: Some(p.last_hits.max(0) as u32),
                ..Performance::default()
            });
            let build = payload
                .hero
                .as_ref()
                .filter(|h| !h.name.is_empty())
                .map(|h| BuildInfo {
                    character: Some(display_hero_name(&h.name)),
                    ..BuildInfo::default()
                });
            events.push(TelemetryEvent::MatchEnded(Box::new(MatchResult {
                game_id: GAME_ID.to_string(),
                mode: "match".to_string(),
                map: map.name.clone(),
                result,
                streak_eligible: true,
                own_score: own,
                opp_score: opp,
                performance,
                build,
                run: None,
                source: SourceQuality::Live,
                ts: now_ms(),
            })));
        }

        events
    }
}

/// Orient (radiant, dire) kill scores to the player's side; unknown side falls
/// back to (leading, trailing) so the numbers stay meaningful for display.
fn split_scores(player_team: &str, radiant: u32, dire: u32) -> (u32, u32) {
    match player_team {
        "radiant" => (radiant, dire),
        "dire" => (dire, radiant),
        _ => (radiant.max(dire), radiant.min(dire)),
    }
}

/// Win/loss from `map.win_team` vs the player's side; unknown side (or no
/// declared winner) can't be attributed.
fn derive_outcome(player_team: &str, win_team: &str) -> Outcome {
    if win_team.is_empty() || !matches!(player_team, "radiant" | "dire") {
        return Outcome::Incomplete;
    }
    if win_team.eq_ignore_ascii_case(player_team) {
        Outcome::Win
    } else {
        Outcome::Loss
    }
}

/// "npc_dota_hero_shadow_shaman" → "shadow shaman".
fn display_hero_name(internal: &str) -> String {
    internal
        .strip_prefix("npc_dota_hero_")
        .unwrap_or(internal)
        .replace('_', " ")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ---------------------------------------------------------------------------
// GSI payload (only the fields we subscribe to; everything optional/defaulted
// so partial or malformed payloads degrade to "no events" rather than errors).
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct GsiPayload {
    provider: Option<Provider>,
    map: Option<MapState>,
    player: Option<PlayerState>,
    hero: Option<HeroState>,
    auth: Option<AuthState>,
}

#[derive(Deserialize, Default)]
struct Provider {
    #[serde(default)]
    appid: i64,
}

#[derive(Deserialize, Default)]
struct MapState {
    #[serde(default)]
    name: String,
    #[serde(default)]
    game_state: String,
    /// "radiant" | "dire" | "none" (empty until the game ends).
    #[serde(default)]
    win_team: String,
    #[serde(default)]
    customgamename: String,
    #[serde(default)]
    radiant_score: u32,
    #[serde(default)]
    dire_score: u32,
}

#[derive(Deserialize, Default)]
struct PlayerState {
    /// "radiant" | "dire".
    #[serde(default)]
    team_name: String,
    #[serde(default)]
    kills: i32,
    #[serde(default)]
    deaths: i32,
    #[serde(default)]
    assists: i32,
    #[serde(default)]
    last_hits: i32,
}

#[derive(Deserialize, Default)]
struct HeroState {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
struct AuthState {
    #[serde(default)]
    token: String,
}

// ---------------------------------------------------------------------------
// Config installation (Windows-first, mirroring the CS2 adapter).
// ---------------------------------------------------------------------------

/// The GSI config file contents pointing Dota 2 at our listener.
#[cfg(windows)]
fn render_cfg(token: &str, port: u16) -> String {
    format!(
        r#""Mello Game State Integration v1"
{{
    "uri"     "http://127.0.0.1:{port}"
    "timeout" "5.0"
    "auth"
    {{
        "token" "{token}"
    }}
    "data"
    {{
        "provider" "1"
        "map"      "1"
        "player"   "1"
        "hero"     "1"
    }}
}}
"#
    )
}

#[cfg(windows)]
const CFG_FILE_NAME: &str = "gamestate_integration_mello.cfg";

#[cfg(windows)]
fn install_cfg(token: &str, port: u16) -> Result<(), TelemetryError> {
    // Unlike CS2, Dota requires cfgs under a `gamestate_integration/` subdir
    // (which doesn't exist until some integration creates it).
    let gsi_dir = super::steam::find_app_subdir(
        "dota 2 beta",
        &["game", "dota", "cfg"],
        "Dota 2 install (app 570) not found in any Steam library",
    )?
    .join("gamestate_integration");
    std::fs::create_dir_all(&gsi_dir)?;

    let cfg_path = gsi_dir.join(CFG_FILE_NAME);
    let desired = render_cfg(token, port);

    // Idempotent: only write when missing or contents changed (token/port).
    let current = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    if current != desired {
        std::fs::write(&cfg_path, desired)?;
        log::info!(
            "[telemetry] installed Dota 2 GSI cfg at {}",
            cfg_path.display()
        );
    } else {
        log::debug!("[telemetry] Dota 2 GSI cfg already up to date");
    }
    Ok(())
}

#[cfg(not(windows))]
fn install_cfg(_token: &str, _port: u16) -> Result<(), TelemetryError> {
    Err(TelemetryError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> Dota2GsiAdapter {
        Dota2GsiAdapter::new()
    }

    const TOKEN: &str = "secrettoken";

    fn payload(game_state: &str, win_team: &str, radiant: u32, dire: u32, team: &str) -> String {
        format!(
            r#"{{
                "provider": {{ "appid": 570 }},
                "auth": {{ "token": "{TOKEN}" }},
                "map": {{ "name": "dota", "game_state": "{game_state}",
                          "win_team": "{win_team}", "customgamename": "",
                          "radiant_score": {radiant}, "dire_score": {dire} }},
                "player": {{ "team_name": "{team}", "kills": 9, "deaths": 4,
                             "assists": 17, "last_hits": 212 }},
                "hero": {{ "name": "npc_dota_hero_shadow_shaman" }}
            }}"#
        )
    }

    /// Menu payload: provider only, no map (what Dota sends outside a match).
    fn menu_payload() -> String {
        format!(
            r#"{{
                "provider": {{ "appid": 570 }},
                "auth": {{ "token": "{TOKEN}" }}
            }}"#
        )
    }

    #[test]
    fn match_start_then_win_with_stats() {
        let a = adapter();

        let evs = a.parse(
            &payload(
                "DOTA_GAMERULES_STATE_GAME_IN_PROGRESS",
                "none",
                5,
                3,
                "radiant",
            ),
            TOKEN,
        );
        assert!(matches!(evs[0], TelemetryEvent::MatchStarted { .. }));
        assert!(evs
            .iter()
            .any(|e| matches!(e, TelemetryEvent::ScoreChanged { own: 5, opp: 3 })));

        let evs = a.parse(
            &payload(
                "DOTA_GAMERULES_STATE_POST_GAME",
                "radiant",
                41,
                22,
                "radiant",
            ),
            TOKEN,
        );
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .expect("expected MatchEnded");
        assert_eq!(ended.result, Outcome::Win);
        assert_eq!(ended.own_score, 41);
        assert_eq!(ended.opp_score, 22);
        assert!(ended.streak_eligible);

        let perf = ended.performance.as_ref().expect("expected performance");
        assert_eq!(perf.kills, Some(9));
        assert_eq!(perf.deaths, Some(4));
        assert_eq!(perf.assists, Some(17));
        assert_eq!(perf.cs, Some(212));

        let build = ended.build.as_ref().expect("expected build");
        assert_eq!(build.character.as_deref(), Some("shadow shaman"));
    }

    #[test]
    fn loss_when_other_team_wins() {
        let a = adapter();
        a.parse(
            &payload(
                "DOTA_GAMERULES_STATE_GAME_IN_PROGRESS",
                "none",
                1,
                0,
                "dire",
            ),
            TOKEN,
        );
        let evs = a.parse(
            &payload("DOTA_GAMERULES_STATE_POST_GAME", "radiant", 40, 18, "dire"),
            TOKEN,
        );
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .expect("expected MatchEnded");
        assert_eq!(ended.result, Outcome::Loss);
        // Dire perspective: own 18, opp 40.
        assert_eq!(ended.own_score, 18);
        assert_eq!(ended.opp_score, 40);
    }

    #[test]
    fn abandoned_match_finalizes_incomplete() {
        let a = adapter();
        a.parse(
            &payload(
                "DOTA_GAMERULES_STATE_GAME_IN_PROGRESS",
                "none",
                10,
                12,
                "radiant",
            ),
            TOKEN,
        );

        let evs = a.parse(&menu_payload(), TOKEN);
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .expect("expected Incomplete MatchEnded on abandon");
        assert_eq!(ended.result, Outcome::Incomplete);
        assert!(a.parse(&menu_payload(), TOKEN).is_empty());
    }

    #[test]
    fn custom_games_emit_nothing() {
        let a = adapter();
        let body = format!(
            r#"{{
                "provider": {{ "appid": 570 }},
                "auth": {{ "token": "{TOKEN}" }},
                "map": {{ "name": "dota", "game_state": "DOTA_GAMERULES_STATE_GAME_IN_PROGRESS",
                          "win_team": "none", "customgamename": "dota_auto_chess",
                          "radiant_score": 0, "dire_score": 0 }}
            }}"#
        );
        assert!(a.parse(&body, TOKEN).is_empty());
    }

    #[test]
    fn rejects_cs2_payload() {
        // Strict self-identification: CS2's appid is not ours.
        let a = adapter();
        let body = format!(
            r#"{{
                "provider": {{ "appid": 730 }},
                "auth": {{ "token": "{TOKEN}" }},
                "map": {{ "name": "de_mirage", "game_state": "DOTA_GAMERULES_STATE_GAME_IN_PROGRESS" }}
            }}"#
        );
        assert!(a.parse(&body, TOKEN).is_empty());
    }

    #[test]
    fn rejects_wrong_token() {
        let a = adapter();
        let body = payload(
            "DOTA_GAMERULES_STATE_GAME_IN_PROGRESS",
            "none",
            0,
            0,
            "radiant",
        );
        assert!(a.parse(&body, "other-token").is_empty());
    }

    #[test]
    fn hero_name_prettified() {
        assert_eq!(display_hero_name("npc_dota_hero_axe"), "axe");
        assert_eq!(
            display_hero_name("npc_dota_hero_shadow_shaman"),
            "shadow shaman"
        );
        assert_eq!(display_hero_name("weird"), "weird");
    }

    #[cfg(windows)]
    #[test]
    fn rendered_cfg_contains_token_and_port() {
        let cfg = render_cfg("abc123", 29406);
        assert!(cfg.contains("abc123"));
        assert!(cfg.contains("29406"));
        assert!(cfg.contains("hero"));
    }
}
