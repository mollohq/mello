use std::cell::RefCell;
use std::collections::HashMap;

use base64::Engine;
use slint::Model;

use crate::app_context::AppContext;
use crate::converters::make_initials;
use crate::hud_manager::{HudCrew, HudMode, HudState, HudStreamCard, HudVoice, HudVoiceMember};

thread_local! {
    static AVATAR_CACHE: RefCell<HashMap<u64, String>> = RefCell::new(HashMap::new());
}

/// Build the full HUD state from the current AppContext state.
pub fn build_hud_state(ctx: &AppContext, mode: HudMode) -> HudState {
    let crew = build_crew(ctx);
    let voice = build_voice(ctx);
    let recent_messages = None;
    let stream_card = build_stream_card(ctx);

    HudState {
        mode,
        crew,
        voice,
        recent_messages,
        stream_card,
        clip_toast: None,
    }
}

const HUD_AVATAR_SIZE: u32 = 24;

fn encode_avatar(img: &slint::Image, target_size: u32) -> Option<String> {
    use std::hash::{Hash, Hasher};

    let buf = img.to_rgba8()?;
    let (w, h) = (buf.width(), buf.height());
    if w == 0 || h == 0 {
        return None;
    }

    let rgba_data = buf.as_bytes();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (w, h, target_size).hash(&mut hasher);
    rgba_data[..rgba_data.len().min(256)].hash(&mut hasher);
    let key = hasher.finish();

    AVATAR_CACHE.with(|cache| {
        let c = cache.borrow();
        if let Some(cached) = c.get(&key) {
            return Some(cached.clone());
        }
        drop(c);

        let dyn_img = image::RgbaImage::from_raw(w, h, rgba_data.to_vec())?;
        let resized = image::imageops::resize(
            &dyn_img,
            target_size,
            target_size,
            image::imageops::Lanczos3,
        );
        let b64 = base64::engine::general_purpose::STANDARD.encode(resized.as_raw());
        let result = format!("{}:{}:{}", target_size, target_size, b64);
        cache.borrow_mut().insert(key, result.clone());
        Some(result)
    })
}

fn build_crew(ctx: &AppContext) -> Option<HudCrew> {
    let active_id = ctx.app.get_active_crew_id();
    if active_id.is_empty() {
        return None;
    }

    let crews = ctx.app.get_crews();
    (0..crews.row_count()).find_map(|i| {
        let c = crews.row_data(i)?;
        if c.id == active_id {
            let avatar_rgba = if c.has_avatar {
                encode_avatar(&c.avatar, HUD_AVATAR_SIZE)
            } else {
                None
            };
            Some(HudCrew {
                initials: make_initials(&c.name),
                name: c.name.to_string(),
                online_count: c.online_count as u32,
                avatar_rgba,
            })
        } else {
            None
        }
    })
}

fn build_voice(ctx: &AppContext) -> Option<HudVoice> {
    if !ctx.app.get_in_voice() {
        return None;
    }

    let active_channel_id = ctx.active_voice_channel.borrow().clone();
    let voice_channels = ctx.app.get_voice_channels();
    let self_muted = ctx.app.get_mic_muted();
    let my_id = ctx.app.get_user_id();

    // Find the active channel
    let mut channel_name = String::new();
    let mut members = Vec::new();

    for i in 0..voice_channels.row_count() {
        if let Some(ch) = voice_channels.row_data(i) {
            if ch.id.as_str() == active_channel_id {
                channel_name = ch.name.to_string();
                for j in 0..ch.members.row_count() {
                    if let Some(m) = ch.members.row_data(j) {
                        let is_self = m.id.as_str() == my_id.as_str();
                        let avatar_rgba = if m.has_avatar {
                            encode_avatar(&m.avatar, HUD_AVATAR_SIZE)
                        } else {
                            None
                        };
                        members.push(HudVoiceMember {
                            id: m.id.to_string(),
                            display_name: m.name.to_string(),
                            initials: m.initials.to_string(),
                            avatar_rgba,
                            speaking: m.speaking,
                            muted: if is_self { self_muted } else { false },
                            is_self,
                        });
                    }
                }
                break;
            }
        }
    }

    if channel_name.is_empty() && members.is_empty() {
        return None;
    }

    Some(HudVoice {
        channel_name,
        members,
        self_muted,
    })
}

fn build_stream_card(ctx: &AppContext) -> Option<HudStreamCard> {
    if ctx.app.get_is_hosting() {
        let streamer = ctx.app.get_streamer_name().to_string();
        let title = ctx.app.get_stream_label().to_string();
        return Some(HudStreamCard { streamer, title });
    }

    let streamer = ctx.app.get_active_streamer_name().to_string();
    if streamer.is_empty() {
        return None;
    }
    let title = ctx.app.get_stream_label().to_string();
    Some(HudStreamCard { streamer, title })
}
