//! Counter-Strike 2 Game State Integration (GSI) adapter.
//!
//! CS2 posts its full subscribed state to a local HTTP endpoint whenever
//! something changes (Valve-supported, no injection). We install a config file
//! into CS2's `cfg` directory pointing at our loopback listener, then derive
//! match outcomes from the `map.phase` lifecycle.

use std::sync::Mutex;

use serde::Deserialize;

use super::{
    GameTelemetryAdapter, MatchResult, Outcome, Performance, SourceQuality, TelemetryError,
    TelemetryEvent,
};

/// Game DB id (matches `client/assets/games.json`).
const GAME_ID: &str = "counter-strike-2";

/// CS2/CS:GO share Steam app id 730.
const CS2_APPID: i64 = 730;

/// Modes whose results move win/loss streaks. Other modes (casual, deathmatch,
/// arms race, …) are "played only" and produce no match outcomes.
fn is_streak_mode(mode: &str) -> bool {
    matches!(
        mode.to_ascii_lowercase().as_str(),
        "competitive" | "premier" | "scrimcomp2v2" | "wingman"
    )
}

/// CS2 GSI adapter. Holds the small amount of cross-payload state needed to turn
/// the `map.phase` lifecycle into discrete match events.
pub struct Cs2GsiAdapter {
    state: Mutex<Cs2State>,
}

#[derive(Default)]
struct Cs2State {
    match_active: bool,
    last_ct: u32,
    last_t: u32,
    /// Mode/map captured at match start, used to finalize an abandoned match.
    mode: String,
    map_name: String,
}

impl Cs2GsiAdapter {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(Cs2State::default()),
        }
    }
}

impl Default for Cs2GsiAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GameTelemetryAdapter for Cs2GsiAdapter {
    fn game_id(&self) -> &str {
        GAME_ID
    }

    fn ensure_installed(&self, token: &str, port: u16) -> Result<(), TelemetryError> {
        install_cfg(token, port)
    }

