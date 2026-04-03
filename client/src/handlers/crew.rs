use std::rc::Rc;

use base64::Engine as _;
use mello_core::{Command, Event};
use slint::Model;

use crate::app_context::AppContext;
use crate::converters::{bento_bases, update_active_crew_card};
use crate::{ChatMessageData, CrewData, DiscoverCrewData, SearchUserData, VoiceChannelData};

pub fn handle(ctx: &AppContext, event: Event) {
    match event {
        Event::DiscoverCrewsLoaded { crews, cursor } => {
            let is_append = *ctx.discover_loading.borrow();
            *ctx.discover_loading.borrow_mut() = false;
            *ctx.discover_cursor.borrow_mut() = cursor;

            log::info!(
                "[discover] loaded {} crews, append={}, has_more={}",
                crews.len(),
                is_append,
                ctx.discover_cursor.borrow().is_some()
            );

            let avatar_crew_ids: Vec<String> = crews
                .iter()
                .filter(|c| c.avatar_url.is_some())
                .map(|c| c.id.clone())
                .collect();

            let step = ctx.app.get_onboarding_step();
            if step <= 3 && !is_append {
                let onboard_count = crews.len().min(5);
                let model: Vec<CrewData> = crews[..onboard_count]
                    .iter()
                    .map(|c| CrewData {
                        id: c.id.clone().into(),
                        name: c.name.clone().into(),
                        description: c.description.clone().into(),
                        member_count: c.member_count,
                        online_count: 0,
                        ..Default::default()
                    })
                    .collect();
                let bases = bento_bases(onboard_count, 5);
                let rc = Rc::new(slint::VecModel::from(model));
                ctx.app.set_discover_crews(rc.into());
                ctx.app
                    .set_onboarding_bento_bases(Rc::new(slint::VecModel::from(bases)).into());
                if step == 0 || step == 1 {
                    ctx.app.set_onboarding_step(1);
                }
            }

            if is_append {
                let existing = ctx.app.get_discover_crews_list();
                let mut all: Vec<DiscoverCrewData> = (0..existing.row_count())
                    .filter_map(|i| existing.row_data(i))
                    .collect();
                for c in &crews {
                    all.push(DiscoverCrewData {
                        id: c.id.clone().into(),
                        name: c.name.clone().into(),
                        description: c.description.clone().into(),
                        member_count: c.member_count,
                        online_count: 0,
                        open: true,
                        ..Default::default()
                    });
                }
                let total = all.len();
                ctx.app
                    .set_discover_crews_list(Rc::new(slint::VecModel::from(all)).into());
                ctx.app.set_discover_bento_bases(
                    Rc::new(slint::VecModel::from(bento_bases(total, 7))).into(),
                );
            } else {
                let discover_model: Vec<DiscoverCrewData> = crews
                    .iter()
                    .map(|c| DiscoverCrewData {
                        id: c.id.clone().into(),
                        name: c.name.clone().into(),
                        description: c.description.clone().into(),
                        member_count: c.member_count,
                        online_count: 0,
                        open: true,
                        ..Default::default()
                    })
                    .collect();
                let count = discover_model.len();
                ctx.app
                    .set_discover_crews_list(Rc::new(slint::VecModel::from(discover_model)).into());
                ctx.app.set_discover_bento_bases(
                    Rc::new(slint::VecModel::from(bento_bases(count, 7))).into(),
                );
            }

            if !avatar_crew_ids.is_empty() {
                let _ = ctx.cmd_tx.try_send(Command::FetchCrewAvatars {
                    crew_ids: avatar_crew_ids,
                });
            }
        }
        Event::CrewsLoaded { crews } => {
            log::info!("[crews] loaded {} crews", crews.len());
            for c in &crews {
                log::info!(
                    "[crews]   id={} name={:?} avatar_url={:?}",
                    c.id,
                    c.name,
                    c.avatar_url
                );
            }
            let crew_ids: Vec<String> = crews.iter().map(|c| c.id.clone()).collect();
            let avatar_crew_ids: Vec<String> = crews
                .iter()
                .filter(|c| c.avatar_url.is_some())
                .map(|c| c.id.clone())
                .collect();
            log::info!(
                "[crews] {} crews have avatar_url, fetching avatars",
                avatar_crew_ids.len()
            );

            let current = ctx.app.get_crews();
            let mut existing: std::collections::HashMap<String, CrewData> = (0..current
                .row_count())
                .filter_map(|i| current.row_data(i))
                .map(|c| (c.id.to_string(), c))
                .collect();

            let model: Vec<CrewData> = crews
                .into_iter()
                .map(|c| {
                    if let Some(mut prev) = existing.remove(&c.id) {
                        prev.name = c.name.into();
                        prev.description = c.description.into();
                        prev.member_count = c.member_count;
                        prev
                    } else {
                        CrewData {
                            id: c.id.clone().into(),
                            name: c.name.into(),
                            description: c.description.into(),
                            member_count: c.member_count,
                            online_count: 0,
                            ..Default::default()
                        }
                    }
                })
                .collect();
            let rc = Rc::new(slint::VecModel::from(model));
            ctx.app.set_crews(rc.into());

            if !avatar_crew_ids.is_empty() {
                let _ = ctx.cmd_tx.try_send(Command::FetchCrewAvatars {
                    crew_ids: avatar_crew_ids,
                });
            }

            if ctx.app.get_active_crew_id().is_empty() {
                let last = ctx.settings.borrow().last_crew_id.clone();
                let target = match &last {
                    Some(id) if crew_ids.contains(id) => {
                        log::info!("[auth] restoring last crew: {}", id);
                        Some(id.clone())
                    }
                    _ => crew_ids.first().map(|id| {
                        log::info!("[auth] auto-selecting first crew: {}", id);
                        id.clone()
                    }),
                };
                if let Some(id) = target {
                    let _ = ctx.cmd_tx.try_send(Command::SelectCrew { crew_id: id });
                }
            }
        }
        Event::CrewCreated { crew, invite_code } => {
            log::info!(
                "UI: crew created: {} invite_code={:?}",
                crew.name,
                invite_code
            );
            *ctx.new_crew_avatar_b64.lock().unwrap() = None;
            ctx.invited_users.borrow_mut().clear();
            if let Some(code) = invite_code {
                ctx.app.set_new_crew_invite_code(code.into());
                ctx.app.set_new_crew_created(true);
            } else {
                ctx.app.set_new_crew_open(false);
                ctx.app.set_new_crew_has_avatar(false);
            }
        }
        Event::CrewCreateFailed { reason } => {
            log::warn!("UI: crew creation failed: {}", reason);
        }
        Event::UserSearchResults { users } => {
            let model: Vec<SearchUserData> = users
                .into_iter()
                .map(|u| SearchUserData {
                    id: u.id.into(),
                    display_name: u.display_name.into(),
                    is_friend: u.is_friend,
                })
                .collect();
            let rc_model = Rc::new(slint::VecModel::from(model));
            ctx.app
                .set_new_crew_search_results(slint::ModelRc::from(rc_model));
        }
        Event::CrewAvatarLoaded { crew_id, data } => {
            log::info!(
                "[avatar] UI: received avatar for crew {} ({} bytes base64)",
                crew_id,
                data.len()
            );
            let decoded = match base64::engine::general_purpose::STANDARD.decode(&data) {
                Ok(d) => {
                    log::debug!(
                        "[avatar] decoded {} raw bytes for crew {}",
                        d.len(),
                        crew_id
                    );
                    d
                }
                Err(e) => {
                    log::error!(
                        "[avatar] failed to decode base64 for crew {}: {}",
                        crew_id,
                        e
                    );
                    return;
                }
            };
            let img = match image::load_from_memory(&decoded) {
                Ok(i) => i,
                Err(e) => {
                    log::error!(
                        "[avatar] failed to decode image for crew {}: {}",
                        crew_id,
                        e
                    );
                    return;
                }
            };
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &rgba.into_raw(),
                w,
                h,
            );
            let slint_img = slint::Image::from_rgba8(buf);

            let crews = ctx.app.get_crews();
            for i in 0..crews.row_count() {
                if let Some(mut c) = crews.row_data(i) {
                    if c.id == crew_id.as_str() {
                        c.avatar = slint_img.clone();
                        c.has_avatar = true;
                        crews.set_row_data(i, c);
                        break;
                    }
                }
            }

            let discover = ctx.app.get_discover_crews_list();
            for i in 0..discover.row_count() {
                if let Some(mut c) = discover.row_data(i) {
                    if c.id == crew_id.as_str() {
                        c.avatar = slint_img.clone();
                        c.has_avatar = true;
                        discover.set_row_data(i, c);
                        break;
                    }
                }
            }

            let onboarding = ctx.app.get_discover_crews();
            for i in 0..onboarding.row_count() {
                if let Some(mut c) = onboarding.row_data(i) {
                    if c.id == crew_id.as_str() {
                        c.avatar = slint_img;
                        c.has_avatar = true;
                        onboarding.set_row_data(i, c);
                        break;
                    }
                }
            }
        }
        Event::CrewJoined { crew_id } => {
            log::info!("UI: joined crew {}", crew_id);
            ctx.app.set_show_discover(false);
            *ctx.active_voice_channel.borrow_mut() = String::new();
            let old_id = ctx.app.get_active_crew_id();
            if !old_id.is_empty() && old_id != crew_id.as_str() {
                let crews = ctx.app.get_crews();
                let cleared: Vec<CrewData> = (0..crews.row_count())
                    .map(|i| {
                        let mut c = crews.row_data(i).unwrap();
                        if c.id == old_id {
                            c.voice_count = 0;
                            c.v0_speaking = false;
                            c.v1_speaking = false;
                            c.v2_speaking = false;
                            c.v3_speaking = false;
                        }
                        c
                    })
                    .collect();
                ctx.app
                    .set_crews(Rc::new(slint::VecModel::from(cleared)).into());
            }
            ctx.app.set_active_crew_id(crew_id.clone().into());
            let empty_channels: Vec<VoiceChannelData> = vec![];
            ctx.app
                .set_voice_channels(Rc::new(slint::VecModel::from(empty_channels)).into());
            ctx.chat_messages.borrow_mut().clear();
            let empty: Vec<ChatMessageData> = vec![];
            let rc = Rc::new(slint::VecModel::from(empty));
            ctx.app.set_messages(rc.into());
            update_active_crew_card(&ctx.app);
            // Catch-up is now fetched in mello-core handle_select_crew
            // (before set_active_crew) to avoid the last_seen race.
            let mut s = ctx.settings.borrow_mut();
            s.last_crew_id = Some(crew_id);
            s.save();
        }
        Event::CrewLeft { crew_id } => {
            log::info!("UI: left crew {}", crew_id);
            let crews = ctx.app.get_crews();
            let updated: Vec<CrewData> = (0..crews.row_count())
                .map(|i| {
                    let mut c = crews.row_data(i).unwrap();
                    if c.id == crew_id.as_str() {
                        c.online_count = 0;
                    }
                    c
                })
                .collect();
            ctx.app
                .set_crews(Rc::new(slint::VecModel::from(updated)).into());
            ctx.app.set_active_crew_id("".into());
        }
        _ => {}
    }
}
