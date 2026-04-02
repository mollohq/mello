use std::rc::Rc;

use mello_core::Command;
use slint::{ComponentHandle, Model};

use crate::app_context::AppContext;
use crate::callbacks::onboarding::{load_avatar_grid, start_ambient_shuffle};
use crate::{avatar, SearchUserData};

pub fn wire(ctx: &AppContext) {
    // --- Crew selection ---
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_select_crew(move |crew_id| {
            let _ = cmd.try_send(Command::SelectCrew {
                crew_id: crew_id.to_string(),
            });
        });
    }

    // --- New crew modal: avatar picker ---
    {
        let app_weak = ctx.app.as_weak();
        let avatar_b64 = ctx.new_crew_avatar_b64.clone();
        let rt_handle = ctx.rt.clone();
        ctx.app.on_new_crew_avatar_pick(move || {
            let app_weak = app_weak.clone();
            let avatar_b64 = avatar_b64.clone();
            rt_handle.spawn(async move {
                let file = rfd::AsyncFileDialog::new()
                    .add_filter("Images", &["jpg", "jpeg", "png", "gif"])
                    .pick_file()
                    .await;
                let Some(file) = file else { return };
                let bytes = file.read().await;
                if bytes.len() > 20 * 1024 * 1024 {
                    log::warn!("Avatar file too large: {} bytes", bytes.len());
                    return;
                }
                let Ok(img) = image::load_from_memory(&bytes) else {
                    log::warn!("Failed to decode avatar image");
                    return;
                };
                let resized = img.resize(256, 256, image::imageops::FilterType::Lanczos3);
                let rgba = resized.to_rgba8();
                let (w, h) = (rgba.width(), rgba.height());

                let mut jpeg_buf = std::io::Cursor::new(Vec::new());
                if image::DynamicImage::ImageRgba8(rgba.clone())
                    .write_to(&mut jpeg_buf, image::ImageFormat::Jpeg)
                    .is_err()
                {
                    log::warn!("Failed to encode avatar as JPEG");
                    return;
                }
                let jpeg_bytes = jpeg_buf.into_inner();
                log::info!(
                    "[avatar] picked image {}x{} -> JPEG {} bytes -> base64 {} chars",
                    w,
                    h,
                    jpeg_bytes.len(),
                    base64::engine::general_purpose::STANDARD
                        .encode(&jpeg_bytes)
                        .len()
                );
                use base64::Engine as _;
                let b64 = base64::engine::general_purpose::STANDARD.encode(jpeg_bytes);
                *avatar_b64.lock().unwrap() = Some(b64);

                let rgba_bytes = rgba.into_raw();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(app) = app_weak.upgrade() else {
                        return;
                    };
                    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        &rgba_bytes,
                        w,
                        h,
                    );
                    app.set_new_crew_avatar_preview(slint::Image::from_rgba8(buf));
                    app.set_new_crew_has_avatar(true);
                });
            });
        });
    }

    // --- Create crew ---
    {
        let cmd = ctx.cmd_tx.clone();
        let avatar_b64 = ctx.new_crew_avatar_b64.clone();
        let invited = ctx.invited_users.clone();
        let app_weak = ctx.app.as_weak();
        let s = ctx.settings.clone();
        let avatar_st = ctx.avatar_state.clone();
        let shuffle_timer = ctx.avatar_shuffle_timer.clone();
        let rt_handle = ctx.rt.clone();
        ctx.app
            .on_create_crew(move |name, description, is_private| {
                let Some(app) = app_weak.upgrade() else {
                    return;
                };

                if app.get_onboarding_step() < 4 {
                    let mut settings = s.borrow_mut();
                    settings.pending_crew_id = None;
                    settings.pending_crew_name = Some(name.to_string());
                    settings.pending_crew_description = Some(description.to_string());
                    settings.pending_crew_open = Some(!is_private);
                    settings.onboarding_step = 2;
                    settings.save();
                    drop(settings);
                    app.set_new_crew_open(false);
                    app.set_onboarding_step(2);
                    log::info!(
                        "[onboarding] crew created locally: name={:?} open={} — loading avatars",
                        name.as_str(),
                        !is_private,
                    );

                    *avatar_st.lock().unwrap() = avatar::AvatarGridState::new();
                    load_avatar_grid(app.as_weak(), &avatar_st, &rt_handle);
                    let shuffle_timer2 = shuffle_timer.clone();
                    let avatar_st2 = avatar_st.clone();
                    let app_weak2 = app.as_weak();
                    let rt_h = rt_handle.clone();
                    let delay_timer = slint::Timer::default();
                    delay_timer.start(
                        slint::TimerMode::SingleShot,
                        std::time::Duration::from_millis(7 * 60 + 500),
                        move || {
                            start_ambient_shuffle(
                                app_weak2.clone(),
                                &avatar_st2,
                                &shuffle_timer2,
                                &rt_h,
                            );
                        },
                    );
                    std::mem::forget(delay_timer);
                    return;
                }

                let avatar = avatar_b64.lock().unwrap().take();
                let invite_user_ids: Vec<String> = invited
                    .borrow()
                    .iter()
                    .map(|(id, _, _)| id.clone())
                    .collect();
                log::info!(
                    "[ui] create crew name={:?} has_avatar={} invites={}",
                    name.as_str(),
                    avatar.is_some(),
                    invite_user_ids.len()
                );
                let _ = cmd.try_send(Command::CreateCrew {
                    name: name.to_string(),
                    description: description.to_string(),
                    open: !is_private,
                    avatar,
                    invite_user_ids,
                });
            });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_new_crew_search_users(move |query| {
            if query.len() >= 2 {
                let _ = cmd.try_send(Command::SearchUsers {
                    query: query.to_string(),
                });
            }
        });
    }
    {
        let app_weak = ctx.app.as_weak();
        let invited = ctx.invited_users.clone();
        ctx.app.on_new_crew_invite_user(move |user_id| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let results_model = app.get_new_crew_search_results();
            let mut found = None;
            for i in 0..results_model.row_count() {
                if let Some(r) = results_model.row_data(i) {
                    if r.id == user_id {
                        found = Some((r.id.to_string(), r.display_name.to_string(), r.is_friend));
                        break;
                    }
                }
            }
            if let Some(entry) = found {
                let mut list = invited.borrow_mut();
                if !list.iter().any(|(id, _, _)| *id == entry.0) {
                    list.push(entry);
                }
                let model: Vec<SearchUserData> = list
                    .iter()
                    .map(|(id, name, is_friend)| SearchUserData {
                        id: id.into(),
                        display_name: name.into(),
                        is_friend: *is_friend,
                    })
                    .collect();
                let rc_model = Rc::new(slint::VecModel::from(model));
                app.set_new_crew_invited_users(slint::ModelRc::from(rc_model));
            }
        });
    }
    {
        let app_weak = ctx.app.as_weak();
        let invited = ctx.invited_users.clone();
        ctx.app.on_new_crew_uninvite_user(move |user_id| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let mut list = invited.borrow_mut();
            list.retain(|(id, _, _)| *id != user_id.as_str());
            let model: Vec<SearchUserData> = list
                .iter()
                .map(|(id, name, is_friend)| SearchUserData {
                    id: id.into(),
                    display_name: name.into(),
                    is_friend: *is_friend,
                })
                .collect();
            let rc_model = Rc::new(slint::VecModel::from(model));
            app.set_new_crew_invited_users(slint::ModelRc::from(rc_model));
        });
    }
    {
        let app_weak = ctx.app.as_weak();
        ctx.app.on_new_crew_copy_invite_code(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let code = app.get_new_crew_invite_code().to_string();
            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                let _ = clipboard.set_text(&code);
                log::info!("Invite code copied to clipboard: {}", code);
            }
        });
    }

    // --- Discover crews ---
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_discover_requested(move || {
            let _ = cmd.try_send(Command::DiscoverCrews { cursor: None });
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_discover_join_crew(move |crew_id| {
            let _ = cmd.try_send(Command::JoinCrew {
                crew_id: crew_id.to_string(),
            });
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_discover_join_invite(move |code| {
            log::info!("[discover] join-by-invite code={}", code);
            let _ = cmd.try_send(Command::JoinByInviteCode {
                code: code.to_string(),
            });
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let cursor_ref = ctx.discover_cursor.clone();
        let loading_ref = ctx.discover_loading.clone();
        ctx.app.on_discover_load_more(move || {
            let mut loading = loading_ref.borrow_mut();
            if *loading {
                return;
            }
            let cursor = cursor_ref.borrow().clone();
            if cursor.is_none() {
                return;
            }
            *loading = true;
            log::info!("[discover] load-more triggered");
            let _ = cmd.try_send(Command::DiscoverCrews { cursor });
        });
    }
}
