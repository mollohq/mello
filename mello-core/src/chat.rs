use chrono::{DateTime, Datelike, Local, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::events::ChatMessage;

// ---------------------------------------------------------------------------
// Structured message envelope types (spec §2)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageType {
    #[default]
    Text,
    Gif,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GifData {
    pub id: String,
    pub url: String,
    pub preview: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub alt: String,
}

/// The parsed message envelope stored in the Nakama `content` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEnvelope {
    pub v: u32,
    #[serde(rename = "type")]
    pub msg_type: MessageType,
    #[serde(default)]
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gif: Option<GifData>,
    // System message fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl MessageEnvelope {
    pub fn text(body: &str, reply_to: Option<String>) -> Self {
        let mentions = extract_mentions(body);
        Self {
            v: 1,
            msg_type: MessageType::Text,
            body: body.to_string(),
            reply_to,
            mentions,
            gif: None,
            event: None,
            data: None,
        }
    }

    pub fn gif(gif: GifData, body: &str) -> Self {
        Self {
            v: 1,
            msg_type: MessageType::Gif,
            body: body.to_string(),
            reply_to: None,
            mentions: Vec::new(),
            gif: Some(gif),
            event: None,
            data: None,
        }
    }
}

/// Extract user IDs from `<@user_id>` tokens in a message body.
pub fn extract_mentions(body: &str) -> Vec<String> {
    let mut mentions = Vec::new();
    let mut start = 0;
    while let Some(open) = body[start..].find("<@") {
        let abs_open = start + open + 2;
        if let Some(close) = body[abs_open..].find('>') {
            let user_id = &body[abs_open..abs_open + close];
            if !user_id.is_empty() {
                mentions.push(user_id.to_string());
            }
            start = abs_open + close + 1;
        } else {
            break;
        }
    }
    mentions
}

/// A URL extracted from message text for pill rendering in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatLink {
    pub url: String,
    pub label: String,
}

/// Max characters shown in a link pill label (host + path/query).
pub const LINK_LABEL_MAX_CHARS: usize = 52;

