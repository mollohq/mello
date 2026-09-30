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

/// A user mentioned in a message: the id from its `<@user_id>` token and the
/// name shown for it. Composers send these for the members the user picked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionRef {
    pub user_id: String,
    pub name: String,
}

/// Name shown for a mention whose user cannot be resolved. A raw user id is never shown.
pub const UNKNOWN_MENTION_NAME: &str = "unknown";

fn is_mention_id(id: &str) -> bool {
    !id.is_empty() && !id.contains(|c: char| c.is_whitespace() || c == '<' || c == '@')
}

/// Byte range and user id of each `<@user_id>` token in `body`, in order.
fn mention_tokens(body: &str) -> Vec<(std::ops::Range<usize>, &str)> {
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(rel) = body[start..].find("<@") {
        let id_start = start + rel + 2;
        let Some(rel_close) = body[id_start..].find('>') else {
            break;
        };
        let id_end = id_start + rel_close;
        let id = &body[id_start..id_end];
        if is_mention_id(id) {
            out.push((start + rel..id_end + 1, id));
            start = id_end + 1;
        } else {
            start = id_start;
        }
    }
    out
}

/// Extract user IDs from `<@user_id>` tokens in a message body.
pub fn extract_mentions(body: &str) -> Vec<String> {
    mention_tokens(body)
        .into_iter()
        .map(|(_, id)| id.to_string())
        .collect()
}

/// Replace each `<@user_id>` token with `@name`. Returns the text to show and
/// the mentioned users in order of first appearance. A user missing from
/// `member_names` shows as [`UNKNOWN_MENTION_NAME`].
pub fn resolve_mentions(
    body: &str,
    member_names: &std::collections::HashMap<String, String>,
) -> (String, Vec<MentionRef>) {
    let mut out = String::with_capacity(body.len());
    let mut mentions: Vec<MentionRef> = Vec::new();
    let mut cursor = 0;
    for (range, user_id) in mention_tokens(body) {
        let name = member_names
            .get(user_id)
            .filter(|n| !n.is_empty())
            .map_or(UNKNOWN_MENTION_NAME, String::as_str);
        out.push_str(&body[cursor..range.start]);
        out.push('@');
        out.push_str(name);
        cursor = range.end;
        if !mentions.iter().any(|m| m.user_id == user_id) {
            mentions.push(MentionRef {
                user_id: user_id.to_string(),
                name: name.to_string(),
            });
        }
    }
    out.push_str(&body[cursor..]);
    (out, mentions)
}

/// Byte ranges of each mention's `@name` in a display body, in text order and
/// without overlap. Longer names claim their text first, so `@Bob Smith` is not
/// split by a mention of `@Bob`; a name must end at a word boundary. Clients use
/// these ranges to style mentions.
pub fn mention_spans(display_body: &str, mentions: &[MentionRef]) -> Vec<std::ops::Range<usize>> {
    let mut tokens: Vec<String> = mentions
        .iter()
        .filter(|m| !m.name.is_empty())
        .map(|m| format!("@{}", m.name))
        .collect();
    tokens.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    tokens.dedup();

    let mut spans: Vec<std::ops::Range<usize>> = Vec::new();
    for token in &tokens {
        let mut from = 0;
        while let Some(rel) = display_body[from..].find(token.as_str()) {
            let start = from + rel;
            let end = start + token.len();
            let ends_word = !display_body[end..]
                .chars()
                .next()
                .is_some_and(continues_name);
            if ends_word && !spans.iter().any(|r| r.start < end && start < r.end) {
                spans.push(start..end);
            }
            from = end;
        }
    }
    spans.sort_by_key(|r| r.start);
    spans
}

