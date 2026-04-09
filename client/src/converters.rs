use std::rc::Rc;

use slint::Model;

use crate::{
    ChatMessageData, CrewData, DebugHistory, MainWindow, MemberData, VoiceChannelData,
    VoiceChannelMember,
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

pub fn chat_messages_to_slint(
    raw: &[mello_core::events::ChatMessage],
    user_id: &str,
    user_avatar: &slint::Image,
    has_user_avatar: bool,
    cache: &std::collections::HashMap<String, slint::Image>,
) -> Vec<ChatMessageData> {
    mello_core::chat::prepare_messages_for_display(raw)
        .into_iter()
        .map(|d| {
            let is_gif = d.gif.is_some();
            let (gif_preview_url, gif_width, gif_height) = match &d.gif {
                Some(g) => (g.preview.clone(), g.width as i32, g.height as i32),
                None => (String::new(), 0, 0),
            };
            let is_self = d.sender_id == user_id;
            let (sender_av, has_sender_av) = if is_self && has_user_avatar {
                (user_avatar.clone(), true)
            } else if let Some(img) = cache.get(&d.sender_id) {
                (img.clone(), true)
            } else {
                (slint::Image::default(), false)
            };
            ChatMessageData {
                message_id: d.message_id.into(),
                sender_id: d.sender_id.into(),
                sender_name: d.sender_name.into(),
                sender_initials: d.sender_initials.into(),
                sender_avatar: sender_av,
                has_sender_avatar: has_sender_av,
                text: d.content.into(),
                timestamp: d.timestamp.into(),
                display_time: d.display_time.into(),
                is_group_start: d.is_group_start,
                is_continuation: d.is_continuation,
                is_system: d.is_system,
                is_gif,
                gif_image: slint::Image::default(),
                has_gif_image: false,
                gif_preview_url: gif_preview_url.into(),
                gif_width,
                gif_height,
                is_clip: false,
                clip_duration: slint::SharedString::default(),
                clip_id: slint::SharedString::default(),
            }
        })
        .collect()
}

/// Scan messages for GIFs and kick off animated frame fetches.
pub fn fetch_gif_images_for_messages(
    model: &Rc<slint::VecModel<ChatMessageData>>,
    rt: &tokio::runtime::Handle,
    chat_anim: &crate::gif_animator::GifAnimator,
) {
    let inbox = chat_anim.inbox();
    for i in 0..model.row_count() {
        if let Some(item) = model.row_data(i) {
            let url = item.gif_preview_url.to_string();
            if item.is_gif && !url.is_empty() && !chat_anim.has_url(&url) {
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

pub fn voice_members_to_ui(
    members: &[mello_core::crew_state::VoiceMember],
    local_user_id: &str,
    user_avatar: &slint::Image,
    has_user_avatar: bool,
    cache: &std::collections::HashMap<String, slint::Image>,
) -> Vec<VoiceChannelMember> {
    const EPOCH_2024: i64 = 1_704_067_200;
    let mut out: Vec<VoiceChannelMember> = members
        .iter()
        .map(|m| {
            let secs = m.joined_at.unwrap_or(0) / 1000 - EPOCH_2024;
            let is_self = m.user_id == local_user_id;
            let (av, has_av) = if is_self && has_user_avatar {
                (user_avatar.clone(), true)
            } else if let Some(img) = cache.get(&m.user_id) {
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
                joined_at: secs as i32,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        let a_local = a.id == local_user_id;
        let b_local = b.id == local_user_id;
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
    local_user_id: &str,
    user_avatar: &slint::Image,
    has_user_avatar: bool,
    cache: &std::collections::HashMap<String, slint::Image>,
) -> VoiceChannelData {
    let members = voice_members_to_ui(
        &ch.members,
        local_user_id,
        user_avatar,
        has_user_avatar,
        cache,
    );
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
    local_user_id: &str,
    user_avatar: &slint::Image,
    has_user_avatar: bool,
    cache: &std::collections::HashMap<String, slint::Image>,
) -> Vec<VoiceChannelData> {
    channels
        .iter()
        .map(|ch| {
            channel_to_ui(
                ch,
                active_channel_id,
                local_user_id,
                user_avatar,
                has_user_avatar,
                cache,
            )
        })
        .collect()
}

pub fn update_active_crew_card(app: &MainWindow) {
    let active_id = app.get_active_crew_id();
    if active_id.is_empty() {
        return;
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
    app.set_crews(Rc::new(slint::VecModel::from(updated)).into());
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