    fn reset(&self) {
        *self.state.lock().expect("cs2 telemetry state poisoned") = Cs2State::default();
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
        // that positively identify as CS2 are ours. Our cfg subscribes to
        // `provider`, so real CS2 payloads always carry the appid.
        match &payload.provider {
            Some(p) if p.appid == CS2_APPID => {}
            _ => return Vec::new(),
        }

        let map = match &payload.map {
            Some(m) => m,
            None => {
                // No map block = menus/loading. If a match was in flight the
                // player abandoned or disconnected: finalize it as Incomplete
                // (never a loss) so the next match starts from clean state.
                let mut st = self.state.lock().expect("cs2 telemetry state poisoned");
                if st.match_active {
                    let ended = TelemetryEvent::MatchEnded(Box::new(MatchResult {
                        game_id: GAME_ID.to_string(),
                        mode: std::mem::take(&mut st.mode),
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
                    *st = Cs2State::default();
                    return vec![ended];
                }
                return Vec::new();
            }
        };

        let mut st = self.state.lock().expect("cs2 telemetry state poisoned");

        // Non-streak modes (casual, DM, …): track nothing, emit nothing.
        // Spec 18 §3.2 records this as the v1 decision: non-streak modes are
        // "played only" at the process level, with no match outcomes.
        if !is_streak_mode(&map.mode) {
            *st = Cs2State::default();
            return Vec::new();
        }

        let mut events = Vec::new();
        let phase = map.phase.as_str();
        let ct = map.team_ct.score;
        let t = map.team_t.score;

        // Match start: entering warmup/live while no match is active (covers a
        // fresh match and connecting mid-match after a restart).
        if matches!(phase, "warmup" | "live") && !st.match_active {
            st.match_active = true;
            st.last_ct = 0;
            st.last_t = 0;
            st.mode = map.mode.clone();
            st.map_name = map.name.clone();
            events.push(TelemetryEvent::MatchStarted {
                mode: map.mode.clone(),
                map: map.name.clone(),
            });
        }

        let player_team = payload
            .player
            .as_ref()
            .map(|p| p.team.as_str())
            .unwrap_or("");

        // Round resolved: the live score changed. Player perspective when the
        // side is known; unknown side falls back to (leading, trailing).
        if st.match_active && (ct != st.last_ct || t != st.last_t) {
            st.last_ct = ct;
            st.last_t = t;
            let (own, opp) = split_scores(player_team, ct, t);
            events.push(TelemetryEvent::ScoreChanged { own, opp });
        }

        // Match over: derive the outcome from the player's current side.
        if phase == "gameover" && st.match_active {
            st.match_active = false;
            let result = derive_outcome(player_team, ct, t);
            let (own_score, opp_score) = split_scores(player_team, ct, t);
            let performance = payload
                .player
                .as_ref()
                .and_then(|p| p.match_stats.as_ref())
                .map(|ms| Performance {
                    kills: Some(ms.kills.max(0) as u32),
                    deaths: Some(ms.deaths.max(0) as u32),
                    assists: Some(ms.assists.max(0) as u32),
                    mvps: Some(ms.mvps.max(0) as u32),
                    score: Some(ms.score.max(0) as u32),
                    ..Performance::default()
                });
            events.push(TelemetryEvent::MatchEnded(Box::new(MatchResult {
                game_id: GAME_ID.to_string(),
                mode: map.mode.clone(),
                map: map.name.clone(),
                result,
                streak_eligible: true,
                own_score,
                opp_score,
                performance,
                build: None,
                run: None,
                source: SourceQuality::Live,
                ts: now_ms(),
            })));
        }

        events
    }
}

/// Split team scores into (own, opp) by the player's side; unknown side falls
/// back to (leading, trailing) so the numbers stay meaningful for display.
fn split_scores(player_team: &str, ct: u32, t: u32) -> (u32, u32) {
    match player_team {
        "CT" => (ct, t),
        "T" => (t, ct),
        _ => (ct.max(t), ct.min(t)),
    }
}

/// Determine win/loss/draw from the player's side and the final team scores.
fn derive_outcome(player_team: &str, ct: u32, t: u32) -> Outcome {
    let (own, opp) = match player_team {
        "CT" => (ct, t),
        "T" => (t, ct),
        _ => return Outcome::Incomplete, // unknown side → can't attribute
    };
    use std::cmp::Ordering;
    match own.cmp(&opp) {
        Ordering::Greater => Outcome::Win,
        Ordering::Less => Outcome::Loss,
        Ordering::Equal => Outcome::Draw,
    }
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
    mode: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    phase: String,
    #[serde(default)]
    team_ct: TeamState,
    #[serde(default)]
    team_t: TeamState,
}

#[derive(Deserialize, Default)]
struct TeamState {
    #[serde(default)]
    score: u32,
}

#[derive(Deserialize, Default)]
struct PlayerState {
    #[serde(default)]
    team: String,
    match_stats: Option<MatchStats>,
}

/// `player.match_stats` from the `player_match_stats` subscription. Values can
/// go negative in-game (suicides/team kills), hence signed.
#[derive(Deserialize, Default)]
struct MatchStats {
    #[serde(default)]
    kills: i32,
    #[serde(default)]
    assists: i32,
    #[serde(default)]
    deaths: i32,
    #[serde(default)]
    mvps: i32,
    #[serde(default)]
    score: i32,
}

#[derive(Deserialize, Default)]
struct AuthState {
    #[serde(default)]
    token: String,
}

// ---------------------------------------------------------------------------
// Config installation (Windows-first; other platforms unsupported for now).
// ---------------------------------------------------------------------------

/// The GSI config file contents pointing CS2 at our listener.
/// Windows-only for now; config installation is gated to Windows (spec 17/18
/// are Windows-first). macOS/Linux paths follow when those backends land.
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
        "provider"           "1"
        "map"                "1"
        "round"              "1"
        "player_id"          "1"
        "player_state"       "1"
        "player_match_stats" "1"
    }}
}}
"#
    )
}

