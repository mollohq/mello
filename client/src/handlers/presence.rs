use std::rc::Rc;

use base64::Engine as _;
use mello_core::{Command, Event};
use slint::Model;

use crate::app_context::AppContext;
use crate::converters::{
    channels_to_ui, chat_messages_to_slint, make_initials, update_active_crew_card,
};
use crate::{avatar, CrewData, MemberData};

pub fn handle(ctx: &AppContext, event: Event) {
    match event {
        Event::UserAvatarLoaded { user_id, data } => {
            log::info!(
                "[avatar] UI: received avatar for user {} ({} bytes)",
                user_id,
                data.len()
            );
            let slint_img = if let Ok(decoded) =
                base64::engine::general_purpose::STANDARD.decode(&data)
            {
                if let Ok(dyn_img) = image::load_from_memory(&decoded) {
                    let rgba = dyn_img.to_rgba8();
                    let (w, h) = (rgba.width(), rgba.height());
                    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        &rgba.into_raw(),
                        w,
                        h,
                    );
                    slint::Image::from_rgba8(buf)
                } else if let Some(rgba) = avatar::rasterize_svg(&String::from_utf8_lossy(&decoded))
                {
                    avatar::rgba_to_image(&rgba, avatar::RENDER_SIZE, avatar::RENDER_SIZE)
                } else {
                    log::error!("[avatar] failed to decode avatar for user {}", user_id);
                    return;
                }
            } else if let Some(rgba) = avatar::rasterize_svg(&data) {
                avatar::rgba_to_image(&rgba, avatar::RENDER_SIZE, avatar::RENDER_SIZE)
            } else {
                log::error!("[avatar] failed to decode avatar for user {}", user_id);
                return;
            };
            let is_local = user_id == ctx.app.get_user_id().as_str();
            if is_local {
                ctx.app.set_user_avatar(slint_img.clone());
                ctx.app.set_has_user_avatar(true);
            }

            ctx.avatar_cache
                .borrow_mut()
                .insert(user_id.clone(), slint_img.clone());

            {
                let uid = ctx.app.get_user_id().to_string();
                let uav = ctx.app.get_user_avatar();
                let huav = ctx.app.get_has_user_avatar();
                let display = chat_messages_to_slint(
                    &ctx.chat_messages.borrow(),
                    &uid,
                    &uav,
                    huav,
                    &ctx.avatar_cache.borrow(),
                );
                let rc = Rc::new(slint::VecModel::from(display));
                ctx.app.set_messages(rc.into());
            }

            {
                let members_model = ctx.app.get_members();
                for i in 0..members_model.row_count() {
                    if let Some(mut m) = members_model.row_data(i) {
                        if m.id == user_id.as_str() {
                            m.avatar = slint_img.clone();
                            m.has_avatar = true;
                            members_model.set_row_data(i, m);
                            break;
                        }
                    }
                }
            }

            {
                let vc = ctx.app.get_voice_channels();
                for i in 0..vc.row_count() {
                    if let Some(ch) = vc.row_data(i) {
                        let members = ch.members;
                        let mut changed = false;
                        for j in 0..members.row_count() {
                            if let Some(mut m) = members.row_data(j) {
                                if m.id == user_id.as_str() {
                                    m.avatar = slint_img.clone();
                                    m.has_avatar = true;
                                    members.set_row_data(j, m);
                                    changed = true;
                                    break;
                                }
                            }
                        }
                        if changed {
                            let mut ch_copy = vc.row_data(i).unwrap();
                            ch_copy.members = members;
                            vc.set_row_data(i, ch_copy);
                        }
                    }
                }
            }
        }
        Event::MemberJoined { member, .. } => {
            let current = ctx.app.get_members();
            let initials = make_initials(&member.display_name);
            let is_self = member.id == ctx.app.get_user_id().as_str();
            let cache = ctx.avatar_cache.borrow();
            let cached = cache.get(&member.id);
            let (av, has_av) = if is_self && ctx.app.get_has_user_avatar() {
                (ctx.app.get_user_avatar(), true)
            } else if let Some(img) = cached {
                (img.clone(), true)
            } else {
                (slint::Image::default(), false)
            };
            let member_id = member.id.clone();
            drop(cache);
            let new_member = MemberData {
                id: member.id.into(),
                name: member.display_name.into(),
                initials: initials.into(),
                avatar: av,
                has_avatar: has_av,
                online: true,
                speaking: false,
            };
            let mut members: Vec<MemberData> = (0..current.row_count())
                .map(|i| current.row_data(i).unwrap())
                .collect();
            if !members.iter().any(|m| m.id == new_member.id) {
                members.push(new_member);
            }
            let rc = Rc::new(slint::VecModel::from(members));
            if !is_self && !has_av && !ctx.avatar_cache.borrow().contains_key(&member_id) {
                let _ = ctx
                    .cmd_tx
                    .try_send(Command::FetchUserAvatar { user_id: member_id });
            }
            ctx.app.set_members(rc.into());
            update_active_crew_card(&ctx.app);
        }
        Event::MemberLeft { member_id, .. } => {
            let current = ctx.app.get_members();
            let members: Vec<MemberData> = (0..current.row_count())
                .map(|i| current.row_data(i).unwrap())
                .filter(|m| m.id != member_id.as_str())
                .collect();
            let rc = Rc::new(slint::VecModel::from(members));
            ctx.app.set_members(rc.into());
            update_active_crew_card(&ctx.app);
        }
        Event::PresenceUpdated { user_id, online } => {
            let current = ctx.app.get_members();
            let members: Vec<MemberData> = (0..current.row_count())
                .map(|i| {
                    let mut m = current.row_data(i).unwrap();
                    if m.id == user_id.as_str() {
                        m.online = online;
                    }
                    m
                })
                .collect();
            let rc = Rc::new(slint::VecModel::from(members));
            ctx.app.set_members(rc.into());
        }
        Event::PresenceChanged { change } => {
            log::debug!(
                "UI: presence change user={} in crew={}",
                change.user_id,
                change.crew_id
            );
            let active_id = ctx.app.get_active_crew_id();
            if active_id == change.crew_id.as_str() {
                let current = ctx.app.get_members();
                let is_online =
                    change.presence.status != mello_core::presence::PresenceStatus::Offline;
                let members: Vec<MemberData> = (0..current.row_count())
                    .map(|i| {
                        let mut m = current.row_data(i).unwrap();
                        if m.id == change.user_id.as_str() {
                            m.online = is_online;
                        }
                        m
                    })
                    .collect();
                ctx.app
                    .set_members(Rc::new(slint::VecModel::from(members)).into());
                update_active_crew_card(&ctx.app);
            }
        }
        Event::CrewStateLoaded { state } => {
            log::info!(
                "UI: crew state loaded for {} (online={}, total={}, voice_channels={})",
                state.crew_id,
                state.counts.online,
                state.counts.total,
                state.voice_channels.len()
            );

            let crews = ctx.app.get_crews();
            let updated: Vec<CrewData> = (0..crews.row_count())
                .map(|i| {
                    let mut c = crews.row_data(i).unwrap();
                    if c.id == state.crew_id.as_str() {
                        if !state.name.is_empty() {
                            c.name = state.name.clone().into();
                        }
                        c.member_count = state.counts.total as i32;
                        c.online_count = state.counts.online as i32;
                        c.sfu_enabled = state.sfu_enabled;
                        let vlen = state.voice.members.len().min(4);
                        c.voice_count = vlen as i32;
                        if let Some(m) = state.voice.members.first() {
                            c.v0_name = m.username.clone().into();
                            c.v0_initials = make_initials(&m.username).into();
                            c.v0_speaking = m.speaking.unwrap_or(false);
                        }
                        if let Some(m) = state.voice.members.get(1) {
                            c.v1_name = m.username.clone().into();
                            c.v1_initials = make_initials(&m.username).into();
                            c.v1_speaking = m.speaking.unwrap_or(false);
                        }
                        if let Some(m) = state.voice.members.get(2) {
                            c.v2_name = m.username.clone().into();
                            c.v2_initials = make_initials(&m.username).into();
                            c.v2_speaking = m.speaking.unwrap_or(false);
                        }
                        if let Some(m) = state.voice.members.get(3) {
                            c.v3_name = m.username.clone().into();
                            c.v3_initials = make_initials(&m.username).into();
                            c.v3_speaking = m.speaking.unwrap_or(false);
                        }
                        if let Some(ref stream) = state.stream {
                            c.has_stream = stream.active;
                            c.stream_name = stream.title.clone().unwrap_or_default().into();
                        }
                        // Active games
                        let glen = state.active_games.len().min(5);
                        c.game_count = glen as i32;
                        if let Some(g) = state.active_games.first() {
                            c.g0_name = g.game_name.clone().into();
                            c.g0_initial = g.short_name.clone().into();
                            c.g0_count = g.players.len() as i32;
                        }
                        if let Some(g) = state.active_games.get(1) {
                            c.g1_name = g.game_name.clone().into();
                            c.g1_initial = g.short_name.clone().into();
                            c.g1_count = g.players.len() as i32;
                        }
                        if let Some(g) = state.active_games.get(2) {
                            c.g2_name = g.game_name.clone().into();
                            c.g2_initial = g.short_name.clone().into();
                            c.g2_count = g.players.len() as i32;
                        }
                        if let Some(g) = state.active_games.get(3) {
                            c.g3_name = g.game_name.clone().into();
                            c.g3_initial = g.short_name.clone().into();
                            c.g3_count = g.players.len() as i32;
                        }
                        if let Some(g) = state.active_games.get(4) {
                            c.g4_name = g.game_name.clone().into();
                            c.g4_initial = g.short_name.clone().into();
                            c.g4_count = g.players.len() as i32;
                        }

                        c.msg_count = state.recent_messages.len().min(2) as i32;
                        if let Some(m) = state.recent_messages.first() {
                            c.m0_author = m.username.clone().into();
                            c.m0_text = m.preview.clone().into();
                        }
                        if let Some(m) = state.recent_messages.get(1) {
                            c.m1_author = m.username.clone().into();
                            c.m1_text = m.preview.clone().into();
                        }
                    }
                    c
                })
                .collect();
            ctx.app
                .set_crews(Rc::new(slint::VecModel::from(updated)).into());

            if ctx.app.get_active_crew_id() == state.crew_id.as_str() {
                if let Some(ref stream) = state.stream {
                    if stream.active {
                        let sid = stream.streamer_id.clone().unwrap_or_default();
                        let sname = stream.streamer_username.clone().unwrap_or_default();
                        let local_id = ctx.app.get_user_id().to_string();
                        if sid != local_id {
                            ctx.app.set_active_streamer_id(sid.into());
                            ctx.app.set_active_streamer_name(sname.into());
                            ctx.app.set_active_stream_session_id(
                                stream.stream_id.clone().unwrap_or_default().into(),
                            );
                            ctx.app.set_active_stream_width(stream.width as i32);
                            ctx.app.set_active_stream_height(stream.height as i32);
                        } else {
                            ctx.app.set_active_streamer_id("".into());
                            ctx.app.set_active_streamer_name("".into());
                            ctx.app.set_active_stream_session_id("".into());
                            ctx.app.set_active_stream_width(0);
                            ctx.app.set_active_stream_height(0);
                        }
                    } else {
                        ctx.app.set_active_streamer_id("".into());
                        ctx.app.set_active_streamer_name("".into());
                        ctx.app.set_active_stream_session_id("".into());
                        ctx.app.set_active_stream_width(0);
                        ctx.app.set_active_stream_height(0);
                    }
                } else {
                    ctx.app.set_active_streamer_id("".into());
                    ctx.app.set_active_streamer_name("".into());
                    ctx.app.set_active_stream_session_id("".into());
                    ctx.app.set_active_stream_width(0);
                    ctx.app.set_active_stream_height(0);
                }
            }

            if ctx.app.get_active_crew_id() == state.crew_id.as_str() {
                let avc_id = if ctx.app.get_in_voice() {
                    let current_avc = ctx.active_voice_channel.borrow().clone();
                    if current_avc.is_empty() {
                        let default_id = state
                            .voice_channels
                            .iter()
                            .find(|ch| ch.is_default)
                            .or_else(|| state.voice_channels.first())
                            .map(|ch| ch.id.clone())
                            .unwrap_or_default();
                        *ctx.active_voice_channel.borrow_mut() = default_id.clone();
                        default_id
                    } else {
                        current_avc
                    }
                } else {
                    String::new()
                };
                let local_id = ctx.app.get_user_id();
                let uav = ctx.app.get_user_avatar();
                let huav = ctx.app.get_has_user_avatar();
                let vc_data = channels_to_ui(
                    &state.voice_channels,
                    &avc_id,
                    &local_id,
                    &uav,
                    huav,
                    &ctx.avatar_cache.borrow(),
                );
                ctx.app
                    .set_voice_channels(Rc::new(slint::VecModel::from(vc_data)).into());

                ctx.app.set_can_manage_channels(state.my_role <= 1);
                // Catch-up is now fetched in mello-core handle_select_crew
                // (before set_active_crew) to avoid the last_seen race.
            }

            let local_uid = ctx.app.get_user_id().to_string();
            if let Some(ref members) = state.members {
                let mut need_fetch: Vec<String> = Vec::new();
                for cm in members {
                    if cm.user_id == local_uid {
                        continue;
                    }
                    if ctx.avatar_cache.borrow().contains_key(&cm.user_id) {
                        continue;
                    }
                    if let Some(ref avatar_str) = cm.avatar {
                        if !avatar_str.is_empty() {
                            let parsed: Option<String> =
                                serde_json::from_str::<serde_json::Value>(avatar_str)
                                    .ok()
                                    .and_then(|v| v.get("data")?.as_str().map(String::from));
                            if let Some(data) = parsed {
                                let img = if let Ok(decoded) =
                                    base64::engine::general_purpose::STANDARD.decode(&data)
                                {
                                    if let Ok(dyn_img) = image::load_from_memory(&decoded) {
                                        let rgba = dyn_img.to_rgba8();
                                        let (w, h) = (rgba.width(), rgba.height());
                                        let buf = slint::SharedPixelBuffer::<
                                            slint::Rgba8Pixel,
                                        >::clone_from_slice(
                                            &rgba.into_raw(), w, h
                                        );
                                        Some(slint::Image::from_rgba8(buf))
                                    } else {
                                        avatar::rasterize_svg(&String::from_utf8_lossy(&decoded))
                                            .map(|rgba| {
                                                avatar::rgba_to_image(
                                                    &rgba,
                                                    avatar::RENDER_SIZE,
                                                    avatar::RENDER_SIZE,
                                                )
                                            })
                                    }
                                } else {
                                    avatar::rasterize_svg(&data).map(|rgba| {
                                        avatar::rgba_to_image(
                                            &rgba,
                                            avatar::RENDER_SIZE,
                                            avatar::RENDER_SIZE,
                                        )
                                    })
                                };
                                if let Some(img) = img {
                                    ctx.avatar_cache
                                        .borrow_mut()
                                        .insert(cm.user_id.clone(), img);
                                    continue;
                                }
                            }
                        }
                    }
                    need_fetch.push(cm.user_id.clone());
                }
                if !need_fetch.is_empty() {
                    log::info!(
                        "[avatar] fetching avatars for {} users not in crew state",
                        need_fetch.len()
                    );
                    let _ = ctx.cmd_tx.try_send(Command::FetchUserAvatars {
                        user_ids: need_fetch,
                    });
                }
            }
        }
        Event::SidebarUpdated {
            crews: sidebar_crews,
        } => {
            log::info!("UI: sidebar updated for {} crews", sidebar_crews.len());

            let current = ctx.app.get_crews();
            let mut updated: Vec<CrewData> = (0..current.row_count())
                .map(|i| current.row_data(i).unwrap())
                .collect();

            for sc in &sidebar_crews {
                let c = if let Some(c) = updated.iter_mut().find(|c| c.id == sc.crew_id.as_str()) {
                    c
                } else {
                    updated.push(CrewData {
                        id: sc.crew_id.clone().into(),
                        name: sc.name.clone().into(),
                        member_count: sc.counts.total as i32,
                        ..Default::default()
                    });
                    updated.last_mut().unwrap()
                };

                c.online_count = sc.counts.online as i32;
                c.sfu_enabled = sc.sfu_enabled;
                if let Some(ref voice) = sc.voice {
                    let vlen = voice.members.len().min(4);
                    c.voice_count = vlen as i32;
                    if let Some(m) = voice.members.first() {
                        c.v0_name = m.username.clone().into();
                        c.v0_initials = make_initials(&m.username).into();
                    }
                    if let Some(m) = voice.members.get(1) {
                        c.v1_name = m.username.clone().into();
                        c.v1_initials = make_initials(&m.username).into();
                    }
                    if let Some(m) = voice.members.get(2) {
                        c.v2_name = m.username.clone().into();
                        c.v2_initials = make_initials(&m.username).into();
                    }
                    if let Some(m) = voice.members.get(3) {
                        c.v3_name = m.username.clone().into();
                        c.v3_initials = make_initials(&m.username).into();
                    }
                }
                if let Some(ref stream) = sc.stream {
                    c.has_stream = stream.active;
                    c.stream_name = stream.title.clone().unwrap_or_default().into();
                    if c.id == ctx.app.get_active_crew_id() {
                        if stream.active {
                            let sid = stream.streamer_id.clone().unwrap_or_default();
                            let sname = stream.streamer_username.clone().unwrap_or_default();
                            let local_id = ctx.app.get_user_id().to_string();
                            if sid != local_id {
                                ctx.app.set_active_streamer_id(sid.into());
                                ctx.app.set_active_streamer_name(sname.into());
                                ctx.app.set_active_stream_session_id(
                                    stream.stream_id.clone().unwrap_or_default().into(),
                                );
                                ctx.app.set_active_stream_width(stream.width as i32);
                                ctx.app.set_active_stream_height(stream.height as i32);
                            }
                        } else {
                            ctx.app.set_active_streamer_id("".into());
                            ctx.app.set_active_streamer_name("".into());
                            ctx.app.set_active_stream_session_id("".into());
                            ctx.app.set_active_stream_width(0);
                            ctx.app.set_active_stream_height(0);
                        }
                    }
                }
                c.msg_count = sc.recent_messages.len().min(2) as i32;
                if let Some(m) = sc.recent_messages.first() {
                    c.m0_author = m.username.clone().into();
                    c.m0_text = m.preview.clone().into();
                }
                if let Some(m) = sc.recent_messages.get(1) {
                    c.m1_author = m.username.clone().into();
                    c.m1_text = m.preview.clone().into();
                }
            }
            ctx.app
                .set_crews(Rc::new(slint::VecModel::from(updated)).into());
        }
        Event::CrewEventReceived { event } => {
            log::info!("UI: crew event {} in crew {}", event.event, event.crew_id);
            let _ = ctx.cmd_tx.try_send(Command::SetActiveCrew {
                crew_id: event.crew_id,
            });
        }
        Event::CatchupLoaded { response } => {
            log::info!(
                "UI: catchup loaded for crew {} ({} events)",
                response.crew_id,
                response.event_count
            );
            let crews = ctx.app.get_crews();
            let updated: Vec<CrewData> = (0..crews.row_count())
                .map(|i| {
                    let mut c = crews.row_data(i).unwrap();
                    if c.id == response.crew_id.as_str() {
                        c.has_catchup = response.has_events;
                        c.catchup_text = response.catchup_text.clone().into();
                    }
                    c
                })
                .collect();
            ctx.app
                .set_crews(Rc::new(slint::VecModel::from(updated)).into());
        }
        Event::MomentPosted { event_id } => {
            log::info!("UI: moment posted, event_id={}", event_id);
        }
        Event::MomentPostFailed { reason } => {
            log::warn!("UI: moment post failed: {}", reason);
        }
        Event::ProfileUpdated {
            display_name,
            avatar_data,
        } => {
            log::info!("[profile] UI: profile updated, name={}", display_name);
            ctx.app.set_user_name(display_name.clone().into());
            ctx.app
                .set_user_initials(make_initials(&display_name).into());
            if let Some(data) = avatar_data {
                let slint_img = if let Ok(decoded) =
                    base64::engine::general_purpose::STANDARD.decode(&data)
                {
                    if let Ok(dyn_img) = image::load_from_memory(&decoded) {
                        let rgba = dyn_img.to_rgba8();
                        let (w, h) = (rgba.width(), rgba.height());
                        let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                            &rgba.into_raw(),
                            w,
                            h,
                        );
                        Some(slint::Image::from_rgba8(buf))
                    } else {
                        avatar::rasterize_svg(&String::from_utf8_lossy(&decoded)).map(|rgba| {
                            avatar::rgba_to_image(&rgba, avatar::RENDER_SIZE, avatar::RENDER_SIZE)
                        })
                    }
                } else {
                    avatar::rasterize_svg(&data).map(|rgba| {
                        avatar::rgba_to_image(&rgba, avatar::RENDER_SIZE, avatar::RENDER_SIZE)
                    })
                };
                if let Some(img) = slint_img {
                    ctx.app.set_user_avatar(img.clone());
                    ctx.app.set_has_user_avatar(true);
                    let uid = ctx.app.get_user_id().to_string();
                    ctx.avatar_cache.borrow_mut().insert(uid, img);
                }
            }
            ctx.app
                .set_profile_nickname_value(display_name.clone().into());
            ctx.app.set_profile_selected_avatar(-1);
        }
        _ => {}
    }
}
