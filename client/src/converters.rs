use std::collections::HashMap;
use std::rc::Rc;

use slint::Model;
use slint::StyledText;

use crate::{
    ChatLinkData, ChatMessageData, CrewData, DebugHistory, InvitePersonData, MainWindow,
    MemberData, VoiceChannelData, VoiceChannelMember,
};

pub fn parse_capture_source_id(id: &str, mode: &str) -> (Option<u32>, Option<u64>, Option<u32>) {
    let num_part = id.rsplit('-').next().unwrap_or("");
    match mode {
        "monitor" => (num_part.parse().ok(), None, None),
        "window" => (None, num_part.parse().ok(), None),
        "process" => (None, None, num_part.parse().ok()),
        _ => (None, None, None),
    }
}

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

/// The identity colour of a person, from a stable seed: one of the five
/// `Theme.id-*` colours that `UserAvatar` maps `color-index` to.
pub fn avatar_color_index(seed: &str) -> i32 {
    if seed.is_empty() {
        return 0;
    }

    (seed.bytes().fold(0u32, |acc, b| acc.wrapping_add(b as u32)) % 5) as i32
}

/// A person shown with an invite (the inviter, or a member), as an octagon.
pub fn invite_person(p: &mello_core::crew::InvitePerson) -> InvitePersonData {
    InvitePersonData {
        name: p.display_name.as_str().into(),
        initials: make_initials(&p.display_name).into(),
        color_index: avatar_color_index(&p.avatar_seed),
    }
}

pub struct ChatConvertOptions<'a> {
    pub user_id: &'a str,
    pub user_avatar: &'a slint::Image,
    pub has_user_avatar: bool,
    pub avatar_cache: &'a HashMap<String, slint::Image>,
    /// `Theme.mention` as `#rrggbb`, for the mention spans in the markdown.
    pub mention_color: &'a str,
    pub first_unread_id: Option<&'a str>,
}

/// `color` as `#rrggbb` for a `<font color>` tag in styled text.
pub fn color_hex(color: slint::Color) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        color.red(),
        color.green(),
        color.blue()
    )
}

/// Backslash-escapes ASCII punctuation, so a `*`, `_` or `<` in a name stays
/// literal text inside the markdown instead of changing the markup.
fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The message markdown with each mention (`@name`) in the mention colour
/// (TEXT-CHAT §7). The rest of the text is passed through unchanged, so a
/// user's own markdown still applies.
pub fn markdown_with_mentions(
    text: &str,
    mentions: &[mello_core::chat::MentionRef],
    color: &str,
) -> String {
    let spans = mello_core::chat::mention_spans(text, mentions);
    if spans.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + spans.len() * 32);
    let mut cursor = 0;
    for span in spans {
        out.push_str(&text[cursor..span.start]);
        out.push_str(&format!(
            "<font color=\"{color}\">{}</font>",
            escape_markdown(&text[span.clone()])
        ));
        cursor = span.end;
    }
    out.push_str(&text[cursor..]);
    out
}

