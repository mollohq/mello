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
    #[serde(default)]
    pub duration_min: u32,
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