/// Host + path/query for link pills, e.g. `https://slint.dev/docs/foo?q=1` → `slint.dev/docs/foo?q=1`.
pub fn link_display_label(url: &str) -> String {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host_end = without_scheme
        .find(&['/', '?', '#'][..])
        .unwrap_or(without_scheme.len());
    let host = without_scheme[..host_end]
        .strip_prefix("www.")
        .unwrap_or(&without_scheme[..host_end]);
    let tail = without_scheme[host_end..].trim_start_matches('/');
    let tail = tail.split('#').next().unwrap_or(tail).trim_end_matches('/');
    let label = if tail.is_empty() {
        host.to_string()
    } else {
        format!("{host}/{tail}")
    };
    truncate_chars(&label, LINK_LABEL_MAX_CHARS)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Resolve mentions and pull bare URLs out of the body for separate pill widgets.
/// Returns markdown-safe plain text (no link syntax) plus link metadata.
pub fn prepare_body_for_markdown(
    body: &str,
    current_user_id: &str,
    member_names: &std::collections::HashMap<String, String>,
) -> (String, bool, Vec<ChatLink>) {
    let (resolved, mentions_self) = prepare_body_for_display(body, current_user_id, member_names);
    let (plain, links) = extract_urls_from_text(&resolved);
    (plain, mentions_self, links)
}

fn extract_urls_from_text(text: &str) -> (String, Vec<ChatLink>) {
    let mut plain = String::with_capacity(text.len());
    let mut links = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find("http") {
        let before = rest[..pos].trim_end();
        if !plain.is_empty() && !before.is_empty() && !plain.ends_with(' ') {
            plain.push(' ');
        }
        plain.push_str(before);
        let url_rest = &rest[pos..];
        let end = url_rest
            .find(|c: char| c.is_whitespace() || c == '>' || c == ')' || c == ']')
            .unwrap_or(url_rest.len());
        let url = &url_rest[..end];
        if url.starts_with("http://") || url.starts_with("https://") {
            if !plain.is_empty() && !plain.ends_with(' ') {
                plain.push(' ');
            }
            links.push(ChatLink {
                url: url.to_string(),
                label: link_display_label(url),
            });
        } else {
            plain.push_str(url);
        }
        rest = &url_rest[end..];
    }
    let tail = rest.trim_start();
    if !plain.is_empty() && !tail.is_empty() && !plain.ends_with(' ') {
        plain.push(' ');
    }
    plain.push_str(tail);
    let plain = plain.trim().to_string();
    (plain, links)
}

/// Resolve `<@user_id>` tokens in a message body to `@display_name`.
/// Returns the body with tokens replaced, plus whether the current user is mentioned.
pub fn prepare_body_for_display(
    body: &str,
    current_user_id: &str,
    member_names: &std::collections::HashMap<String, String>,
) -> (String, bool) {
    let mut result = body.to_string();
    let mut mentions_self = false;
    let mentions = extract_mentions(body);
    for uid in &mentions {
        if uid == current_user_id {
            mentions_self = true;
        }
        let display = member_names
            .get(uid.as_str())
            .map(|n| format!("@{}", n))
            .unwrap_or_else(|| format!("@{}", uid));
        let token = format!("<@{}>", uid);
        result = result.replace(&token, &display);
    }
    (result, mentions_self)
}

/// Build a [`ChatMessage`] from a parsed envelope and Nakama metadata.
pub fn chat_message_from_envelope(
    message_id: String,
    sender_id: String,
    sender_name: String,
    create_time: String,
    update_time: String,
    envelope: MessageEnvelope,
    content_str: &str,
) -> Option<ChatMessage> {
    let is_system = envelope.msg_type == MessageType::System;
    let is_deleted = !is_system
        && content_str.trim().is_empty()
        && envelope.gif.is_none()
        && envelope.body.is_empty();
    if is_deleted {
        return Some(ChatMessage {
            message_id,
            sender_id,
            sender_name,
            content: String::new(),
            timestamp: create_time.clone(),
            create_time,
            update_time,
            gif: None,
            reply_to: envelope.reply_to,
            is_system: false,
            is_edited: false,
            is_deleted: true,
        });
    }

    let is_edited = !is_system
        && !update_time.is_empty()
        && !create_time.is_empty()
        && update_time != create_time;

    Some(ChatMessage {
        message_id,
        sender_id,
        sender_name,
        content: envelope.body,
        timestamp: create_time.clone(),
        create_time,
        update_time,
        gif: envelope.gif,
        reply_to: envelope.reply_to,
        is_system,
        is_edited,
        is_deleted: false,
    })
}

/// Try to parse the Nakama content field as a structured envelope.
/// Falls back to legacy `{"text":"..."}` format.
pub fn parse_content(content_str: &str) -> Option<MessageEnvelope> {
    // Try structured envelope first
    if let Ok(env) = serde_json::from_str::<MessageEnvelope>(content_str) {
        if env.v >= 1 {
            return Some(env);
        }
    }
    // Fall back to legacy `{"text":"..."}` format
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(content_str) {
        if let Some(text) = val.get("text").and_then(|v| v.as_str()) {
            return Some(MessageEnvelope {
                v: 0,
                msg_type: MessageType::Text,
                body: text.to_string(),
                reply_to: None,
                mentions: Vec::new(),
                gif: None,
                event: None,
                data: None,
            });
        }
    }
    // Plain-text content (pre-envelope messages)
    let trimmed = content_str.trim();
    if !trimmed.is_empty() {
        return Some(MessageEnvelope::text(trimmed, None));
    }
    None
}

// ---------------------------------------------------------------------------
// Client-side unread tracking (volatile, resets on restart)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct UnreadState {
    pub count: u32,
    pub has_mention: bool,
}

/// Tracks unread messages per crew.
#[derive(Debug, Default)]
pub struct UnreadTracker {
    counts: std::collections::HashMap<String, UnreadState>,
}

impl UnreadTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Increment unread count for a crew. Set `mentions_self` if the message @-mentions the user.
    pub fn increment(&mut self, crew_id: &str, mentions_self: bool) {
        let entry = self.counts.entry(crew_id.to_string()).or_default();
        entry.count = entry.count.saturating_add(1);
        if mentions_self {
            entry.has_mention = true;
        }
    }

    /// Reset unread count for a crew (e.g., when the user switches to it).
    pub fn reset(&mut self, crew_id: &str) {
        self.counts.remove(crew_id);
    }

    /// Get unread state for a crew.
    pub fn get(&self, crew_id: &str) -> UnreadState {
        self.counts.get(crew_id).cloned().unwrap_or_default()
    }

    pub fn all(&self) -> &std::collections::HashMap<String, UnreadState> {
        &self.counts
    }
}

// ---------------------------------------------------------------------------
// Display types
// ---------------------------------------------------------------------------