pub fn chat_messages_to_slint(
    raw: &[mello_core::events::ChatMessage],
    opts: &ChatConvertOptions<'_>,
) -> Vec<ChatMessageData> {
    let display = mello_core::chat::prepare_messages_for_display(raw);
    let mut out = Vec::with_capacity(display.len() + 1);

    for d in display {
        if let Some(unread_id) = opts.first_unread_id {
            if d.message_id == unread_id {
                out.push(ChatMessageData {
                    message_id: slint::SharedString::from("__unread_divider__"),
                    is_unread_divider: true,
                    ..Default::default()
                });
            }
        }

        let is_gif = d.gif.is_some();
        let (gif_preview_url, gif_width, gif_height) = match &d.gif {
            Some(g) => (g.preview.clone(), g.width as i32, g.height as i32),
            None => (String::new(), 0, 0),
        };
        let is_self = d.sender_id == opts.user_id;
        let (sender_av, has_sender_av) = if is_self && opts.has_user_avatar {
            (opts.user_avatar.clone(), true)
        } else if let Some(img) = opts.avatar_cache.get(&d.sender_id) {
            (img.clone(), true)
        } else {
            (slint::Image::default(), false)
        };

        let mentions_self = d.mentions.iter().any(|m| m.user_id == opts.user_id);
        let (display_text, links) = if d.is_system || d.is_deleted {
            (d.display_body.clone(), Vec::new())
        } else {
            mello_core::chat::prepare_body_for_markdown(&d.display_body)
        };

        // Guard: one message must never render into a giant Skia glyph buffer.
        // `wrap: word-wrap` in the .slint handles long messages that contain
        // spaces; this bounds the glyph count for unbroken/huge pastes (a
        // spaceless multi-MB string is millions of glyphs that word-wrap can't
        // break), which is what made a single pasted log cost ~180 MB. The full
        // untruncated body is still kept in `text` for copy/edit.
        let display_text = {
            const MAX_DISPLAY_CHARS: usize = 8000;
            let char_count = display_text.chars().count();
            if char_count > MAX_DISPLAY_CHARS {
                log::warn!(
                    "[chat] message {} body is {} chars — truncating display to {} to bound render cost",
                    d.message_id, char_count, MAX_DISPLAY_CHARS
                );
                let mut truncated: String = display_text.chars().take(MAX_DISPLAY_CHARS).collect();
                truncated.push('…');
                truncated
            } else {
                display_text
            }
        };

        let display_styled: StyledText = if d.is_system || d.is_deleted {
            StyledText::from_plain_text(&display_text)
        } else {
            StyledText::from_markdown(&markdown_with_mentions(
                &display_text,
                &d.mentions,
                opts.mention_color,
            ))
            .unwrap_or_else(|_| StyledText::from_plain_text(&display_text))
        };

        let slint_links: Vec<ChatLinkData> = links
            .into_iter()
            .map(|l| ChatLinkData {
                url: l.url.into(),
                label: l.label.into(),
            })
            .collect();

        out.push(ChatMessageData {
            message_id: d.message_id.into(),
            sender_id: d.sender_id.into(),
            sender_name: d.sender_name.into(),
            sender_initials: d.sender_initials.into(),
            sender_avatar: sender_av,
            has_sender_avatar: has_sender_av,
            // Copy and edit work on the shown text (`@name`, not `<@user_id>`).
            text: d.display_body.into(),
            display_text: display_text.into(),
            display_styled,
            links: Rc::new(slint::VecModel::from(slint_links)).into(),
            timestamp: d.timestamp.into(),
            display_time: d.display_time.into(),
            is_group_start: d.is_group_start,
            is_continuation: d.is_continuation,
            is_system: d.is_system,
            is_unread_divider: false,
            is_gif,
            gif_image: slint::Image::default(),
            has_gif_image: false,
            gif_preview_url: gif_preview_url.into(),
            gif_width,
            gif_height,
            is_clip: false,
            clip_duration: slint::SharedString::default(),
            clip_id: slint::SharedString::default(),
            mentions_self,
            reply_to_id: d.reply_to.clone().unwrap_or_default().into(),
            reply_to_name: d.reply_to_name.clone().unwrap_or_default().into(),
            reply_preview_text: d.reply_preview.clone().unwrap_or_default().into(),
            is_edited: d.is_edited,
            is_deleted: d.is_deleted,
        });
    }

    out
}

/// Make the crew list equal to `crews`, in place.
///
/// A new model makes Slint rebuild every crew card, and a popup open on the
/// active card closes (#105). So keep the current `VecModel`: change only the
/// rows that differ, insert new crews and remove gone ones. Only a model of
/// another type, or a new order, replaces the rows.
pub fn sync_crews(app: &MainWindow, crews: Vec<CrewData>) {
    let model = app.get_crews();
    let Some(rows) = model.as_any().downcast_ref::<slint::VecModel<CrewData>>() else {
        app.set_crews(Rc::new(slint::VecModel::from(crews)).into());
        return;
    };

    // From the end, so the indices of the rows still to check hold.
    for i in (0..rows.row_count()).rev() {
        let gone = rows
            .row_data(i)
            .is_none_or(|row| !crews.iter().any(|c| c.id == row.id));
        if gone {
            rows.remove(i);
        }
    }

    for (i, crew) in crews.iter().enumerate() {
        match rows.row_data(i) {
            Some(row) if row.id == crew.id => {
                if row != *crew {
                    rows.set_row_data(i, crew.clone());
                }
            }
            Some(_)
                if (i + 1..rows.row_count())
                    .any(|j| rows.row_data(j).is_some_and(|row| row.id == crew.id)) =>
            {
                log::debug!("[crews] crew order changed, replacing the rows");
                rows.set_vec(crews.clone());
                return;
            }
            _ => rows.insert(i, crew.clone()),
        }
    }
}

pub fn apply_unread_to_crews(app: &MainWindow, tracker: &mello_core::chat::UnreadTracker) {
    let crews = app.get_crews();
    let updated: Vec<CrewData> = (0..crews.row_count())
        .map(|i| {
            let mut c = crews.row_data(i).unwrap();
            let state = tracker.get(c.id.as_str());
            c.unread_count = state.count.min(99) as i32;
            if state.count > 99 {
                // Slint shows count; use 99+ via separate property if needed — cap at 99 for now
            }
            c.unread_mention = state.has_mention;
            c
        })
        .collect();
    sync_crews(app, updated);
}