fn continues_name(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Replace each picked `@name` in composer text with its `<@user_id>` token.
/// Typed `@name` text without a pick stays plain text. A name matches only as a
/// whole word, so a pick of `@bob` leaves `@bobby` and `me@bob` alone.
pub fn encode_mentions(text: &str, picks: &[MentionRef]) -> String {
    let mut picks: Vec<&MentionRef> = picks
        .iter()
        .filter(|p| !p.name.is_empty() && is_mention_id(&p.user_id))
        .collect();
    // Longest first, so a pick of "Bob Smith" wins over a pick of "Bob".
    picks.sort_by_key(|p| std::cmp::Reverse(p.name.len()));

    let mut out = String::with_capacity(text.len());
    let mut prev: Option<char> = None;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if c == '@' && !prev.is_some_and(continues_name) {
            let after = &rest[1..];
            let pick = picks.iter().find(|p| {
                after.starts_with(p.name.as_str())
                    && !after[p.name.len()..]
                        .chars()
                        .next()
                        .is_some_and(continues_name)
            });
            if let Some(p) = pick {
                out.push_str("<@");
                out.push_str(&p.user_id);
                out.push('>');
                prev = p.name.chars().last();
                rest = &after[p.name.len()..];
                continue;
            }
        }
        out.push(c);
        prev = Some(c);
        rest = &rest[c.len_utf8()..];
    }
    out
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

/// Pull bare URLs out of a display body (mentions already resolved, see
/// [`resolve_mentions`]) for separate pill widgets. Returns markdown-safe plain
/// text (no link syntax) plus link metadata.
pub fn prepare_body_for_markdown(display_body: &str) -> (String, Vec<ChatLink>) {
    extract_urls_from_text(display_body)
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

/// True when a Nakama channel `content` field must not appear in chat (signaling, empty JSON, etc.).
pub fn is_non_display_channel_content(content_str: &str) -> bool {
    let trimmed = content_str.trim();
    if trimmed.is_empty() {
        return true;
    }
    let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return false;
    };
    let Some(obj) = val.as_object() else {
        return false;
    };
    if obj.get("signal").and_then(|v| v.as_bool()) == Some(true) {
        return true;
    }
    if obj.get("to").is_some() && (obj.contains_key("data") || obj.contains_key("signal")) {
        return true;
    }
    if obj.is_empty() {
        return true;
    }
    !json_object_has_displayable_chat(obj)
}

fn json_object_has_displayable_chat(obj: &serde_json::Map<String, serde_json::Value>) -> bool {
    if obj
        .get("text")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
    {
        return true;
    }
    if obj.get("gif").is_some() {
        return true;
    }
    if obj.get("event").is_some() {
        return true;
    }
    if obj.get("v").and_then(|v| v.as_u64()).unwrap_or(0) >= 1 {
        if obj
            .get("body")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty())
        {
            return true;
        }
        if obj.get("type").and_then(|v| v.as_str()) == Some("system") {
            return true;
        }
    }
    false
}

fn envelope_is_displayable(envelope: &MessageEnvelope) -> bool {
    if envelope.msg_type == MessageType::System {
        return true;
    }
    if envelope.gif.is_some() {
        return true;
    }
    !envelope.body.trim().is_empty()
}