/// Display-ready message with grouping and formatted timestamps.
#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub message_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub sender_initials: String,
    pub content: String,
    pub timestamp: String,
    pub display_time: String,
    pub is_group_start: bool,
    pub is_continuation: bool,
    pub is_system: bool,
    pub is_edited: bool,
    pub is_deleted: bool,
    pub gif: Option<GifData>,
    pub reply_to: Option<String>,
    pub reply_to_name: Option<String>,
    pub reply_preview: Option<String>,
}

const GROUP_GAP_SECS: i64 = 300; // 5 minutes

/// Compute 2-letter initials from a display name.
/// Matches the existing `make_initials` logic in client/src/main.rs.
pub fn make_initials(name: &str) -> String {
    let parts: Vec<&str> = name.split_whitespace().collect();
    match parts.len() {
        0 => "?".into(),
        1 => parts[0].chars().take(2).collect::<String>().to_uppercase(),
        _ => {
            let first = parts[0].chars().next().unwrap_or('?');
            let last = parts[parts.len() - 1].chars().next().unwrap_or('?');
            format!("{}{}", first, last).to_uppercase()
        }
    }
}

fn parse_timestamp(ts: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ")
                .ok()
                .map(|ndt| ndt.and_utc())
        })
}

/// Format a timestamp for display per spec section 4.2.
pub fn format_display_time(ts: &str) -> String {
    let now = Utc::now();
    let Some(dt) = parse_timestamp(ts) else {
        return ts.to_string();
    };

    let diff = now.signed_duration_since(dt);
    let secs = diff.num_seconds();

    if secs < 60 {
        return "just now".to_string();
    }
    if secs < 3600 {
        return format!("{}m ago", secs / 60);
    }

    let local_dt = dt.with_timezone(&Local);
    let local_now = now.with_timezone(&Local);

    if local_dt.date_naive() == local_now.date_naive() {
        return local_dt.format("%H:%M").to_string();
    }

    let yesterday = local_now.date_naive() - chrono::Duration::days(1);
    if local_dt.date_naive() == yesterday {
        return "Yesterday".to_string();
    }

    if local_dt.year() == local_now.year() {
        return local_dt.format("%b %-d").to_string();
    }

    local_dt.format("%b %-d, %Y").to_string()
}

/// Count non-system, non-deleted messages from the current local week (Mon–Sun) in the loaded set.
pub fn count_messages_this_week(messages: &[ChatMessage]) -> i32 {
    let now = Local::now();
    let today = now.date_naive();
    let week_start = today - chrono::Duration::days(now.weekday().num_days_from_monday() as i64);
    messages
        .iter()
        .filter(|m| !m.is_system && !m.is_deleted)
        .filter(|m| {
            parse_timestamp(&m.create_time)
                .or_else(|| parse_timestamp(&m.timestamp))
                .is_some_and(|dt| {
                    let d = dt.with_timezone(&Local).date_naive();
                    d >= week_start && d <= today
                })
        })
        .count() as i32
}

fn truncate_preview(text: &str, max: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    let mut s: String = t.chars().take(max).collect();
    s.push('…');
    s
}

fn resolve_reply(
    reply_to: &Option<String>,
    messages: &[ChatMessage],
) -> (Option<String>, Option<String>) {
    let Some(id) = reply_to else {
        return (None, None);
    };
    let Some(orig) = messages.iter().find(|m| &m.message_id == id) else {
        return (None, None);
    };
    let preview = if orig.is_deleted {
        "[message deleted]".to_string()
    } else if orig.gif.is_some() && orig.content.is_empty() {
        "GIF".to_string()
    } else {
        truncate_preview(&orig.content, 100)
    };
    (Some(orig.sender_name.clone()), Some(preview))
}