/// Scan recent messages for GIFs and kick off animated frame fetches.
pub fn fetch_gif_images_for_messages(
    model: &Rc<slint::VecModel<ChatMessageData>>,
    rt: &tokio::runtime::Handle,
    chat_anim: &crate::gif_animator::GifAnimator,
) {
    const MAX_GIF_PREFETCH: usize = 8;
    let inbox = chat_anim.inbox();
    let start = model.row_count().saturating_sub(MAX_GIF_PREFETCH);
    for i in start..model.row_count() {
        if let Some(item) = model.row_data(i) {
            let url = item.gif_preview_url.to_string();
            if item.is_gif && !url.is_empty() && !chat_anim.has_url(&url) {
                chat_anim.note_activity();
                crate::image_cache::spawn_gif_fetch(url, rt, &inbox);
            }
        }
    }
}

pub fn bento_bases(count: usize, items_per_set: usize) -> Vec<i32> {
    let num_sets = if count == 0 {
        0
    } else {
        count.div_ceil(items_per_set)
    };
    (0..num_sets).map(|i| (i * items_per_set) as i32).collect()
}

pub struct VoiceUiCtx<'a> {
    pub local_user_id: &'a str,
    pub user_avatar: &'a slint::Image,
    pub has_user_avatar: bool,
    pub cache: &'a std::collections::HashMap<String, slint::Image>,
    pub local_muted: bool,
    pub local_deafened: bool,
    pub game_lines: &'a std::collections::HashMap<String, String>,
}

pub fn current_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Humanized elapsed play time for member rows: "3m", "47m", "1h 23m".
pub fn format_elapsed_minutes(elapsed_ms: i64) -> String {
    if elapsed_ms <= 0 {
        return "1m".to_string();
    }
    let mins = ((elapsed_ms + 59_999) / 60_000).max(1);
    if mins >= 60 {
        format!("{}h {}m", mins / 60, mins % 60)
    } else {
        format!("{mins}m")
    }
}

/// Sidebar game line: "Valorant · 1h 23m", or empty when not playing.
pub fn format_game_line(game: &mello_core::presence::GamePresence, now_ms: i64) -> String {
    if game.game_name.is_empty() {
        return String::new();
    }
    let duration = game
        .started_at
        .is_empty()
        .then(String::new)
        .unwrap_or_else(|| {
            mello_core::presence::from_rfc3339_ms(&game.started_at)
                .map(|started_ms| format_elapsed_minutes(now_ms.saturating_sub(started_ms)))
                .unwrap_or_default()
        });
    if duration.is_empty() {
        game.game_name.clone()
    } else {
        format!("{} · {duration}", game.game_name)
    }
}

pub fn game_line_from_presence(
    game: Option<&mello_core::presence::GamePresence>,
    now_ms: i64,
) -> String {
    game.filter(|g| !g.game_name.is_empty())
        .map(|g| format_game_line(g, now_ms))
        .unwrap_or_default()
}

pub fn game_lines_from_members(
    members: &impl Model<Data = MemberData>,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for i in 0..members.row_count() {
        if let Some(m) = members.row_data(i) {
            if !m.game_line.is_empty() {
                out.insert(m.id.to_string(), m.game_line.to_string());
            }
        }
    }
    out
}

pub fn voice_members_to_ui(
    members: &[mello_core::crew_state::VoiceMember],
    ctx: &VoiceUiCtx<'_>,
) -> Vec<VoiceChannelMember> {
    const EPOCH_2024: i64 = 1_704_067_200;
    let mut out: Vec<VoiceChannelMember> = members
        .iter()
        .map(|m| {
            let secs = m.joined_at.unwrap_or(0) / 1000 - EPOCH_2024;
            let is_self = m.user_id == ctx.local_user_id;
            let (av, has_av) = if is_self && ctx.has_user_avatar {
                (ctx.user_avatar.clone(), true)
            } else if let Some(img) = ctx.cache.get(&m.user_id) {
                (img.clone(), true)
            } else {
                (slint::Image::default(), false)
            };
            VoiceChannelMember {
                id: m.user_id.clone().into(),
                name: m.username.clone().into(),
                initials: make_initials(&m.username).into(),
                avatar: av,
                has_avatar: has_av,
                speaking: m.speaking.unwrap_or(false),
                muted: if is_self {
                    ctx.local_muted
                } else {
                    m.muted.unwrap_or(false)
                },
                deafened: if is_self {
                    ctx.local_deafened
                } else {
                    m.deafened.unwrap_or(false)
                },
                joined_at: secs as i32,
                game_line: ctx
                    .game_lines
                    .get(&m.user_id)
                    .cloned()
                    .unwrap_or_default()
                    .into(),
            }
        })
        .collect();
    out.sort_by(|a, b| {
        let a_local = a.id == ctx.local_user_id;
        let b_local = b.id == ctx.local_user_id;
        match b_local.cmp(&a_local) {
            std::cmp::Ordering::Equal => a.joined_at.cmp(&b.joined_at),
            other => other,
        }
    });
    out
}