/// Build a [`ChatMessage`] from a parsed envelope and Nakama metadata.
/// `member_names` resolves the `<@user_id>` tokens for `display_body`; resolve
/// unknown mentioned users into it first (see `MemberNames::ensure`).
#[allow(clippy::too_many_arguments)]
pub fn chat_message_from_envelope(
    message_id: String,
    sender_id: String,
    sender_name: String,
    create_time: String,
    update_time: String,
    envelope: MessageEnvelope,
    content_str: &str,
    member_names: &std::collections::HashMap<String, String>,
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
            display_body: String::new(),
            mentions: Vec::new(),
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

    if !is_deleted && !envelope_is_displayable(&envelope) {
        return None;
    }

    let is_edited = !is_system
        && !update_time.is_empty()
        && !create_time.is_empty()
        && update_time != create_time;

    let (display_body, mentions) = resolve_mentions(&envelope.body, member_names);
    Some(ChatMessage {
        message_id,
        sender_id,
        sender_name,
        content: envelope.body,
        display_body,
        mentions,
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
/// Falls back to legacy `{"text":"..."}` format, then non-JSON plain text.
pub fn parse_content(content_str: &str) -> Option<MessageEnvelope> {
    if is_non_display_channel_content(content_str) {
        return None;
    }

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
        // Valid JSON object without displayable chat fields — do not plain-text fallback.
        if val.is_object() {
            return None;
        }
    }
    // Plain-text content (pre-envelope messages, not JSON)
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
    /// The body as sent, with `<@user_id>` mention tokens.
    pub content: String,
    /// The body to show, mentions resolved. See [`ChatMessage::display_body`].
    pub display_body: String,
    pub mentions: Vec<MentionRef>,
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
        truncate_preview(&orig.display_body, 100)
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
                display_body: msg.display_body.clone(),
                mentions: msg.mentions.clone(),
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
            display_body: if msg.is_deleted {
                "[message deleted]".to_string()
            } else {
                msg.display_body.clone()
            },
            mentions: msg.mentions.clone(),
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
            display_body: text.to_string(),
            mentions: Vec::new(),
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

    fn names(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(id, n)| (id.to_string(), n.to_string()))
            .collect()
    }

    fn pick(user_id: &str, name: &str) -> MentionRef {
        MentionRef {
            user_id: user_id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn resolve_mentions_shows_names_and_lists_each_user_once() {
        let (body, mentions) = resolve_mentions(
            "hey <@u1> and <@u2>, <@u1>!",
            &names(&[("u1", "Alice"), ("u2", "Bob Smith")]),
        );
        assert_eq!(body, "hey @Alice and @Bob Smith, @Alice!");
        assert_eq!(mentions, vec![pick("u1", "Alice"), pick("u2", "Bob Smith")]);
    }

    #[test]
    fn resolve_mentions_never_shows_a_raw_user_id() {
        let (body, mentions) = resolve_mentions("ping <@9f3c-uuid>", &names(&[]));
        assert_eq!(body, "ping @unknown");
        assert_eq!(mentions, vec![pick("9f3c-uuid", UNKNOWN_MENTION_NAME)]);
    }

    #[test]
    fn resolve_mentions_leaves_text_that_is_not_a_token() {
        let text = "a <@ b> c <@> d <@u1";
        let (body, mentions) = resolve_mentions(text, &names(&[("u1", "Alice")]));
        assert_eq!(body, text);
        assert!(mentions.is_empty());
    }

    #[test]
    fn encode_mentions_turns_picked_names_into_tokens() {
        let body = encode_mentions(
            "@Bob Smith and @Alice, see this",
            &[pick("u1", "Alice"), pick("u2", "Bob Smith")],
        );
        assert_eq!(body, "<@u2> and <@u1>, see this");
    }

    #[test]
    fn encode_mentions_leaves_unpicked_and_partial_names() {
        let picks = [pick("u1", "bob")];
        assert_eq!(encode_mentions("@bobby hi", &picks), "@bobby hi");
        assert_eq!(encode_mentions("me@bob hi", &picks), "me@bob hi");
        assert_eq!(encode_mentions("@carol hi", &picks), "@carol hi");
        assert_eq!(encode_mentions("hi @bob.", &picks), "hi <@u1>.");
    }

    #[test]
    fn encode_mentions_prefers_the_longest_picked_name() {
        let picks = [pick("u1", "Bob"), pick("u2", "Bob Smith")];
        assert_eq!(encode_mentions("@Bob Smith @Bob", &picks), "<@u2> <@u1>");
    }

    #[test]
    fn encoded_mentions_round_trip_through_display() {
        let picks = [pick("u1", "Åsa"), pick("u2", "Bob Smith")];
        let body = encode_mentions("hej @Åsa och @Bob Smith 👋", &picks);
        assert_eq!(extract_mentions(&body), vec!["u1", "u2"]);
        let (shown, mentions) =
            resolve_mentions(&body, &names(&[("u1", "Åsa"), ("u2", "Bob Smith")]));
        assert_eq!(shown, "hej @Åsa och @Bob Smith 👋");
        assert_eq!(mentions, picks.to_vec());
    }

    #[test]
    fn mention_spans_cover_whole_names_longest_first() {
        let text = "@Bob Smith and @Bob, not @Bobby or me@Bob";
        let spans = mention_spans(text, &[pick("u1", "Bob"), pick("u2", "Bob Smith")]);
        let found: Vec<&str> = spans.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(found, vec!["@Bob Smith", "@Bob", "@Bob"]);
        // "@Bobby" is not a mention of Bob; "me@Bob" still ends at a boundary.
        assert_eq!(spans[1].start, text.find("@Bob,").unwrap());
    }

    #[test]
    fn mention_spans_handle_unicode_names() {
        let text = "hej @Åsa 👋 och @unknown";
        let spans = mention_spans(text, &[pick("u1", "Åsa"), pick("u9", UNKNOWN_MENTION_NAME)]);
        let found: Vec<&str> = spans.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(found, vec!["@Åsa", "@unknown"]);
    }

    #[test]
    fn chat_message_carries_the_resolved_body() {
        let env = MessageEnvelope::text("yo <@u2>", None);
        let json = serde_json::to_string(&env).unwrap();
        let m = chat_message_from_envelope(
            "m1".into(),
            "u1".into(),
            "Alice".into(),
            "t".into(),
            "t".into(),
            env,
            &json,
            &names(&[("u2", "Bob")]),
        )
        .unwrap();
        assert_eq!(m.content, "yo <@u2>");
        assert_eq!(m.display_body, "yo @Bob");
        assert!(m.mentions_user("u2"));
        assert!(!m.mentions_user("u1"));
    }

    #[test]
    fn reply_preview_uses_the_resolved_body() {
        let mut orig = msg("1", "u1", "alice", "2026-03-08T12:00:00Z", "hi <@u2>");
        orig.display_body = "hi @Bob".into();
        let mut reply = msg("2", "u2", "bob", "2026-03-08T12:01:00Z", "yes");
        reply.reply_to = Some("1".into());
        let display = prepare_messages_for_display(&[orig, reply]);
        assert_eq!(display[1].reply_preview.as_deref(), Some("hi @Bob"));
    }

    #[test]
    fn link_display_label_host_only() {
        assert_eq!(link_display_label("https://slint.dev"), "slint.dev");
    }

    #[test]
    fn prepare_body_extracts_url_pills() {
        let (plain, links) = prepare_body_for_markdown("see https://slint.dev/docs ok");
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
    fn parse_content_rejects_empty_json_object() {
        assert!(parse_content("{}").is_none());
        assert!(is_non_display_channel_content("{}"));
    }

    #[test]
    fn parse_content_rejects_signaling_shape_without_signal_flag() {
        let json = r#"{"to":"user-b","data":"{\"Offer\":{}}"}"#;
        assert!(parse_content(json).is_none());
        assert!(is_non_display_channel_content(json));
    }

    #[test]
    fn parse_content_rejects_signal_payload() {
        let json = r#"{"signal":true,"to":"user-b","data":"{}"}"#;
        assert!(parse_content(json).is_none());
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
            display_body: "hi".into(),
            mentions: Vec::new(),
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
