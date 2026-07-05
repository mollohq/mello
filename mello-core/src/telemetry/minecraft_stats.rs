//! Minecraft (Java Edition) world-stats adapter.
//!
//! Singleplayer/LAN worlds keep per-player statistics in
//! `saves/<world>/stats/<uuid>.json`, updated when the world saves (including
//! on exit). This adapter snapshots the numbers at game start and diffs them
//! at `reset()` (game exit — after the final world save, before the client
//! folds the session), emitting one played-only `MatchEnded` with the session
//! delta: playtime, mob kills, deaths, blocks mined/placed. Third-party
//! servers store stats server-side, so those sessions quietly contribute
//! nothing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Mutex;

use super::{
    GameTelemetryAdapter, MatchResult, Outcome, Performance, RunInfo, SourceQuality,
    TelemetryError, TelemetryEvent,
};

const GAME_ID: &str = "minecraft";

/// The counters we track per stats file.
#[derive(Default, Clone, Copy, PartialEq)]
struct WorldStats {
    /// `minecraft:custom/minecraft:play_time`, in ticks (20/s).
    play_time_ticks: u64,
    deaths: u64,
    mob_kills: u64,
    blocks_mined: u64,
    blocks_placed: u64,
}

impl WorldStats {
    fn delta(&self, baseline: &WorldStats) -> WorldStats {
        WorldStats {
            play_time_ticks: self
                .play_time_ticks
                .saturating_sub(baseline.play_time_ticks),
            deaths: self.deaths.saturating_sub(baseline.deaths),
            mob_kills: self.mob_kills.saturating_sub(baseline.mob_kills),
            blocks_mined: self.blocks_mined.saturating_sub(baseline.blocks_mined),
            blocks_placed: self.blocks_placed.saturating_sub(baseline.blocks_placed),
        }
    }

    fn is_empty(&self) -> bool {
        *self == WorldStats::default()
    }
}

pub struct MinecraftStatsAdapter {
    running: Mutex<bool>,
    /// stats-file path → counters at game start.
    baseline: Mutex<HashMap<PathBuf, WorldStats>>,
    tx: Mutex<Option<Sender<TelemetryEvent>>>,
}

impl MinecraftStatsAdapter {
    pub fn new() -> Self {
        Self {
            running: Mutex::new(false),
            baseline: Mutex::new(HashMap::new()),
            tx: Mutex::new(None),
        }
    }
}

impl Default for MinecraftStatsAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GameTelemetryAdapter for MinecraftStatsAdapter {
    fn game_id(&self) -> &str {
        GAME_ID
    }

    fn info(&self) -> super::AdapterInfo {
        super::AdapterInfo {
            game_name: "Minecraft",
            writes_files: false,
            note: "Reads your singleplayer world statistics before and after a session. Nothing is installed.",
            account_link: None,
        }
    }

