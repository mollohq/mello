use mello_core::Command;
use slint::ComponentHandle;

use crate::app_context::AppContext;

pub fn wire(ctx: &AppContext) {
    // --- Save crew (name + description + optional avatar) ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        let avatar_b64 = ctx.crew_settings_avatar_b64.clone();
        ctx.app.on_crew_settings_save(move |name, description| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let crew_id = app.get_active_crew_id().to_string();
            if crew_id.is_empty() {
                return;
            }
            let avatar = avatar_b64.lock().unwrap().take();
            let open = app.get_crew_settings_is_open();
            let invite_policy = app.get_crew_settings_invite_policy().to_string();
            let _ = cmd.try_send(Command::UpdateCrew {
                crew_id,
                name: if name.is_empty() {
                    None
                } else {
                    Some(name.to_string())
                },
                description: Some(description.to_string()),
                avatar,
                open: Some(open),
                invite_policy: Some(invite_policy),
            });
        });
    }

    // --- Avatar upload ---
    {
        let app_weak = ctx.app.as_weak();
        let avatar_b64 = ctx.crew_settings_avatar_b64.clone();
        let rt_handle = ctx.rt.clone();
        ctx.app.on_crew_settings_avatar_upload(move || {
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
                    log::warn!("crew avatar file too large: {} bytes", bytes.len());
                    return;
                }
                let Ok(img) = image::load_from_memory(&bytes) else {
                    log::warn!("Failed to decode crew avatar image");
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
                    log::warn!("Failed to encode crew avatar as JPEG");
                    return;
                }
                let jpeg_bytes = jpeg_buf.into_inner();
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
                    app.set_crew_settings_avatar(slint::Image::from_rgba8(buf));
                    app.set_crew_settings_has_avatar(true);
                });
            });
        });
    }

    // --- Add voice channel ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_crew_settings_add_channel(move |name| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let crew_id = app.get_active_crew_id().to_string();
            if crew_id.is_empty() || name.is_empty() {
                return;
            }
            let _ = cmd.try_send(Command::CreateVoiceChannel {
                crew_id,
                name: name.to_string(),
            });
        });
    }

    // --- Rename voice channel ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app
            .on_crew_settings_rename_channel(move |channel_id, new_name| {
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                let crew_id = app.get_active_crew_id().to_string();
                if crew_id.is_empty() {
                    return;
                }
                let _ = cmd.try_send(Command::RenameVoiceChannel {
                    crew_id,
                    channel_id: channel_id.to_string(),
                    name: new_name.to_string(),
                });
            });
    }

    // --- Delete voice channel ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_crew_settings_delete_channel(move |channel_id| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let crew_id = app.get_active_crew_id().to_string();
            if crew_id.is_empty() {
                return;
            }
            let _ = cmd.try_send(Command::DeleteVoiceChannel {
                crew_id,
                channel_id: channel_id.to_string(),
            });
        });
    }

    // --- Delete crew ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_crew_settings_delete_crew(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let crew_id = app.get_active_crew_id().to_string();
            if crew_id.is_empty() {
                return;
            }
            let _ = cmd.try_send(Command::DeleteCrew { crew_id });
        });
    }

    // --- Promote member (member -> admin) ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_crew_settings_promote_member(move |user_id| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let crew_id = app.get_active_crew_id().to_string();
            if crew_id.is_empty() {
                return;
            }
            let _ = cmd.try_send(Command::ChangeCrewRole {
                crew_id,
                user_id: user_id.to_string(),
                new_role: 1,
            });
        });
    }

    // --- Demote member (admin -> member) ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_crew_settings_demote_member(move |user_id| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let crew_id = app.get_active_crew_id().to_string();
            if crew_id.is_empty() {
                return;
            }
            let _ = cmd.try_send(Command::ChangeCrewRole {
                crew_id,
                user_id: user_id.to_string(),
                new_role: 2,
            });
        });
    }

    // --- Kick member ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_crew_settings_kick_member(move |user_id| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let crew_id = app.get_active_crew_id().to_string();
            if crew_id.is_empty() {
                return;
            }
            let _ = cmd.try_send(Command::KickCrewMember {
                crew_id,
                user_id: user_id.to_string(),
            });
        });
    }

    // --- Context menu: add channel (quick action) ---
    {
        let app_weak = ctx.app.as_weak();
        ctx.app.on_crew_context_add_channel(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_crew_settings_active_tab(1);
            app.set_crew_settings_open(true);
        });
    }

    // --- Context menu: leave crew ---
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_crew_context_leave(move || {
            let _ = cmd.try_send(Command::LeaveCrew);
        });
    }
}