/// Takes a flat list of ChatMessages and produces DisplayMessages with grouping info.
pub fn prepare_messages_for_display(messages: &[ChatMessage]) -> Vec<DisplayMessage> {
    let mut result = Vec::with_capacity(messages.len());

    for (i, msg) in messages.iter().enumerate() {
        if msg.is_system {
            result.push(DisplayMessage {
                message_id: msg.message_id.clone(),
                sender_id: msg.sender_id.clone(),
                sender_name: msg.sender_name.clone(),
                sender_initials: String::new(),
                content: msg.content.clone(),
                timestamp: msg.timestamp.clone(),
                display_time: String::new(),
                is_group_start: false,
                is_continuation: false,
                is_system: true,
                is_edited: false,
                is_deleted: false,
                gif: None,
                reply_to: None,
                reply_to_name: None,
                reply_preview: None,
            });
            continue;
        }

        let is_group_start = if msg.reply_to.is_some() || i == 0 {
            true
        } else {
            let prev = &messages[i - 1];
            if prev.is_system || prev.sender_id != msg.sender_id {
                true
            } else if let (Some(prev_dt), Some(cur_dt)) = (
                parse_timestamp(&prev.timestamp),
                parse_timestamp(&msg.timestamp),
            ) {
                (cur_dt - prev_dt).num_seconds().abs() > GROUP_GAP_SECS
            } else {
                true
            }
        };

        let (reply_to_name, reply_preview) = resolve_reply(&msg.reply_to, messages);

        result.push(DisplayMessage {
            message_id: msg.message_id.clone(),
            sender_id: msg.sender_id.clone(),
            sender_name: msg.sender_name.clone(),
            sender_initials: make_initials(&msg.sender_name),
            content: if msg.is_deleted {
                "[message deleted]".to_string()
            } else {
                msg.content.clone()
            },
            timestamp: msg.timestamp.clone(),
            display_time: format_display_time(&msg.timestamp),
            is_group_start,
            is_continuation: !is_group_start,
            is_system: false,
            is_edited: msg.is_edited && !msg.is_deleted,
            is_deleted: msg.is_deleted,
            gif: msg.gif.clone(),
            reply_to: msg.reply_to.clone(),
            reply_to_name,
            reply_preview,
        });
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, sender: &str, name: &str, ts: &str, text: &str) -> ChatMessage {
        ChatMessage {
            message_id: id.to_string(),
            sender_id: sender.to_string(),
            sender_name: name.to_string(),
            content: text.to_string(),
            timestamp: ts.to_string(),
            create_time: ts.to_string(),
            update_time: ts.to_string(),
            gif: None,
            reply_to: None,
            is_system: false,
            is_edited: false,
            is_deleted: false,
        }
    }

    #[test]
    fn single_message_is_group_start() {
        let msgs = vec![msg("1", "u1", "alice", "2026-03-08T12:00:00Z", "hello")];
        let display = prepare_messages_for_display(&msgs);
        assert_eq!(display.len(), 1);
        assert!(display[0].is_group_start);
        assert!(!display[0].is_continuation);
    }

    #[test]
    fn same_sender_within_5min_groups() {
        let msgs = vec![
            msg("1", "u1", "alice", "2026-03-08T12:00:00Z", "hello"),
            msg("2", "u1", "alice", "2026-03-08T12:01:00Z", "world"),
            msg("3", "u1", "alice", "2026-03-08T12:04:00Z", "still grouped"),
        ];
        let display = prepare_messages_for_display(&msgs);
        assert!(display[0].is_group_start);
        assert!(display[1].is_continuation);
        assert!(display[2].is_continuation);
    }

    #[test]
    fn different_sender_breaks_group() {
        let msgs = vec![
            msg("1", "u1", "alice", "2026-03-08T12:00:00Z", "hello"),
            msg("2", "u2", "bob", "2026-03-08T12:00:30Z", "hey"),
        ];
        let display = prepare_messages_for_display(&msgs);
        assert!(display[0].is_group_start);
        assert!(display[1].is_group_start);
    }

    #[test]
    fn time_gap_breaks_group() {
        let msgs = vec![
            msg("1", "u1", "alice", "2026-03-08T12:00:00Z", "hello"),
            msg("2", "u1", "alice", "2026-03-08T12:06:00Z", "after gap"),
        ];
        let display = prepare_messages_for_display(&msgs);
        assert!(display[0].is_group_start);
        assert!(display[1].is_group_start);
    }

    #[test]
    fn initials_from_two_words() {
        assert_eq!(make_initials("Alice Baker"), "AB");
    }

    #[test]
    fn initials_from_single_word() {
        assert_eq!(make_initials("alice"), "AL");
    }

    #[test]
    fn initials_from_username_single_word() {
        assert_eq!(make_initials("k0ji_tech"), "K0");
    }

    #[test]
    fn initials_empty() {
        assert_eq!(make_initials(""), "?");
    }

    #[test]
    fn prepare_body_resolves_mentions() {
        let mut names = std::collections::HashMap::new();
        names.insert("u1".to_string(), "Alice".to_string());
        names.insert("u2".to_string(), "Bob".to_string());
        let (body, mentions_self) = prepare_body_for_display("hey <@u1> and <@u2>", "u1", &names);
        assert_eq!(body, "hey @Alice and @Bob");
        assert!(mentions_self);
    }

    #[test]
    fn prepare_body_no_self_mention() {
        let names = std::collections::HashMap::new();
        let (_, mentions_self) = prepare_body_for_display("hey <@u2>", "u1", &names);
        assert!(!mentions_self);
    }

    #[test]
    fn link_display_label_host_only() {
        assert_eq!(link_display_label("https://slint.dev"), "slint.dev");
    }

    #[test]
    fn prepare_body_extracts_url_pills() {
        let names = std::collections::HashMap::new();
        let (plain, _, links) =
            prepare_body_for_markdown("see https://slint.dev/docs ok", "u1", &names);
        assert_eq!(plain, "see ok");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].url, "https://slint.dev/docs");
        assert_eq!(links[0].label, "slint.dev/docs");
    }

    #[test]
    fn link_display_label_includes_query_and_truncates() {
        let long_path = "a".repeat(60);
        let url = format!("https://example.com/{long_path}?tab=main");
        let label = link_display_label(&url);
        assert!(label.starts_with("example.com/"));
        assert!(label.contains("?tab=main") || label.ends_with('…'));
        assert!(label.chars().count() <= LINK_LABEL_MAX_CHARS);
    }

    #[test]
    fn extract_mentions_basic() {
        let mentions = extract_mentions("hey <@user_abc> check <@user_def> out");
        assert_eq!(mentions, vec!["user_abc", "user_def"]);
    }

    #[test]
    fn extract_mentions_none() {
        assert!(extract_mentions("no mentions here").is_empty());
    }

    #[test]
    fn parse_content_structured_envelope() {
        let json = r#"{"v":1,"type":"text","body":"hello world"}"#;
        let env = parse_content(json).unwrap();
        assert_eq!(env.v, 1);
        assert_eq!(env.msg_type, MessageType::Text);
        assert_eq!(env.body, "hello world");
    }

    #[test]
    fn parse_content_legacy_format() {
        let json = r#"{"text":"legacy message"}"#;
        let env = parse_content(json).unwrap();
        assert_eq!(env.v, 0);
        assert_eq!(env.body, "legacy message");
    }

    #[test]
    fn parse_content_plain_text() {
        let env = parse_content("hello from the past").unwrap();
        assert_eq!(env.body, "hello from the past");
    }

    #[test]
    fn parse_content_gif_envelope() {
        let json = r#"{"v":1,"type":"gif","body":"","gif":{"id":"123","url":"http://a","preview":"http://b","width":320,"height":240,"alt":"cat"}}"#;
        let env = parse_content(json).unwrap();
        assert_eq!(env.msg_type, MessageType::Gif);
        assert!(env.gif.is_some());
        assert_eq!(env.gif.unwrap().id, "123");
    }

    #[test]
    fn envelope_text_roundtrip() {
        let env = MessageEnvelope::text("hey <@u1> check this", Some("msg123".into()));
        let json = serde_json::to_string(&env).unwrap();
        let parsed: MessageEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.body, "hey <@u1> check this");
        assert_eq!(parsed.reply_to, Some("msg123".into()));
        assert_eq!(parsed.mentions, vec!["u1"]);
    }

    fn msg_at(iso: &str) -> ChatMessage {
        ChatMessage {
            message_id: iso.into(),
            sender_id: "u1".into(),
            sender_name: "bob".into(),
            content: "hi".into(),
            timestamp: iso.into(),
            create_time: iso.into(),
            update_time: iso.into(),
            gif: None,
            reply_to: None,
            is_system: false,
            is_edited: false,
            is_deleted: false,
        }
    }

    #[test]
    fn count_messages_this_week_includes_current_week_only() {
        let today = Local::now().format("%Y-%m-%dT12:00:00Z").to_string();
        let last_week = (Local::now() - chrono::Duration::days(8))
            .format("%Y-%m-%dT12:00:00Z")
            .to_string();
        let msgs = vec![msg_at(&today), msg_at(&last_week), msg_at(&today)];
        assert_eq!(count_messages_this_week(&msgs), 2);
    }

    #[test]
    fn count_messages_this_week_skips_system_and_deleted() {
        let today = Local::now().format("%Y-%m-%dT12:00:00Z").to_string();
        let mut system = msg_at(&today);
        system.is_system = true;
        let mut deleted = msg_at(&today);
        deleted.is_deleted = true;
        assert_eq!(count_messages_this_week(&[system, deleted]), 0);
    }
}
