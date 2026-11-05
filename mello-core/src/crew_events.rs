use serde::{Deserialize, Deserializer, Serialize};

/// Accept an explicit JSON `null` for a field that has a default.
///
/// `#[serde(default)]` covers a *missing* key only. Go marshals a nil slice as
/// `null`, not `[]`, and no `omitempty` is set on the stats fields, so the
/// backend does send `"recent_days": null`. Without this the whole response
/// fails to parse and the You strip reports zero games.
fn null_to_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

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
    /// Foreground minutes; `duration_min` is wall time. Sent so surfaces can
    /// choose which to show for a game left open in the background.
    #[serde(default)]
    pub active_min: u32,
    /// IGDB id when the catalogue resolved the game, 0 otherwise.
    #[serde(default)]
    pub igdb_id: u32,
    /// Decisive (streak-eligible) wins/losses this session, from telemetry.
    #[serde(default)]
    pub wins: u32,
    #[serde(default)]
    pub losses: u32,
    /// Drawn matches this session — recorded but don't move the streak.
    #[serde(default)]
    pub draws: u32,
    /// Client-generated idempotency key so a retried RPC can't double-apply
    /// streaks or duplicate the ledger event server-side.
    #[serde(default)]
    pub session_id: String,
    /// The activity inside the game (spec 18 §3.11): the Roblox experience's
    /// name. Empty for a game without one, and absent from the payload then.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub activity: String,
    /// Session start, Unix ms. 0 when unknown. Additive: the backend ignores
    /// unknown JSON fields today (plain `json.Unmarshal`).
    #[serde(default)]
    pub start_ms: i64,
    /// Session end, Unix ms. For a queued report this is the original end,
    /// not the time of delivery. 0 when unknown.
    #[serde(default)]
    pub end_ms: i64,
    /// Local UTC offset in minutes at the end of the session (east is positive).
    #[serde(default)]
    pub tz_offset_min: i32,
}

/// Local UTC offset in minutes at `at_ms` (Unix ms), east positive.
pub fn local_tz_offset_min(at_ms: i64) -> i32 {
    use chrono::{Local, Offset, TimeZone};
    Local
        .timestamp_millis_opt(at_ms)
        .single()
        .map(|t| t.offset().fix().local_minus_utc() / 60)
        .unwrap_or(0)
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
pub struct RecentDayEntry {
    pub date: String,
    #[serde(default)]
    pub wall_min: u32,
    #[serde(default)]
    pub active_min: u32,
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
    #[serde(default, deserialize_with = "null_to_default")]
    pub recent_form: Vec<String>,
    #[serde(default)]
    pub last_result: String,
    #[serde(default)]
    pub last_played: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub played_min_total: u32,
    #[serde(default)]
    pub active_min_total: u32,
    #[serde(default, deserialize_with = "null_to_default")]
    pub recent_days: Vec<RecentDayEntry>,
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

/// A member who joined the crew in the last 24 hours (`FeedResponse::joined`),
/// from the member_joined events of the ledger. Newest first, at most three.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeedJoin {
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub name: String,
    /// The member whose invite code the joiner used. Empty when none.
    #[serde(default)]
    pub invited_by: String,
    #[serde(default)]
    pub invited_by_name: String,
    #[serde(default)]
    pub ts: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedResponse {
    pub crew_id: String,
    #[serde(default)]
    pub sections: Vec<FeedSection>,
    #[serde(default)]
    pub joined: Vec<FeedJoin>,
    /// The zone feed (`crew_feed_zones.go`), when the request asked for it and
    /// the server has it.
    #[serde(default)]
    pub zones: Option<FeedZones>,
}

// --- Zone feed (crew_feed with `zones: true`) ---
//
// Five fixed zones and Crew history. The server picks what each zone holds;
// the client draws it and adds what is client state (live streams, live
// sessions, voice, sealed and unread state). Free is the last `free_days`
// days; older is Crew history, gated per viewer (`plus`).

/// The zones payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeedZones {
    #[serde(default)]
    pub free_days: i32,
    #[serde(default)]
    pub plus: bool,
    #[serde(default)]
    pub stage: ZoneStage,
    #[serde(default)]
    pub board: ZoneBoard,
    /// The head2head data of the crew for the viewer (titles, pairs, own
    /// values), as the `head2head` feed entry carries it.
    #[serde(default)]
    pub head2head: Option<serde_json::Value>,
    /// Every clip of the free window, game clips first, then newest.
    #[serde(default)]
    pub clips: Vec<FeedEntry>,
    #[serde(default)]
    pub people: ZonePeople,
    /// Every game, voice and stream session of the free window, newest first.
    #[serde(default)]
    pub sessions: Vec<FeedEntry>,
    /// The games most members have, on a quiet week with no session.
    #[serde(default)]
    pub installed: Vec<serde_json::Value>,
    /// One page of past weeks, newest first.
    #[serde(default)]
    pub history: Vec<ZoneHistoryWeek>,
    /// The `history_before` of the next page. Zero at the end.
    #[serde(default)]
    pub history_next: i64,
}

/// The one thing to watch: `kind` is `game_clip`, `stream_replay` (with an
/// entry), `belt` or `own_belts` (built from `head2head`), or empty.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZoneStage {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub entry: Option<FeedEntry>,
}