pub fn channel_to_ui(
    ch: &mello_core::crew_state::VoiceChannelState,
    active_channel_id: &str,
    ctx: &VoiceUiCtx<'_>,
) -> VoiceChannelData {
    let members = voice_members_to_ui(&ch.members, ctx);
    let member_count = members.len() as i32;
    let is_active = ch.id == active_channel_id;
    VoiceChannelData {
        id: ch.id.clone().into(),
        name: ch.name.clone().into(),
        member_count,
        is_default: ch.is_default,
        expanded: is_active || ch.is_default || member_count > 0,
        active: is_active,
        members: Rc::new(slint::VecModel::from(members)).into(),
    }
}

pub fn channels_to_ui(
    channels: &[mello_core::crew_state::VoiceChannelState],
    active_channel_id: &str,
    ctx: &VoiceUiCtx<'_>,
) -> Vec<VoiceChannelData> {
    channels
        .iter()
        .map(|ch| channel_to_ui(ch, active_channel_id, ctx))
        .collect()
}

/// Set the active crew and keep `active_crew_name` in step.
///
/// Discover needs the name to offer a way back to the crew the user came
/// from. Setting the id alone leaves that label stale.
pub fn set_active_crew(app: &MainWindow, crew_id: &str) {
    app.set_active_crew_id(crew_id.into());
    app.set_active_crew_name(lookup_crew_name(app, crew_id).into());
}

fn lookup_crew_name(app: &MainWindow, crew_id: &str) -> String {
    if crew_id.is_empty() {
        return String::new();
    }
    let crews = app.get_crews();
    (0..crews.row_count())
        .filter_map(|i| crews.row_data(i))
        .find(|c| c.id == crew_id)
        .map(|c| c.name.to_string())
        .unwrap_or_default()
}

pub fn update_active_crew_card(app: &MainWindow) {
    let active_id = app.get_active_crew_id();
    if active_id.is_empty() {
        return;
    }

    // The crew list can arrive after the selection, so re-resolve the name
    // whenever the list changes under us.
    let name = lookup_crew_name(app, active_id.as_str());
    if !name.is_empty() && app.get_active_crew_name() != name.as_str() {
        app.set_active_crew_name(name.into());
    }

    let members = app.get_members();
    let online_members: Vec<MemberData> = (0..members.row_count())
        .filter_map(|i| members.row_data(i))
        .filter(|m| m.online)
        .collect();

    let online_count = online_members.len().max(1) as i32;
    let voice_count = online_members.len().min(4) as i32;

    let crews = app.get_crews();
    let updated: Vec<CrewData> = (0..crews.row_count())
        .map(|i| {
            let mut c = crews.row_data(i).unwrap();
            if c.id == active_id {
                c.online_count = online_count;
                c.voice_count = voice_count;

                if let Some(m) = online_members.first() {
                    c.v0_initials = m.initials.clone();
                    c.v0_name = m.name.clone();
                    c.v0_speaking = m.speaking;
                }
                if let Some(m) = online_members.get(1) {
                    c.v1_initials = m.initials.clone();
                    c.v1_name = m.name.clone();
                    c.v1_speaking = m.speaking;
                }
                if let Some(m) = online_members.get(2) {
                    c.v2_initials = m.initials.clone();
                    c.v2_name = m.name.clone();
                    c.v2_speaking = m.speaking;
                }
                if let Some(m) = online_members.get(3) {
                    c.v3_initials = m.initials.clone();
                    c.v3_name = m.name.clone();
                    c.v3_speaking = m.speaking;
                }

                // game_count populated by game detection (future);
                // 0 shows the "quiet" sidebar state.
            }
            c
        })
        .collect();
    sync_crews(app, updated);
}

