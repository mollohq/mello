use mello_core::Command;
use slint::ComponentHandle;

use crate::app_context::AppContext;
use crate::avatar;
use crate::platform;
use crate::Settings;

pub fn wire(ctx: &AppContext) {
    // --- Settings modal open ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        let s = ctx.settings.clone();
        let prof_st = ctx.profile_avatar_state.clone();
        ctx.app.on_settings_requested(move || {
            let _ = cmd.try_send(Command::ListAudioDevices);
            if let Some(app) = app_weak.upgrade() {
                let settings = s.borrow();
                app.set_settings_start_on_boot(settings.start_on_boot);
                app.set_settings_start_minimized(settings.start_minimized);
                app.set_settings_close_to_tray(settings.close_to_tray);
                app.set_settings_auto_connect(settings.auto_connect);
                app.set_settings_minimize_on_join(settings.minimize_on_join);
                app.set_settings_hw_acceleration(settings.hardware_acceleration);
                app.set_settings_input_volume(settings.input_volume);
                app.set_settings_output_volume(settings.output_volume);
                app.set_settings_noise_suppression(settings.noise_suppression);
                app.set_settings_echo_cancellation(settings.echo_cancellation);
                app.set_settings_ptt_mode(settings.input_mode == "push_to_talk");
                app.set_settings_vad_threshold(settings.vad_threshold);
                app.set_settings_hud_enabled(settings.hud_enabled);
                app.set_settings_hud_overlay_in_game(settings.hud_show_overlay_in_game);
                app.set_settings_hud_overlay_opacity(settings.hud_overlay_opacity);
                app.set_settings_hud_clip_toasts(settings.hud_show_clip_toasts);
                let ptt_label: slint::SharedString = if let Some(ref key_str) = settings.ptt_key {
                    platform::hotkeys::parse_ptt_string(key_str)
                        .map(|(_, label)| label)
                        .unwrap_or_else(|| "Unassigned".into())
                } else {
                    "Unassigned".into()
                }
                .into();
                app.set_settings_ptt_key_label(ptt_label);
                *prof_st.lock().unwrap() = avatar::AvatarGridState::new();
                app.set_profile_selected_avatar(-1);
                app.set_profile_grid_expanded(false);
                app.set_profile_nickname_value(app.get_user_name());
                app.set_settings_open(true);
            }
        });
    }

    // --- Device selection ---
    {
        let cmd = ctx.cmd_tx.clone();
        let s = ctx.settings.clone();
        ctx.app.on_capture_device_selected(move |id| {
            let id_str = id.to_string();
            let _ = cmd.try_send(Command::SetCaptureDevice { id: id_str.clone() });
            let mut settings = s.borrow_mut();
            settings.capture_device_id = Some(id_str);
            settings.save();
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let s = ctx.settings.clone();
        ctx.app.on_playback_device_selected(move |id| {
            let id_str = id.to_string();
            let _ = cmd.try_send(Command::SetPlaybackDevice { id: id_str.clone() });
            let mut settings = s.borrow_mut();
            settings.playback_device_id = Some(id_str);
            settings.save();
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_mic_test_toggled(move || {
            if let Some(app) = app_weak.upgrade() {
                let enabled = app.get_mic_testing();
                let _ = cmd.try_send(Command::SetLoopback { enabled });
            }
        });
    }

    // --- Settings toggles ---
    {
        let s = ctx.settings.clone();
        ctx.app.on_setting_changed_start_on_boot(move |v| {
            let mut settings = s.borrow_mut();
            settings.start_on_boot = v;
            settings.save();
            if let Err(e) = crate::autolaunch::set_start_on_boot(v) {
                log::warn!("Failed to set start on boot: {}", e);
            }
        });
    }
    {
        let s = ctx.settings.clone();
        ctx.app.on_setting_changed_start_minimized(move |v| {
            let mut settings = s.borrow_mut();
            settings.start_minimized = v;
            settings.save();
        });
    }
    {
        let s = ctx.settings.clone();
        ctx.app.on_setting_changed_close_to_tray(move |v| {
            let mut settings = s.borrow_mut();
            settings.close_to_tray = v;
            settings.save();
        });
    }
    {
        let s = ctx.settings.clone();
        ctx.app.on_setting_changed_auto_connect(move |v| {
            let mut settings = s.borrow_mut();
            settings.auto_connect = v;
            settings.save();
        });
    }
    {
        let s = ctx.settings.clone();
        ctx.app.on_setting_changed_minimize_on_join(move |v| {
            let mut settings = s.borrow_mut();
            settings.minimize_on_join = v;
            settings.save();
        });
    }
    {
        let s = ctx.settings.clone();
        ctx.app.on_setting_changed_hw_acceleration(move |v| {
            let mut settings = s.borrow_mut();
            settings.hardware_acceleration = v;
            settings.save();
        });
    }
    // HUD settings
    {
        let s = ctx.settings.clone();
        let hud = ctx.hud_manager.clone();
        let fg = ctx.fg_monitor.clone();
        ctx.app.on_setting_changed_hud_enabled(move |v| {
            let mut settings = s.borrow_mut();
            settings.hud_enabled = v;
            settings.save();
            fg.borrow_mut().set_hud_enabled(v);
            if !v {
                hud.shutdown();
            }
            log::info!("[settings] hud_enabled = {}", v);
        });
    }
    {
        let s = ctx.settings.clone();
        let hud = ctx.hud_manager.clone();
        let fg = ctx.fg_monitor.clone();
        ctx.app.on_setting_changed_hud_overlay_in_game(move |v| {
            let mut settings = s.borrow_mut();
            settings.hud_show_overlay_in_game = v;
            settings.save();
            fg.borrow_mut().set_overlay_enabled(v);
            hud.push_settings(crate::hud_manager::HudSettings {
                overlay_opacity: settings.hud_overlay_opacity,
                show_clip_toasts: settings.hud_show_clip_toasts,
                overlay_enabled: v,
            });
        });
    }
    {
        let s = ctx.settings.clone();
        let hud = ctx.hud_manager.clone();
        ctx.app.on_setting_changed_hud_overlay_opacity(move |v| {
            let mut settings = s.borrow_mut();
            settings.hud_overlay_opacity = v;
            settings.save();
            hud.push_settings(crate::hud_manager::HudSettings {
                overlay_opacity: v,
                show_clip_toasts: settings.hud_show_clip_toasts,
                overlay_enabled: settings.hud_show_overlay_in_game,
            });
        });
    }
    {
        let s = ctx.settings.clone();
        let hud = ctx.hud_manager.clone();
        ctx.app.on_setting_changed_hud_clip_toasts(move |v| {
            let mut settings = s.borrow_mut();
            settings.hud_show_clip_toasts = v;
            settings.save();
            hud.push_settings(crate::hud_manager::HudSettings {
                overlay_opacity: settings.hud_overlay_opacity,
                show_clip_toasts: v,
                overlay_enabled: settings.hud_show_overlay_in_game,
            });
        });
    }
    {
        let s = ctx.settings.clone();
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_setting_changed_input_volume(move |v| {
            let mut settings = s.borrow_mut();
            settings.input_volume = v;
            settings.save();
            let _ = cmd.try_send(Command::SetInputVolume { volume: v });
        });
    }
    {
        let s = ctx.settings.clone();
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_setting_changed_output_volume(move |v| {
            let mut settings = s.borrow_mut();
            settings.output_volume = v;
            settings.save();
            let _ = cmd.try_send(Command::SetOutputVolume { volume: v });
        });
    }
    {
        let s = ctx.settings.clone();
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_setting_changed_noise_suppression(move |v| {
            let mut settings = s.borrow_mut();
            settings.noise_suppression = v;
            settings.save();
            let _ = cmd.try_send(Command::SetNoiseSuppression { enabled: v });
        });
    }
    {
        let s = ctx.settings.clone();
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_setting_changed_echo_cancellation(move |v| {
            let mut settings = s.borrow_mut();
            settings.echo_cancellation = v;
            settings.save();
            let _ = cmd.try_send(Command::SetEchoCancellation { enabled: v });
        });
    }
    {
        let s = ctx.settings.clone();
        let hk = ctx.hotkey_mgr.clone();
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_setting_changed_input_mode(move |is_ptt| {
            let mut settings = s.borrow_mut();
            settings.input_mode = if is_ptt {
                "push_to_talk".into()
            } else {
                "voice_activity".into()
            };
            settings.save();
            hk.borrow().set_active(is_ptt);
            // Mute when entering PTT (unmute on key press), unmute when leaving PTT
            let _ = cmd.try_send(Command::SetMute { muted: is_ptt });
        });
    }
    {
        let s = ctx.settings.clone();
        ctx.app.on_setting_changed_vad_threshold(move |v| {
            let mut settings = s.borrow_mut();
            settings.vad_threshold = v;
            settings.save();
        });
    }
    {
        let s = ctx.settings.clone();
        let app_weak = ctx.app.as_weak();
        let hk = ctx.hotkey_mgr.clone();
        ctx.app
            .on_settings_ptt_key_captured(move |key_text, ctrl, alt, shift, meta| {
                if let Some((binding, label, raw_str)) =
                    platform::hotkeys::slint_key_to_ptt(key_text.as_str(), ctrl, alt, shift, meta)
                {
                    log::info!("PTT key registered: {} ({})", label, raw_str);
                    hk.borrow().register_ptt(binding);
                    let mut settings = s.borrow_mut();
                    settings.ptt_key = Some(raw_str);
                    settings.save();
                    if let Some(app) = app_weak.upgrade() {
                        app.set_settings_ptt_key_label(label.into());
                        app.set_settings_ptt_binding(false);
                    }
                } else {
                    log::debug!(
                        "Unmappable key, staying in binding mode: {:?}",
                        key_text.as_str()
                    );
                }
            });
    }
    {
        let s = ctx.settings.clone();
        let app_weak = ctx.app.as_weak();
        let hk = ctx.hotkey_mgr.clone();
        ctx.app.on_settings_reset_defaults(move || {
            let defaults = Settings::default();
            *s.borrow_mut() = defaults.clone();
            s.borrow().save();
            hk.borrow().unregister_ptt();
            if let Some(app) = app_weak.upgrade() {
                app.set_settings_start_on_boot(defaults.start_on_boot);
                app.set_settings_start_minimized(defaults.start_minimized);
                app.set_settings_close_to_tray(defaults.close_to_tray);
                app.set_settings_auto_connect(defaults.auto_connect);
                app.set_settings_minimize_on_join(defaults.minimize_on_join);
                app.set_settings_hw_acceleration(defaults.hardware_acceleration);
                app.set_settings_input_volume(defaults.input_volume);
                app.set_settings_output_volume(defaults.output_volume);
                app.set_settings_noise_suppression(defaults.noise_suppression);
                app.set_settings_echo_cancellation(defaults.echo_cancellation);
                app.set_settings_ptt_mode(defaults.input_mode == "push_to_talk");
                app.set_settings_vad_threshold(defaults.vad_threshold);
                app.set_settings_ptt_key_label("Unassigned".into());
                app.set_settings_hud_enabled(defaults.hud_enabled);
                app.set_settings_hud_overlay_in_game(defaults.hud_show_overlay_in_game);
                app.set_settings_hud_overlay_opacity(defaults.hud_overlay_opacity);
                app.set_settings_hud_clip_toasts(defaults.hud_show_clip_toasts);
            }
            if let Err(e) = crate::autolaunch::set_start_on_boot(false) {
                log::warn!("Failed to reset auto-launch: {}", e);
            }
        });
    }

    // --- Theme toggle ---
    {
        let app_weak = ctx.app.as_weak();
        let s = ctx.settings.clone();
        ctx.app.on_theme_toggled(move || {
            if let Some(app) = app_weak.upgrade() {
                let new_dark = !app.global::<crate::Theme>().get_dark();
                app.global::<crate::Theme>().set_dark(new_dark);
                let mut settings = s.borrow_mut();
                settings.dark_theme = new_dark;
                settings.save();
            }
        });
    }

    // --- Debug toggle ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_debug_toggled(move || {
            if let Some(app) = app_weak.upgrade() {
                let enabled = app.get_debug_open();
                let _ = cmd.try_send(Command::SetDebugMode { enabled });
            }
        });
    }

    // --- Profile settings callbacks ---
    {
        let prof_st = ctx.profile_avatar_state.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_profile_avatar_clicked(move |idx| {
            let idx_usize = idx as usize;
            let already_selected = prof_st.lock().unwrap().selected_slot == Some(idx_usize);

            if already_selected {
                prof_st.lock().unwrap().selected_slot = None;
                if let Some(app) = app_weak.upgrade() {
                    app.set_profile_selected_avatar(-1);
                }
            } else {
                prof_st.lock().unwrap().selected_slot = Some(idx_usize);
                if let Some(app) = app_weak.upgrade() {
                    app.set_profile_selected_avatar(idx);
                }
            }
        });
    }
    {
        let prof_st = ctx.profile_avatar_state.clone();
        let app_weak = ctx.app.as_weak();
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_profile_use_avatar(move |slot| {
            let st = prof_st.lock().unwrap();
            let (avatar_data, avatar_format, avatar_style, avatar_seed) =
                collect_avatar_data(&st, slot);
            drop(st);

            if let Some(app) = app_weak.upgrade() {
                let nickname = app.get_profile_nickname_value().to_string();
                let _ = cmd.try_send(Command::UpdateProfile {
                    display_name: nickname,
                    avatar_data,
                    avatar_format,
                    avatar_style,
                    avatar_seed,
                });
            }
        });
    }
    {
        let prof_st = ctx.profile_avatar_state.clone();
        let app_weak = ctx.app.as_weak();
        let rt_h = ctx.rt.clone();
        ctx.app.on_profile_reroll(move || {
            {
                let mut st = prof_st.lock().unwrap();
                st.roll_counter += 1;
                st.selected_slot = None;
                for i in 0..7 {
                    st.slots[i].svg_data = None;
                }
            }
            if let Some(app) = app_weak.upgrade() {
                app.set_profile_selected_avatar(-1);
                for i in 0..7 {
                    match i {
                        0 => app.set_profile_avatar_loaded_0(false),
                        1 => app.set_profile_avatar_loaded_1(false),
                        2 => app.set_profile_avatar_loaded_2(false),
                        3 => app.set_profile_avatar_loaded_3(false),
                        4 => app.set_profile_avatar_loaded_4(false),
                        5 => app.set_profile_avatar_loaded_5(false),
                        6 => app.set_profile_avatar_loaded_6(false),
                        _ => {}
                    }
                }
                load_profile_avatar_grid(app.as_weak(), &prof_st, &rt_h);
            }
        });
    }
    {
        let prof_st = ctx.profile_avatar_state.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_profile_upload_clicked(move || {
            let dialog = rfd::FileDialog::new()
                .add_filter("Images", &["png", "jpg", "jpeg", "webp"])
                .pick_file();
            if let Some(path) = dialog {
                match image::open(&path) {
                    Ok(img) => {
                        let resized =
                            img.resize_to_fill(256, 256, image::imageops::FilterType::CatmullRom);
                        let rgba = resized.to_rgba8();
                        let (w, h) = (rgba.width(), rgba.height());
                        let raw = rgba.into_raw();
                        {
                            let mut st = prof_st.lock().unwrap();
                            st.upload_data = Some(raw.clone());
                            st.selected_slot = Some(7);
                        }
                        let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                            &raw, w, h,
                        );
                        let slint_img = slint::Image::from_rgba8(buf);
                        if let Some(app) = app_weak.upgrade() {
                            app.set_profile_upload_preview(slint_img);
                            app.set_profile_has_upload(true);
                            app.set_profile_selected_avatar(7);
                        }
                    }
                    Err(e) => {
                        log::error!("[profile] failed to open image: {}", e);
                    }
                }
            }
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_profile_nickname_accepted(move |name| {
            let nickname = name.to_string();
            if nickname.is_empty() {
                return;
            }
            let _ = cmd.try_send(Command::UpdateProfile {
                display_name: nickname,
                avatar_data: None,
                avatar_format: None,
                avatar_style: None,
                avatar_seed: None,
            });
            if let Some(app) = app_weak.upgrade() {
                app.set_settings_show_saved(true);
            }
        });
    }
    {
        let prof_st = ctx.profile_avatar_state.clone();
        let app_weak = ctx.app.as_weak();
        let rt_h = ctx.rt.clone();
        ctx.app.on_profile_expand_grid(move || {
            let st = prof_st.lock().unwrap();
            let already_loaded = st.slots[0].svg_data.is_some();
            drop(st);
            if !already_loaded {
                if let Some(app) = app_weak.upgrade() {
                    load_profile_avatar_grid(app.as_weak(), &prof_st, &rt_h);
                }
            }
        });
    }
}

fn load_profile_avatar_grid(
    app_weak: slint::Weak<crate::MainWindow>,
    state: &std::sync::Arc<std::sync::Mutex<avatar::AvatarGridState>>,
    rt: &tokio::runtime::Handle,
) {
    log::info!("[profile] loading avatar grid for settings");
    let http = reqwest::Client::new();
    for i in 0..7usize {
        let style = avatar::AvatarGridState::pick_random_style().to_string();
        let seed = state.lock().unwrap().make_seed(i);
        {
            let mut s = state.lock().unwrap();
            s.slots[i].style = style.clone();
            s.slots[i].seed = seed.clone();
        }
        let app_weak = app_weak.clone();
        let http = http.clone();
        let state_clone = state.clone();
        rt.spawn(async move {
            match avatar::fetch_and_rasterize(&http, &style, &seed).await {
                Some((svg_data, rgba)) => {
                    state_clone.lock().unwrap().slots[i].svg_data = Some(svg_data);
                    let _ = slint::invoke_from_event_loop(move || {
                        let image =
                            avatar::rgba_to_image(&rgba, avatar::RENDER_SIZE, avatar::RENDER_SIZE);
                        let Some(app) = app_weak.upgrade() else {
                            return;
                        };
                        match i {
                            0 => {
                                app.set_profile_avatar_0(image.clone());
                                app.set_profile_avatar_loaded_0(true);
                            }
                            1 => {
                                app.set_profile_avatar_1(image.clone());
                                app.set_profile_avatar_loaded_1(true);
                            }
                            2 => {
                                app.set_profile_avatar_2(image.clone());
                                app.set_profile_avatar_loaded_2(true);
                            }
                            3 => {
                                app.set_profile_avatar_3(image.clone());
                                app.set_profile_avatar_loaded_3(true);
                            }
                            4 => {
                                app.set_profile_avatar_4(image.clone());
                                app.set_profile_avatar_loaded_4(true);
                            }
                            5 => {
                                app.set_profile_avatar_5(image.clone());
                                app.set_profile_avatar_loaded_5(true);
                            }
                            6 => {
                                app.set_profile_avatar_6(image.clone());
                                app.set_profile_avatar_loaded_6(true);
                            }
                            _ => {}
                        }
                    });
                }
                None => {
                    log::warn!("[profile] avatar slot {} fetch failed", i);
                }
            }
        });
    }
}

fn collect_avatar_data(
    st: &avatar::AvatarGridState,
    slot: i32,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    if (0..=6).contains(&slot) {
        let idx = slot as usize;
        if let Some(ref svg) = st.slots[idx].svg_data {
            (
                Some(svg.clone()),
                Some("svg".to_string()),
                Some(st.slots[idx].style.clone()),
                Some(st.slots[idx].seed.clone()),
            )
        } else {
            (None, None, None, None)
        }
    } else if slot == 7 {
        if let Some(ref raw) = st.upload_data {
            use base64::Engine;
            let mut png_buf = Vec::new();
            let encoder = image::codecs::png::PngEncoder::new(&mut png_buf);
            image::ImageEncoder::write_image(
                encoder,
                raw,
                256,
                256,
                image::ExtendedColorType::Rgba8,
            )
            .ok();
            let b64 = base64::engine::general_purpose::STANDARD.encode(&png_buf);
            (Some(b64), Some("png".to_string()), None, None)
        } else {
            (None, None, None, None)
        }
    } else {
        (None, None, None, None)
    }
}