#[cfg(windows)]
const CFG_FILE_NAME: &str = "gamestate_integration_mello.cfg";

#[cfg(windows)]
fn install_cfg(token: &str, port: u16) -> Result<(), TelemetryError> {
    let cfg_dir = cs2_cfg_dir()?;
    let cfg_path = cfg_dir.join(CFG_FILE_NAME);
    let desired = render_cfg(token, port);

    // Idempotent: only write when missing or contents changed (token/port).
    let current = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    if current != desired {
        std::fs::write(&cfg_path, desired)?;
        log::info!(
            "[telemetry] installed CS2 GSI cfg at {}",
            cfg_path.display()
        );
    } else {
        log::debug!("[telemetry] CS2 GSI cfg already up to date");
    }
    Ok(())
}

#[cfg(not(windows))]
fn install_cfg(_token: &str, _port: u16) -> Result<(), TelemetryError> {
    Err(TelemetryError::Unsupported)
}

/// Locate `…/Counter-Strike Global Offensive/game/csgo/cfg`, searching every
/// Steam library folder. CS2 still lives under the legacy CS:GO directory name.
#[cfg(windows)]
fn cs2_cfg_dir() -> Result<std::path::PathBuf, TelemetryError> {
    use std::path::PathBuf;
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let steam_path: String = hkcu
        .open_subkey("Software\\Valve\\Steam")
        .ok()
        .and_then(|k| k.get_value("SteamPath").ok())
        .ok_or_else(|| TelemetryError::GameNotFound("Steam not found in registry".into()))?;

    let steam_root = PathBuf::from(steam_path);

    // Candidate libraries: the Steam root plus any extra library folders.
    let mut libraries = vec![steam_root.clone()];
    let lib_vdf = steam_root.join("steamapps").join("libraryfolders.vdf");
    if let Ok(contents) = std::fs::read_to_string(&lib_vdf) {
        libraries.extend(parse_library_paths(&contents));
    }

    for lib in libraries {
        let cfg = lib
            .join("steamapps")
            .join("common")
            .join("Counter-Strike Global Offensive")
            .join("game")
            .join("csgo")
            .join("cfg");
        if cfg.is_dir() {
            return Ok(cfg);
        }
    }

    Err(TelemetryError::GameNotFound(
        "CS2 install (app 730) not found in any Steam library".into(),
    ))
}