/// Update one member's speaking flag in the crew member list without rebuilding models.
pub fn set_member_speaking(app: &MainWindow, member_id: &str, speaking: bool) -> bool {
    let members = app.get_members();
    for i in 0..members.row_count() {
        if let Some(mut m) = members.row_data(i) {
            if m.id == member_id && m.speaking != speaking {
                m.speaking = speaking;
                members.set_row_data(i, m);
                update_active_crew_card(app);
                return true;
            }
        }
    }
    false
}

/// Update one voice-channel member row without rebuilding every channel model.
pub fn set_voice_member_speaking(app: &MainWindow, member_id: &str, speaking: bool) -> bool {
    let channels = app.get_voice_channels();
    for i in 0..channels.row_count() {
        let Some(ch) = channels.row_data(i) else {
            continue;
        };
        let ch_members = ch.members;
        for j in 0..ch_members.row_count() {
            if let Some(mut m) = ch_members.row_data(j) {
                if m.id == member_id && m.speaking != speaking {
                    m.speaking = speaking;
                    ch_members.set_row_data(j, m);
                    return true;
                }
            }
        }
    }
    false
}

/// Update one voice-channel member's game line without rebuilding every channel model.
pub fn set_voice_member_game_line(app: &MainWindow, member_id: &str, game_line: &str) -> bool {
    let channels = app.get_voice_channels();
    for i in 0..channels.row_count() {
        let Some(ch) = channels.row_data(i) else {
            continue;
        };
        let ch_members = ch.members;
        for j in 0..ch_members.row_count() {
            if let Some(mut m) = ch_members.row_data(j) {
                if m.id == member_id && m.game_line.as_str() != game_line {
                    m.game_line = game_line.into();
                    ch_members.set_row_data(j, m);
                    return true;
                }
            }
        }
    }
    false
}

pub fn set_level_history(app: &MainWindow, hist: &DebugHistory) {
    macro_rules! set_lh {
        ($($i:literal),*) => {
            $(
                let (level, spk) = hist.get($i);
                paste::paste! {
                    app.[<set_lh $i>](level);
                    app.[<set_sh $i>](spk);
                }
            )*
        };
    }
    set_lh!(
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26, 27, 28, 29
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use mello_core::presence::GamePresence;

    #[test]
    fn game_line_empty_without_game() {
        assert_eq!(game_line_from_presence(None, 1_700_000_000_000), "");
        let empty_name = GamePresence {
            game_name: String::new(),
            game_id: "g1".into(),
            started_at: mello_core::presence::to_rfc3339(1_700_000_000_000),
        };
        assert_eq!(
            game_line_from_presence(Some(&empty_name), 1_700_000_000_000),
            ""
        );
    }

    #[test]
    fn game_line_formats_name_and_elapsed() {
        let now = 1_700_000_000_000i64;
        let started = mello_core::presence::to_rfc3339(now - 83 * 60_000);
        let game = GamePresence {
            game_name: "Valorant".into(),
            game_id: "valorant".into(),
            started_at: started,
        };
        assert_eq!(format_game_line(&game, now), "Valorant · 1h 23m");
    }

    #[test]
    fn format_elapsed_minutes_sub_hour() {
        assert_eq!(format_elapsed_minutes(47 * 60_000), "47m");
    }
}

#[cfg(test)]
mod mention_markdown_tests {
    use super::*;
    use mello_core::chat::MentionRef;

    fn m(user_id: &str, name: &str) -> MentionRef {
        MentionRef {
            user_id: user_id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn mentions_get_the_mention_colour_and_the_rest_is_unchanged() {
        let md = markdown_with_mentions(
            "yo @Alice Baker, **bring** snacks",
            &[m("u1", "Alice Baker")],
            "#ffffff",
        );
        assert_eq!(
            md,
            r##"yo <font color="#ffffff">\@Alice Baker</font>, **bring** snacks"##
        );
        assert!(StyledText::from_markdown(&md).is_ok(), "{md}");
    }

    #[test]
    fn markup_in_a_name_stays_literal() {
        let md = markdown_with_mentions("hi @x*<u>y", &[m("u1", "x*<u>y")], "#eb4d5f");
        assert_eq!(md, r##"hi <font color="#eb4d5f">\@x\*\<u\>y</font>"##);
        assert!(StyledText::from_markdown(&md).is_ok(), "{md}");
    }

    #[test]
    fn text_without_mentions_is_untouched() {
        assert_eq!(
            markdown_with_mentions("@nobody here", &[], "#ffffff"),
            "@nobody here"
        );
    }

    #[test]
    fn color_hex_is_rrggbb() {
        assert_eq!(
            color_hex(slint::Color::from_rgb_u8(0xEB, 0x4D, 0x5F)),
            "#eb4d5f"
        );
    }
}
