use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatchupResponse {
    pub crew_id: String,
    pub catchup_text: String,
    #[serde(default)]
    pub event_count: u32,
    #[serde(default)]
    pub top_events: Vec<CatchupEvent>,
    #[serde(default)]
    pub has_events: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatchupEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub actor_id: String,
    #[serde(default)]
    pub ts: i64,
    #[serde(default)]
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostMomentRequest {
    pub crew_id: String,
    pub sentiment: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub game_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostMomentResponse {
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub event_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameSessionEndRequest {
    pub crew_id: String,
    pub game_name: String,
    /// Stable game DB id, used as the per-user stats key. Empty for games
    /// without telemetry (server falls back to game_name-only behavior).
    #[serde(default)]
    pub game_id: String,
    #[serde(default)]
    pub duration_min: u32,
    /// Decisive (streak-eligible) wins/losses this session, from telemetry.
    #[serde(default)]
    pub wins: u32,
    #[serde(default)]
    pub losses: u32,
    /// Drawn matches this session — recorded but don't move the streak.
    #[serde(default)]
    pub draws: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GameSessionEndResponse {
    #[serde(default)]
    pub success: bool,
    /// Signed streak after this session: +N win streak, -N loss streak.
    /// Defaults to 0 against older servers that don't return it.
    #[serde(default)]
    pub streak_after: i32,
}

/// Per-game personal stats (mirrors the backend `user_game_stats` store).
/// Backs the personal "You strip" + profile (spec 19).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserGameStats {
    pub game_id: String,
    #[serde(default)]
    pub wins: u32,
    #[serde(default)]
    pub losses: u32,
    #[serde(default)]
    pub draws: u32,
    /// Signed: +N win streak, -N loss streak (per session).
    #[serde(default)]
    pub current_streak: i32,
    #[serde(default)]
    pub longest_win_streak: u32,
    #[serde(default)]
    pub longest_loss_streak: u32,
    /// Per-session form, newest last: "W" | "L" | "D".
    #[serde(default)]
    pub recent_form: Vec<String>,
    #[serde(default)]
    pub last_result: String,
    #[serde(default)]
    pub last_played: i64,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserGameStatsListResponse {
    #[serde(default)]
    pub games: Vec<UserGameStats>,
}

// --- Timeline (crew feed) ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    #[serde(default)]
    pub actor_id: String,
    #[serde(default)]
    pub ts: i64,
    #[serde(default)]
    pub score: i32,
    #[serde(default)]
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineResponse {
    pub crew_id: String,
    #[serde(default)]
    pub entries: Vec<TimelineEntry>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub has_more: bool,
}

// --- Curated feed (crew_feed) ---
//
// role/size/type stay plain strings so the server can ship new curation
// treatments without a client release; unknown values degrade gracefully.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub size: String,
    #[serde(default)]
    pub ts: i64,
    #[serde(default)]
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedSection {
    pub id: String,
    #[serde(default)]
    pub entries: Vec<FeedEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedResponse {
    pub crew_id: String,
    #[serde(default)]
    pub sections: Vec<FeedSection>,
}

// --- PostClip ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostClipRequest {
    pub crew_id: String,
    pub clip_id: String,
    #[serde(default)]
    pub clip_type: String,
    pub duration_seconds: f64,
    #[serde(default)]
    pub participants: Vec<String>,
    #[serde(default)]
    pub game: String,
    #[serde(default)]
    pub local_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostClipResponse {
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub event_id: String,
    #[serde(default)]
    pub clip_id: String,
}

// --- Clip upload ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipUploadURLRequest {
    pub clip_id: String,
    pub crew_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipUploadURLResponse {
    #[serde(default)]
    pub upload_url: String,
    #[serde(default)]
    pub media_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipUploadCompleteRequest {
    pub clip_id: String,
    pub crew_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipUploadCompleteResponse {
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub media_url: String,
}

// --- Diagnostic log upload ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticLogUploadURLRequest {
    pub capture_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticLogUploadURLResponse {
    /// Presigned PUT URL; empty when storage is not configured (client skips).
    #[serde(default)]
    pub upload_url: String,
    /// Object key the log will be stored under (for support tooling reference).
    #[serde(default)]
    pub key: String,
}