/// Extract `"path"` values from a `libraryfolders.vdf`. Minimal VDF handling:
/// each library object has a `"path"  "<dir>"` line with `\\`-escaped separators.
#[cfg(windows)]
fn parse_library_paths(vdf: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for line in vdf.lines() {
        let line = line.trim();
        let lower = line.to_ascii_lowercase();
        if !lower.starts_with("\"path\"") {
            continue;
        }
        // Take the second quoted token on the line.
        let mut parts = line.split('"').filter(|s| !s.trim().is_empty());
        let _key = parts.next(); // "path"
        if let Some(raw) = parts.next() {
            out.push(std::path::PathBuf::from(raw.replace("\\\\", "\\")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> Cs2GsiAdapter {
        Cs2GsiAdapter::new()
    }

    const TOKEN: &str = "secrettoken";

    fn payload(mode: &str, phase: &str, ct: u32, t: u32, team: &str) -> String {
        format!(
            r#"{{
                "provider": {{ "appid": 730 }},
                "auth": {{ "token": "{TOKEN}" }},
                "map": {{ "mode": "{mode}", "name": "de_mirage", "phase": "{phase}",
                          "team_ct": {{ "score": {ct} }}, "team_t": {{ "score": {t} }} }},
                "player": {{ "team": "{team}",
                             "match_stats": {{ "kills": 21, "assists": 4, "deaths": 15,
                                               "mvps": 3, "score": 47 }} }}
            }}"#
        )
    }

    #[test]
    fn streak_mode_classification() {
        assert!(is_streak_mode("competitive"));
        assert!(is_streak_mode("Premier"));
        assert!(is_streak_mode("scrimcomp2v2"));
        assert!(!is_streak_mode("casual"));
        assert!(!is_streak_mode("deathmatch"));
    }

    #[test]
    fn rejects_wrong_token() {
        let a = adapter();
        let body = payload("competitive", "live", 1, 0, "CT");
        assert!(a.parse(&body, "different-token").is_empty());
    }

    #[test]
    fn rejects_non_cs_appid() {
        let a = adapter();
        let body = r#"{ "provider": { "appid": 570 }, "auth": { "token": "secrettoken" },
                        "map": { "mode": "competitive", "phase": "live", "team_ct": {"score":1}, "team_t": {"score":0} } }"#;
        assert!(a.parse(body, TOKEN).is_empty());
    }

    #[test]
    fn garbage_payload_no_events() {
        let a = adapter();
        assert!(a.parse("not json", TOKEN).is_empty());
        assert!(a.parse("{}", TOKEN).is_empty()); // no auth → rejected
    }

    #[test]
    fn rejects_payload_without_provider() {
        // Strict self-identification: no provider block → not ours.
        let a = adapter();
        let body = format!(
            r#"{{ "auth": {{ "token": "{TOKEN}" }},
                  "map": {{ "mode": "competitive", "phase": "live",
                            "team_ct": {{"score":1}}, "team_t": {{"score":0}} }} }}"#
        );
        assert!(a.parse(&body, TOKEN).is_empty());
    }

    #[test]
    fn casual_mode_emits_nothing() {
        let a = adapter();
        let body = payload("casual", "live", 5, 3, "CT");
        assert!(a.parse(&body, TOKEN).is_empty());
    }

    #[test]
    fn match_start_then_win() {
        let a = adapter();

        // First live payload → MatchStarted + ScoreChanged for the current score.
        let evs = a.parse(&payload("competitive", "live", 1, 0, "CT"), TOKEN);
        assert!(matches!(evs[0], TelemetryEvent::MatchStarted { .. }));
        assert!(evs
            .iter()
            .any(|e| matches!(e, TelemetryEvent::ScoreChanged { own: 1, opp: 0 })));

        // Game over, player on CT with the higher score → Win.
        let evs = a.parse(&payload("competitive", "gameover", 13, 7, "CT"), TOKEN);
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .expect("expected MatchEnded");
        assert_eq!(ended.result, Outcome::Win);
        assert_eq!(ended.own_score, 13);
        assert_eq!(ended.opp_score, 7);
        assert!(ended.streak_eligible);
        assert_eq!(ended.source, SourceQuality::Live);

        // player_match_stats flows into the performance slot.
        let perf = ended.performance.as_ref().expect("expected performance");
        assert_eq!(perf.kills, Some(21));
        assert_eq!(perf.deaths, Some(15));
        assert_eq!(perf.assists, Some(4));
        assert_eq!(perf.mvps, Some(3));
        assert_eq!(perf.score, Some(47));
        assert_eq!(perf.damage, None);
    }

    #[test]
    fn gameover_without_match_stats_has_no_performance() {
        let a = adapter();
        // Payload without the match_stats block (older cfg / partial payload).
        let body = format!(
            r#"{{
                "provider": {{ "appid": 730 }},
                "auth": {{ "token": "{TOKEN}" }},
                "map": {{ "mode": "competitive", "phase": "gameover",
                          "team_ct": {{ "score": 13 }}, "team_t": {{ "score": 7 }} }},
                "player": {{ "team": "CT" }}
            }}"#
        );
        a.parse(&payload("competitive", "live", 1, 0, "CT"), TOKEN);
        let evs = a.parse(&body, TOKEN);
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .expect("expected MatchEnded");
        assert_eq!(ended.result, Outcome::Win);
        assert!(ended.performance.is_none());
    }

    #[test]
    fn loss_when_player_side_behind() {
        let a = adapter();
        a.parse(&payload("competitive", "live", 0, 1, "T"), TOKEN);
        let evs = a.parse(&payload("competitive", "gameover", 13, 9, "T"), TOKEN);
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .unwrap();
        // Player on T (9) vs CT (13) → Loss.
        assert_eq!(ended.result, Outcome::Loss);
        assert_eq!(ended.own_score, 9);
        assert_eq!(ended.opp_score, 13);
    }

    #[test]
    fn halftime_side_switch_uses_final_side() {
        let a = adapter();
        // Started on T.
        a.parse(&payload("competitive", "live", 0, 3, "T"), TOKEN);
        // After the switch the player is CT and that side wins 13-11.
        let evs = a.parse(&payload("competitive", "gameover", 13, 11, "CT"), TOKEN);
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .unwrap();
        assert_eq!(ended.result, Outcome::Win);
    }

    /// A payload with no `map` block, as CS2 sends from the main menu.
    fn menu_payload() -> String {
        format!(
            r#"{{
                "provider": {{ "appid": 730 }},
                "auth": {{ "token": "{TOKEN}" }},
                "player": {{ "team": "" }}
            }}"#
        )
    }

    #[test]
    fn abandoned_match_finalizes_incomplete() {
        let a = adapter();
        a.parse(&payload("competitive", "live", 3, 5, "T"), TOKEN);

        // Player leaves to the main menu mid-match.
        let evs = a.parse(&menu_payload(), TOKEN);
        let ended = evs
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::MatchEnded(m) => Some(m),
                _ => None,
            })
            .expect("expected Incomplete MatchEnded on abandon");
        assert_eq!(ended.result, Outcome::Incomplete);
        assert_eq!(ended.mode, "competitive");
        assert_eq!(ended.map, "de_mirage");

        // Further menu payloads are quiet; the next match starts fresh.
        assert!(a.parse(&menu_payload(), TOKEN).is_empty());
        let evs = a.parse(&payload("competitive", "live", 0, 0, "CT"), TOKEN);
        assert!(matches!(evs[0], TelemetryEvent::MatchStarted { .. }));
    }

    #[test]
    fn menu_payload_without_match_is_quiet() {
        let a = adapter();
        assert!(a.parse(&menu_payload(), TOKEN).is_empty());
    }

    #[test]
    fn second_match_starts_after_gameover() {
        let a = adapter();
        a.parse(&payload("competitive", "live", 1, 0, "CT"), TOKEN);
        a.parse(&payload("competitive", "gameover", 13, 5, "CT"), TOKEN);
        // New match begins.
        let evs = a.parse(&payload("competitive", "warmup", 0, 0, "CT"), TOKEN);
        assert!(matches!(evs[0], TelemetryEvent::MatchStarted { .. }));
    }

    #[cfg(windows)]
    #[test]
    fn parse_library_paths_extracts_dirs() {
        let vdf = r#"
"libraryfolders"
{
    "0"
    {
        "path"    "C:\\Program Files (x86)\\Steam"
        "apps" { "730" "1234" }
    }
    "1"
    {
        "path"    "D:\\SteamLibrary"
    }
}
"#;
        let paths = parse_library_paths(vdf);
        assert_eq!(paths.len(), 2);
        assert_eq!(
            paths[0],
            std::path::PathBuf::from("C:\\Program Files (x86)\\Steam")
        );
        assert_eq!(paths[1], std::path::PathBuf::from("D:\\SteamLibrary"));
    }

    #[cfg(windows)]
    #[test]
    fn rendered_cfg_contains_token_and_port() {
        let cfg = render_cfg("abc123", 29406);
        assert!(cfg.contains("abc123"));
        assert!(cfg.contains("29406"));
        assert!(cfg.contains("player_match_stats"));
    }
}
