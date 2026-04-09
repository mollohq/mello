use slint::Model;

use crate::app_context::AppContext;
use crate::converters::make_initials;
use crate::hud_manager::{
    HudChatMessage, HudCrew, HudMode, HudState, HudStreamCard, HudVoice, HudVoiceMember,
};

/// Build the full HUD state from the current AppContext state.
pub fn build_hud_state(ctx: &AppContext, mode: HudMode) -> HudState {
    let crew = build_crew(ctx);
    let voice = build_voice(ctx);
    let recent_messages = if mode == HudMode::MiniPlayer {
        build_recent_messages(ctx)
    } else {
        None
    };
    let stream_card = if mode == HudMode::MiniPlayer {
        build_stream_card(ctx)
    } else {
        None
    };

    HudState {
        mode,
        crew,
        voice,
        recent_messages,
        stream_card,
        clip_toast: None,
    }
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
            Some(HudCrew {
                initials: make_initials(&c.name),
                name: c.name.to_string(),
                online_count: c.online_count as u32,
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
                        members.push(HudVoiceMember {
                            id: m.id.to_string(),
                            display_name: m.name.to_string(),
                            initials: m.initials.to_string(),
                            avatar_rgba: None, // TODO: pre-rasterize from avatar cache
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

fn build_recent_messages(ctx: &AppContext) -> Option<Vec<HudChatMessage>> {
    let messages = ctx.chat_messages.borrow();
    if messages.is_empty() {
        return None;
    }

    let recent: Vec<HudChatMessage> = messages
        .iter()
        .rev()
        .take(2)
        .map(|m| HudChatMessage {
            display_name: m.sender_name.clone(),
            text: m.content.clone(),
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    if recent.is_empty() {
        None
    } else {
        Some(recent)
    }
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