    fn detect_install(&self) -> Option<bool> {
        #[cfg(windows)]
        {
            Some(saves_dir().is_some())
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    fn ensure_installed(&self, _token: &str, _port: u16) -> Result<(), TelemetryError> {
        Ok(())
    }

    fn start(&self, tx: Sender<TelemetryEvent>) {
        let mut running = self.running.lock().expect("mc running poisoned");
        if *running {
            return;
        }
        *running = true;
        *self.tx.lock().expect("mc tx poisoned") = Some(tx);
        *self.baseline.lock().expect("mc baseline poisoned") = snapshot_all();
        log::info!("[telemetry] minecraft stats baseline captured");
    }

    fn reset(&self) {
        let mut running = self.running.lock().expect("mc running poisoned");
        if !*running {
            return;
        }
        *running = false;

        let baseline = std::mem::take(&mut *self.baseline.lock().expect("mc baseline poisoned"));
        let tx = self.tx.lock().expect("mc tx poisoned").take();
        let Some(tx) = tx else { return };

        // The world with the most playtime this session is "the" session.
        let mut best: Option<(PathBuf, WorldStats)> = None;
        for (path, now) in snapshot_all() {
            let base = baseline.get(&path).copied().unwrap_or_default();
            let delta = now.delta(&base);
            if delta.is_empty() {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|(_, b)| delta.play_time_ticks > b.play_time_ticks)
            {
                best = Some((path, delta));
            }
        }
        if let Some((path, delta)) = best {
            let _ = tx.send(TelemetryEvent::MatchEnded(Box::new(summarize(
                &world_name(&path),
                &delta,
            ))));
        }
    }
}

fn summarize(world: &str, delta: &WorldStats) -> MatchResult {
    MatchResult {
        game_id: GAME_ID.to_string(),
        mode: "world".to_string(),
        map: world.to_string(),
        // A sandbox session, not a contest: played-only, never streaked.
        result: Outcome::Incomplete,
        streak_eligible: false,
        own_score: 0,
        opp_score: 0,
        performance: (delta.mob_kills > 0 || delta.deaths > 0).then(|| Performance {
            kills: Some(delta.mob_kills.min(u32::MAX as u64) as u32),
            deaths: Some(delta.deaths.min(u32::MAX as u64) as u32),
            ..Performance::default()
        }),
        build: None,
        run: Some(RunInfo {
            stage_reached: Some(format!(
                "{} blocks mined, {} placed",
                delta.blocks_mined, delta.blocks_placed
            )),
            difficulty: None,
            duration_sec: Some((delta.play_time_ticks / 20).min(u32::MAX as u64) as u32),
        }),
        source: SourceQuality::Live,
        ts: now_ms(),
    }
}

/// `saves/<world>/stats/<uuid>.json` → parsed counters, for every world.
fn snapshot_all() -> HashMap<PathBuf, WorldStats> {
    let mut out = HashMap::new();
    let Some(saves) = saves_dir() else {
        return out;
    };
    let Ok(worlds) = std::fs::read_dir(&saves) else {
        return out;
    };
    for world in worlds.flatten() {
        let stats_dir = world.path().join("stats");
        let Ok(files) = std::fs::read_dir(&stats_dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_some_and(|e| e == "json") {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    if let Some(stats) = parse_stats(&contents) {
                        out.insert(path, stats);
                    }
                }
            }
        }
    }
    out
}

/// Parse a vanilla stats JSON (`{"stats": {"minecraft:custom": {...}, …}}`).
fn parse_stats(contents: &str) -> Option<WorldStats> {
    let v: serde_json::Value = serde_json::from_str(contents).ok()?;
    let stats = v.get("stats")?;
    let custom = |key: &str| -> u64 {
        stats
            .get("minecraft:custom")
            .and_then(|c| c.get(key))
            .and_then(|n| n.as_u64())
            .unwrap_or(0)
    };
    let category_total = |key: &str| -> u64 {
        stats
            .get(key)
            .and_then(|c| c.as_object())
            .map(|o| o.values().filter_map(|n| n.as_u64()).sum())
            .unwrap_or(0)
    };
    Some(WorldStats {
        play_time_ticks: custom("minecraft:play_time"),
        deaths: custom("minecraft:deaths"),
        mob_kills: custom("minecraft:mob_kills"),
        blocks_mined: category_total("minecraft:mined"),
        blocks_placed: category_total("minecraft:used"),
    })
}

/// `saves/<world>/stats/<uuid>.json` → `<world>`.
fn world_name(stats_file: &std::path::Path) -> String {
    stats_file
        .parent() // stats/
        .and_then(|p| p.parent()) // <world>/
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(windows)]
fn saves_dir() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    let dir = PathBuf::from(appdata).join(".minecraft").join("saves");
    dir.is_dir().then_some(dir)
}

#[cfg(not(windows))]
fn saves_dir() -> Option<PathBuf> {
    None
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATS: &str = r#"{
        "stats": {
            "minecraft:custom": {
                "minecraft:play_time": 240000,
                "minecraft:deaths": 2,
                "minecraft:mob_kills": 31,
                "minecraft:jump": 999
            },
            "minecraft:mined": { "minecraft:stone": 500, "minecraft:dirt": 120 },
            "minecraft:used": { "minecraft:cobblestone": 300 }
        },
        "DataVersion": 3700
    }"#;

    #[test]
    fn parses_vanilla_stats_json() {
        let s = parse_stats(STATS).expect("stats parse");
        assert_eq!(s.play_time_ticks, 240000);
        assert_eq!(s.deaths, 2);
        assert_eq!(s.mob_kills, 31);
        assert_eq!(s.blocks_mined, 620);
        assert_eq!(s.blocks_placed, 300);
        assert!(parse_stats("not json").is_none());
    }

    #[test]
    fn delta_and_summary() {
        let before = parse_stats(STATS).unwrap();
        let mut after = before;
        after.play_time_ticks += 20 * 60 * 30; // +30 min
        after.mob_kills += 5;
        after.blocks_mined += 100;

        let delta = after.delta(&before);
        assert_eq!(delta.play_time_ticks, 36000);
        assert_eq!(delta.mob_kills, 5);
        assert_eq!(delta.deaths, 0);

        let m = summarize("SkyBase", &delta);
        assert_eq!(m.map, "SkyBase");
        assert!(!m.streak_eligible);
        assert_eq!(m.result, Outcome::Incomplete);
        assert_eq!(m.run.as_ref().unwrap().duration_sec, Some(1800));
        assert_eq!(m.performance.as_ref().unwrap().kills, Some(5));
        assert!(m
            .run
            .as_ref()
            .unwrap()
            .stage_reached
            .as_deref()
            .unwrap()
            .contains("100 blocks mined"));
    }

    #[test]
    fn unchanged_world_is_empty_delta() {
        let s = parse_stats(STATS).unwrap();
        assert!(s.delta(&s).is_empty());
    }

    #[test]
    fn world_name_from_stats_path() {
        let p = PathBuf::from("saves/SkyBase/stats/abc-123.json");
        assert_eq!(world_name(&p), "SkyBase");
    }
}