/// The crew in numbers. `mode`: `tabs`, `quiet`, `past` or `solo`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZoneBoard {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub recap: Option<FeedEntry>,
    /// The daily recap of the current UTC day.
    #[serde(default)]
    pub today: Option<FeedEntry>,
    #[serde(default)]
    pub last_activity_ms: i64,
    #[serde(default)]
    pub players_last_week: Vec<String>,
}

/// The first five and the joins of 24 hours. Voice is client state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZonePeople {
    #[serde(default)]
    pub member_count: i32,
    #[serde(default)]
    pub open_seats: i32,
    #[serde(default)]
    pub joined: Vec<FeedJoin>,
}

/// One past week of Crew history. `locked`: counts and teasers only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZoneHistoryWeek {
    #[serde(default)]
    pub week_start: i64,
    #[serde(default)]
    pub week_number: i32,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub clip_count: i32,
    #[serde(default)]
    pub stream_count: i32,
    #[serde(default)]
    pub recap: Option<FeedEntry>,
    #[serde(default)]
    pub items: Vec<FeedEntry>,
    #[serde(default)]
    pub teasers: Vec<ZoneTeaser>,
}

/// A locked clip or stream: who, what kind, how long. No URL.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZoneTeaser {
    /// `clip`, `voice` or `stream`.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub actor_id: String,
    #[serde(default)]
    pub actor_name: String,
    #[serde(default)]
    pub duration_seconds: f64,
    #[serde(default)]
    pub ts: i64,
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
    #[serde(default)]
    pub waveform: String,
    /// A game clip's video size (CLIPS.md 15.8). Zero for a voice clip.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub width: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub height: u32,
    /// A game clip has the crew voice in it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_voice: bool,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
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

// --- Chat image upload ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatUploadURLRequest {
    pub crew_id: String,
    pub mime: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatUploadURLResponse {
    #[serde(default)]
    pub upload_url: String,
    #[serde(default)]
    pub media_url: String,
    #[serde(default)]
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatDeleteImagesRequest {
    pub keys: Vec<String>,
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
    /// Set when a game clip's poster is complete.
    #[serde(default)]
    pub poster_url: String,
}

// --- Game clip upload (CLIPS.md 15.8) ---

/// `clip_upload_url` for a game clip: the poster PUT and the MP4 parts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameClipUploadUrlRequest {
    pub clip_id: String,
    pub crew_id: String,
    /// Always "game".
    pub clip_type: String,
    pub poster_bytes: u64,
    pub video_bytes: u64,
    /// Set: new part URLs for this multipart upload, no new upload.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub upload_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GameClipUploadUrlResponse {
    #[serde(default)]
    pub poster_upload_url: String,
    #[serde(default)]
    pub poster_url: String,
    #[serde(default)]
    pub upload_id: String,
    #[serde(default)]
    pub part_size: u64,
    #[serde(default)]
    pub parts: Vec<UploadPartUrl>,
    #[serde(default)]
    pub media_url: String,
    #[serde(default)]
    pub cache_control: String,
}

/// A presigned `UploadPart` with its signed length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadPartUrl {
    pub part_number: u32,
    pub url: String,
    pub bytes: u64,
}

/// `clip_upload_complete` for a game clip: `part` is "poster" or "video".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameClipUploadCompleteRequest {
    pub clip_id: String,
    pub crew_id: String,
    /// Always "game".
    pub clip_type: String,
    pub part: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub upload_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<CompletedPart>,
}

/// A part that is up, with the ETag that its PUT returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletedPart {
    pub part_number: u32,
    pub etag: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The backend sets no `omitempty` on the stats slices, and Go marshals a
    /// nil slice as `null`. One such document used to fail the whole parse,
    /// which emptied the list and hid the You strip.
    #[test]
    fn user_game_stats_accepts_null_slices() {
        let json = r#"{
            "games": [{
                "game_id": "cs2",
                "recent_form": null,
                "recent_days": null,
                "played_min_total": 312
            }]
        }"#;

        let parsed: UserGameStatsListResponse =
            serde_json::from_str(json).expect("a null slice must parse as empty");

        assert_eq!(parsed.games.len(), 1);
        assert_eq!(parsed.games[0].played_min_total, 312);
        assert!(parsed.games[0].recent_form.is_empty());
        assert!(parsed.games[0].recent_days.is_empty());
    }

    /// The backend reads `tz_offset_min` for the night_owl title. The key must
    /// be on the wire and carry the local offset at the session end.
    #[test]
    fn session_end_request_sends_the_local_tz_offset() {
        use chrono::{Local, Offset, TimeZone};
        let end_ms = 1_760_000_000_000;
        let expect = Local
            .timestamp_millis_opt(end_ms)
            .single()
            .map(|t| t.offset().fix().local_minus_utc() / 60)
            .unwrap_or(0);
        let req = serde_json::from_value::<GameSessionEndRequest>(serde_json::json!({
            "crew_id": "c",
            "game_name": "G",
            "game_id": "g",
            "duration_min": 1,
            "tz_offset_min": local_tz_offset_min(end_ms),
        }))
        .unwrap();
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["tz_offset_min"], serde_json::json!(expect));
    }
}
