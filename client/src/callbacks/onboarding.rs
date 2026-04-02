use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::ComponentHandle;

use base64::Engine as _;
use mello_core::Command;

use crate::app_context::AppContext;
use crate::{avatar, MainWindow};

pub fn wire(ctx: &AppContext) {
    // --- Onboarding: crew selected ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        let s = ctx.settings.clone();
        let avatar_st = ctx.avatar_state.clone();
        let shuffle_timer = ctx.avatar_shuffle_timer.clone();
        let rt_handle = ctx.rt.clone();
        ctx.app.on_onboarding_crew_selected(move |crew_id| {
            let _ = cmd.try_send(Command::ListAudioDevices);
            if let Some(app) = app_weak.upgrade() {
                app.set_onboarding_step(2);
                let mut settings = s.borrow_mut();
                settings.pending_crew_id = Some(crew_id.to_string());
                settings.pending_crew_name = None;
                settings.onboarding_step = 2;
                settings.save();
                drop(settings);
                log::info!(
                    "[onboarding] crew selected (stored locally): {} — loading avatars",
                    crew_id
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
                    Duration::from_millis(7 * 60 + 500),
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
            }
        });
    }

    // --- Onboarding: create crew ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_onboarding_create_crew(move |_name| {
            let _ = cmd.try_send(Command::ListAudioDevices);
            if let Some(app) = app_weak.upgrade() {
                app.set_new_crew_has_avatar(false);
                app.set_new_crew_created(false);
                app.set_new_crew_open(true);
                log::info!("[onboarding] opening new-crew modal");
            }
        });
    }

    // --- Onboarding: continue to step ---
    {
        let app_weak = ctx.app.as_weak();
        let s = ctx.settings.clone();
        let cmd = ctx.cmd_tx.clone();
        let avatar_b64 = ctx.new_crew_avatar_b64.clone();
        let avatar_st = ctx.avatar_state.clone();
        let shuffle_timer = ctx.avatar_shuffle_timer.clone();
        let rt_handle = ctx.rt.clone();
        ctx.app.on_onboarding_continue(move |step| {
            if let Some(app) = app_weak.upgrade() {
                if step == 2 {
                    log::info!("[avatar] entering step 2 — resetting grid and loading avatars");
                    app.set_onboarding_step(step);
                    let mut settings = s.borrow_mut();
                    settings.onboarding_step = step as u8;
                    settings.save();
                    drop(settings);
                    *avatar_st.lock().unwrap() = avatar::AvatarGridState::new();
                    load_avatar_grid(app.as_weak(), &avatar_st, &rt_handle);
                    let shuffle_timer2 = shuffle_timer.clone();
                    let avatar_st2 = avatar_st.clone();
                    let app_weak2 = app.as_weak();
                    let rt_h = rt_handle.clone();
                    let delay_timer = slint::Timer::default();
                    delay_timer.start(
                        slint::TimerMode::SingleShot,
                        Duration::from_millis(7 * 60 + 500),
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
                if step == 3 {
                    stop_ambient_shuffle(&shuffle_timer);

                    {
                        let state = avatar_st.lock().unwrap();
                        if let Some(sel) = state.selected_slot {
                            let img = if sel == 7 {
                                state.upload_data.as_ref().and_then(|png| {
                                    image::load_from_memory(png).ok().map(|dyn_img| {
                                        let rgba = dyn_img.to_rgba8();
                                        let (w, h) = rgba.dimensions();
                                        avatar::rgba_to_image(rgba.as_raw(), w, h)
                                    })
                                })
                            } else if sel < 7 {
                                state.slots[sel].svg_data.as_ref().and_then(|svg| {
                                    avatar::rasterize_svg(svg).map(|rgba| {
                                        avatar::rgba_to_image(
                                            &rgba,
                                            avatar::RENDER_SIZE,
                                            avatar::RENDER_SIZE,
                                        )
                                    })
                                })
                            } else {
                                None
                            };
                            if let Some(img) = img {
                                app.set_user_avatar(img);
                                app.set_has_user_avatar(true);
                            }
                        }
                    }

                    let nickname = app.get_onboarding_nickname().to_string();
                    let settings = s.borrow();
                    let crew_id = settings.pending_crew_id.clone();
                    let crew_name = settings.pending_crew_name.clone();
                    let crew_description = settings.pending_crew_description.clone();
                    let crew_open = settings.pending_crew_open;
                    drop(settings);
                    let crew_avatar = avatar_b64.lock().unwrap().take();

                    let (avatar_data, avatar_format, avatar_style, avatar_seed) = {
                        let state = avatar_st.lock().unwrap();
                        if let Some(sel) = state.selected_slot {
                            if sel == 7 {
                                if let Some(ref png_bytes) = state.upload_data {
                                    let b64 = base64::engine::general_purpose::STANDARD
                                        .encode(png_bytes);
                                    (Some(b64), Some("png".to_string()), None, None)
                                } else {
                                    (None, None, None, None)
                                }
                            } else if sel < 7 {
                                let slot = &state.slots[sel];
                                (
                                    slot.svg_data.clone(),
                                    Some("svg".to_string()),
                                    Some(slot.style.clone()),
                                    Some(slot.seed.clone()),
                                )
                            } else {
                                (None, None, None, None)
                            }
                        } else {
                            (None, None, None, None)
                        }
                    };

                    log::info!(
                        "[onboarding] finalizing — nickname={} crew_id={:?} crew_name={:?} has_avatar={}",
                        nickname,
                        crew_id,
                        crew_name,
                        avatar_data.is_some(),
                    );
                    let _ = cmd.try_send(Command::FinalizeOnboarding {
                        crew_id,
                        crew_name,
                        crew_description,
                        crew_open,
                        crew_avatar,
                        display_name: nickname,
                        avatar_data,
                        avatar_format,
                        avatar_style,
                        avatar_seed,
                    });
                    return;
                }
                app.set_onboarding_step(step);
                let mut settings = s.borrow_mut();
                settings.onboarding_step = step as u8;
                settings.save();
            }
        });
    }

    // --- Onboarding: login requested (pill click) ---
    {
        let app_weak = ctx.app.as_weak();
        let s = ctx.settings.clone();
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_onboarding_login_requested(move || {
            if let Some(app) = app_weak.upgrade() {
                log::info!("[auth] sign-in pill — entering app as device user");
                app.set_logged_in(true);
                app.set_onboarding_step(4);
                let mut settings = s.borrow_mut();
                settings.onboarding_step = 4;
                settings.save();
                let _ = cmd.try_send(Command::LoadMyCrews);
            }
        });
    }

    // --- Onboarding: social auth ---
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_onboarding_auth_steam(move || {
            let _ = cmd.try_send(Command::AuthSteam);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_onboarding_auth_google(move || {
            let _ = cmd.try_send(Command::LinkGoogle);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_onboarding_auth_twitch(move || {
            let _ = cmd.try_send(Command::AuthTwitch);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_onboarding_auth_discord(move || {
            let _ = cmd.try_send(Command::LinkDiscord);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_onboarding_auth_apple(move || {
            let _ = cmd.try_send(Command::AuthApple);
        });
    }

    // --- Onboarding: link email ---
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_onboarding_link_email(move |email, password| {
            let _ = cmd.try_send(Command::LinkEmail {
                email: email.to_string(),
                password: password.to_string(),
            });
        });
    }

    // --- Onboarding: skip identity ---
    {
        let app_weak = ctx.app.as_weak();
        let s = ctx.settings.clone();
        ctx.app.on_onboarding_skip_identity(move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_onboarding_step(4);
                app.set_logged_in(true);
                let mut settings = s.borrow_mut();
                settings.onboarding_step = 4;
                settings.save();
            }
        });
    }

    // --- Onboarding: device selection ---
    {
        let cmd = ctx.cmd_tx.clone();
        let s = ctx.settings.clone();
        ctx.app.on_onboarding_capture_device_selected(move |id| {
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
        ctx.app.on_onboarding_playback_device_selected(move |id| {
            let id_str = id.to_string();
            let _ = cmd.try_send(Command::SetPlaybackDevice { id: id_str.clone() });
            let mut settings = s.borrow_mut();
            settings.playback_device_id = Some(id_str);
            settings.save();
        });
    }

    // --- Presence ---
    ctx.app.on_presence_changed(move |status| {
        log::info!("Presence changed to {}", status);
    });

    // --- Update toast callbacks ---
    {
        let u = ctx.updater.clone();
        ctx.app.on_update_now_clicked(move || {
            if let Some(ref mut updater) = *u.borrow_mut() {
                if let Err(e) = updater.update_and_restart() {
                    log::warn!("Failed to update: {}", e);
                }
            }
        });
    }
    {
        let app_weak = ctx.app.as_weak();
        ctx.app.on_update_dismiss_clicked(move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_update_available(false);
            }
        });
    }

    // --- Avatar: slot clicked ---
    {
        let app_weak = ctx.app.as_weak();
        let state = ctx.avatar_state.clone();
        let timer_holder = ctx.avatar_shuffle_timer.clone();
        let rt_handle = ctx.rt.clone();
        ctx.app.on_avatar_slot_clicked(move |index| {
            let idx = index as usize;
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let current = state.lock().unwrap().selected_slot;
            if current == Some(idx) {
                state.lock().unwrap().selected_slot = None;
                app.set_selected_avatar(-1);
                start_ambient_shuffle(app.as_weak(), &state, &timer_holder, &rt_handle);
            } else {
                state.lock().unwrap().selected_slot = Some(idx);
                app.set_selected_avatar(idx as i32);
                stop_ambient_shuffle(&timer_holder);
            }
        });
    }

    // --- Avatar: reroll ---
    {
        let app_weak = ctx.app.as_weak();
        let state = ctx.avatar_state.clone();
        let timer_holder = ctx.avatar_shuffle_timer.clone();
        let rt_handle = ctx.rt.clone();
        ctx.app.on_avatar_reroll_clicked(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            state.lock().unwrap().selected_slot = None;
            app.set_selected_avatar(-1);
            stop_ambient_shuffle(&timer_holder);
            state.lock().unwrap().roll_counter += 1;

            let http = reqwest::Client::new();
            for i in 0..7usize {
                let style = avatar::AvatarGridState::pick_random_style().to_string();
                let seed = state.lock().unwrap().make_seed(i);
                {
                    let mut s = state.lock().unwrap();
                    s.slots[i].style = style.clone();
                    s.slots[i].seed = seed.clone();
                    s.flipping[i] = true;
                }
                let http = http.clone();
                let app_weak2 = app.as_weak();
                let state2 = state.clone();
                let delay = i as u64 * 100;
                rt_handle.spawn(async move {
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    if let Some((svg_data, rgba)) =
                        avatar::fetch_and_rasterize(&http, &style, &seed).await
                    {
                        state2.lock().unwrap().slots[i].svg_data = Some(svg_data);
                        let _ = slint::invoke_from_event_loop(move || {
                            let image = avatar::rgba_to_image(
                                &rgba,
                                avatar::RENDER_SIZE,
                                avatar::RENDER_SIZE,
                            );
                            let Some(app) = app_weak2.upgrade() else {
                                return;
                            };
                            set_avatar_back_and_flip(&app, i, image);
                            let app_weak3 = app.as_weak();
                            let state3 = state2.clone();
                            let timer = slint::Timer::default();
                            timer.start(
                                slint::TimerMode::SingleShot,
                                Duration::from_millis(1100),
                                move || {
                                    state3.lock().unwrap().flipping[i] = false;
                                    let Some(app) = app_weak3.upgrade() else {
                                        return;
                                    };
                                    copy_back_to_front(&app, i);
                                },
                            );
                            std::mem::forget(timer);
                        });
                    } else {
                        state2.lock().unwrap().flipping[i] = false;
                    }
                });
            }

            let timer_holder2 = timer_holder.clone();
            let state3 = state.clone();
            let app_weak3 = app.as_weak();
            let rt_h = rt_handle.clone();
            let restart_timer = slint::Timer::default();
            restart_timer.start(
                slint::TimerMode::SingleShot,
                Duration::from_millis(7 * 100 + 1200),
                move || {
                    start_ambient_shuffle(app_weak3.clone(), &state3, &timer_holder2, &rt_h);
                },
            );
            std::mem::forget(restart_timer);
        });
    }

    // --- Avatar: upload ---
    {
        let app_weak = ctx.app.as_weak();
        let state = ctx.avatar_state.clone();
        let timer_holder = ctx.avatar_shuffle_timer.clone();
        let rt_handle = ctx.rt.clone();
        ctx.app.on_avatar_upload_clicked(move || {
            stop_ambient_shuffle(&timer_holder);
            let app_weak = app_weak.clone();
            let state = state.clone();
            rt_handle.spawn(async move {
                let file = rfd::AsyncFileDialog::new()
                    .add_filter("Images", &["png", "jpg", "jpeg", "webp"])
                    .pick_file()
                    .await;
                let Some(file) = file else { return };
                let data = file.read().await;
                let Ok(img) = image::load_from_memory(&data) else {
                    log::warn!("[avatar] failed to decode uploaded image");
                    return;
                };
                let (w, h) = (img.width(), img.height());
                let side = w.min(h);
                let x = (w - side) / 2;
                let y = (h - side) / 2;
                let cropped = img.crop_imm(x, y, side, side);
                let resized = cropped.resize_exact(256, 256, image::imageops::CatmullRom);
                let rgba = resized.to_rgba8();

                let mut png_bytes = Vec::new();
                {
                    let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
                    use image::ImageEncoder;
                    let _ = encoder.write_image(
                        rgba.as_raw(),
                        256,
                        256,
                        image::ExtendedColorType::Rgba8,
                    );
                }

                let rgba_bytes = rgba.into_raw();

                let _ = slint::invoke_from_event_loop(move || {
                    let slint_img = avatar::rgba_to_image(&rgba_bytes, 256, 256);
                    let Some(app) = app_weak.upgrade() else {
                        return;
                    };
                    app.set_upload_preview(slint_img);
                    app.set_has_upload(true);
                    {
                        let mut s = state.lock().unwrap();
                        s.selected_slot = Some(7);
                        s.upload_data = Some(png_bytes);
                    }
                    app.set_selected_avatar(7);
                });
            });
        });
    }

    // --- Easter egg: hold-to-spin ---
    wire_easter_egg(ctx);
}

struct EasterEggState {
    slot_index: usize,
    angle: f32,
    speed: f32,
    word_index: usize,
    last_swap_half: i32,
    decelerating: bool,
}

fn wire_easter_egg(ctx: &AppContext) {
    let easter_egg: Rc<RefCell<Option<EasterEggState>>> = Rc::new(RefCell::new(None));
    let hold_timer: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));
    let spin_timer: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));

    {
        let app_weak = ctx.app.as_weak();
        let easter_egg = easter_egg.clone();
        let hold_timer_ref = hold_timer.clone();
        let spin_timer_ref = spin_timer.clone();
        let shuffle_timer = ctx.avatar_shuffle_timer.clone();
        let avatar_st = ctx.avatar_state.clone();
        let rt_handle = ctx.rt.clone();

        ctx.app.on_avatar_pointer_down(move |index| {
            let idx = index as usize;
            if idx >= 7 {
                return;
            }
            if easter_egg.borrow().is_some() {
                return;
            }

            let app_weak2 = app_weak.clone();
            let easter_egg2 = easter_egg.clone();
            let spin_timer_ref2 = spin_timer_ref.clone();
            let shuffle_timer2 = shuffle_timer.clone();
            let avatar_st2 = avatar_st.clone();
            let rt_handle2 = rt_handle.clone();

            let timer = slint::Timer::default();
            timer.start(
                slint::TimerMode::SingleShot,
                Duration::from_secs(3),
                move || {
                    log::debug!("[easter] hold triggered on slot {}", idx);
                    stop_ambient_shuffle(&shuffle_timer2);

                    let front_word = avatar::EASTER_WORDS[0];
                    let back_word = avatar::EASTER_WORDS[1];

                    *easter_egg2.borrow_mut() = Some(EasterEggState {
                        slot_index: idx,
                        angle: 0.0,
                        speed: 2.8,
                        word_index: 0,
                        last_swap_half: 0,
                        decelerating: false,
                    });

                    if let Some(app) = app_weak2.upgrade() {
                        app.set_spinning_slot(idx as i32);
                        app.set_spin_angle(0.0);
                        app.set_spin_text_front(front_word.into());
                        app.set_spin_text_back(back_word.into());
                        app.set_spin_heart_front(avatar::is_heart(front_word));
                        app.set_spin_heart_back(avatar::is_heart(back_word));
                    }

                    let app_weak3 = app_weak2.clone();
                    let easter_egg3 = easter_egg2.clone();
                    let spin_timer_ref3 = spin_timer_ref2.clone();
                    let shuffle_timer3 = shuffle_timer2.clone();
                    let avatar_st3 = avatar_st2.clone();
                    let rt_h = rt_handle2.clone();

                    let stimer = slint::Timer::default();
                    stimer.start(
                        slint::TimerMode::Repeated,
                        Duration::from_millis(16),
                        move || {
                            let mut egg_opt = easter_egg3.borrow_mut();
                            let Some(egg) = egg_opt.as_mut() else {
                                return;
                            };

                            if egg.decelerating {
                                egg.speed *= 0.96;
                                if egg.speed < 0.5 {
                                    let slot = egg.slot_index;
                                    drop(egg_opt);

                                    if let Some(t) = spin_timer_ref3.borrow_mut().take() {
                                        t.stop();
                                    }
                                    *easter_egg3.borrow_mut() = None;

                                    if let Some(app) = app_weak3.upgrade() {
                                        app.set_spinning_slot(-1);
                                        app.set_spin_angle(0.0);
                                    }

                                    let style =
                                        avatar::AvatarGridState::pick_random_style().to_string();
                                    let seed = avatar_st3.lock().unwrap().make_shuffle_seed(slot);
                                    {
                                        let mut s = avatar_st3.lock().unwrap();
                                        s.slots[slot].style = style.clone();
                                        s.slots[slot].seed = seed.clone();
                                    }
                                    let http = reqwest::Client::new();
                                    let app_w = app_weak3.clone();
                                    let state_c = avatar_st3.clone();
                                    rt_h.spawn(async move {
                                        if let Some((svg_data, rgba)) =
                                            avatar::fetch_and_rasterize(&http, &style, &seed).await
                                        {
                                            state_c.lock().unwrap().slots[slot].svg_data =
                                                Some(svg_data);
                                            let _ = slint::invoke_from_event_loop(move || {
                                                let image = avatar::rgba_to_image(
                                                    &rgba,
                                                    avatar::RENDER_SIZE,
                                                    avatar::RENDER_SIZE,
                                                );
                                                if let Some(app) = app_w.upgrade() {
                                                    set_avatar_image(&app, slot, image);
                                                }
                                            });
                                        }
                                    });

                                    if avatar_st3.lock().unwrap().selected_slot.is_none() {
                                        start_ambient_shuffle(
                                            app_weak3.clone(),
                                            &avatar_st3,
                                            &shuffle_timer3,
                                            &rt_h,
                                        );
                                    }
                                    log::debug!("[easter] spin ended on slot {}", slot);
                                    return;
                                }
                            } else if egg.speed < 8.4 {
                                egg.speed += 0.15;
                            }

                            egg.angle = (egg.angle + egg.speed) % 360.0;
                            let cur_half = (egg.angle / 180.0) as i32;

                            if cur_half != egg.last_swap_half {
                                egg.last_swap_half = cur_half;
                                egg.word_index = (egg.word_index + 1) % avatar::EASTER_WORDS.len();
                                let next_word = avatar::EASTER_WORDS
                                    [(egg.word_index + 1) % avatar::EASTER_WORDS.len()];

                                if let Some(app) = app_weak3.upgrade() {
                                    if cur_half == 1 {
                                        app.set_spin_text_front(next_word.into());
                                        app.set_spin_heart_front(avatar::is_heart(next_word));
                                    } else {
                                        app.set_spin_text_back(next_word.into());
                                        app.set_spin_heart_back(avatar::is_heart(next_word));
                                    }
                                }
                            }

                            if let Some(app) = app_weak3.upgrade() {
                                app.set_spin_angle(egg.angle);
                            }
                        },
                    );
                    *spin_timer_ref2.borrow_mut() = Some(stimer);
                },
            );
            *hold_timer_ref.borrow_mut() = Some(timer);
        });
    }

    {
        let easter_egg = easter_egg.clone();
        let hold_timer_ref = hold_timer.clone();

        ctx.app.on_avatar_pointer_up(move |index| {
            let idx = index as usize;

            if let Some(t) = hold_timer_ref.borrow_mut().take() {
                t.stop();
            }

            let mut egg_opt = easter_egg.borrow_mut();
            if let Some(egg) = egg_opt.as_mut() {
                if egg.slot_index == idx {
                    egg.decelerating = true;
                    log::debug!("[easter] release — decelerating slot {}", idx);
                }
            }
        });
    }
}

// --- Avatar grid helpers ---

pub fn load_avatar_grid(
    app_weak: slint::Weak<MainWindow>,
    state: &Arc<Mutex<avatar::AvatarGridState>>,
    rt: &tokio::runtime::Handle,
) {
    log::info!("[avatar] load_avatar_grid: spawning 7 fetch tasks");
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
            log::debug!("[avatar] slot {} fetching style={} seed={}", i, style, seed);
            match avatar::fetch_and_rasterize(&http, &style, &seed).await {
                Some((svg_data, rgba)) => {
                    log::debug!("[avatar] slot {} fetch OK, rgba len={}", i, rgba.len());
                    state_clone.lock().unwrap().slots[i].svg_data = Some(svg_data);
                    let _ = slint::invoke_from_event_loop(move || {
                        log::debug!("[avatar] slot {} pushing image to UI", i);
                        let image =
                            avatar::rgba_to_image(&rgba, avatar::RENDER_SIZE, avatar::RENDER_SIZE);
                        let Some(app) = app_weak.upgrade() else {
                            log::warn!("[avatar] slot {} app_weak gone", i);
                            return;
                        };
                        set_avatar_image(&app, i, image);
                        set_avatar_loaded(&app, i, true);
                    });
                }
                None => {
                    log::warn!("[avatar] slot {} fetch_and_rasterize returned None", i);
                }
            }
        });
    }
    // Stagger deal-in animations
    for i in 0..7usize {
        let app_weak = app_weak.clone();
        let delay = Duration::from_millis(i as u64 * 60);
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::SingleShot, delay, move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            set_avatar_deal(&app, i, true);
        });
        std::mem::forget(timer);
    }
    // Upload slot deal-in (last, after all 7)
    {
        let app_weak = app_weak.clone();
        let delay = Duration::from_millis(7 * 60);
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::SingleShot, delay, move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_avatar_deal_upload(true);
        });
        std::mem::forget(timer);
    }
}

pub fn start_ambient_shuffle(
    app_weak: slint::Weak<MainWindow>,
    state: &Arc<Mutex<avatar::AvatarGridState>>,
    timer_holder: &Rc<RefCell<Option<slint::Timer>>>,
    rt: &tokio::runtime::Handle,
) {
    stop_ambient_shuffle(timer_holder);
    let timer = slint::Timer::default();
    let app_weak = app_weak.clone();
    let state = state.clone();
    let rt = rt.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(3),
        move || {
            let Some(slot_idx) = state.lock().unwrap().pick_unselected_non_flipping_slot() else {
                return;
            };
            let style = avatar::AvatarGridState::pick_random_style().to_string();
            let seed = state.lock().unwrap().make_shuffle_seed(slot_idx);
            {
                let mut s = state.lock().unwrap();
                s.flipping[slot_idx] = true;
                s.slots[slot_idx].style = style.clone();
                s.slots[slot_idx].seed = seed.clone();
            }

            let http = reqwest::Client::new();
            let app_weak2 = app_weak.clone();
            let state2 = state.clone();
            rt.spawn(async move {
                if let Some((svg_data, rgba)) =
                    avatar::fetch_and_rasterize(&http, &style, &seed).await
                {
                    state2.lock().unwrap().slots[slot_idx].svg_data = Some(svg_data);
                    let _ = slint::invoke_from_event_loop(move || {
                        let image =
                            avatar::rgba_to_image(&rgba, avatar::RENDER_SIZE, avatar::RENDER_SIZE);
                        let Some(app) = app_weak2.upgrade() else {
                            return;
                        };
                        set_avatar_back_and_flip(&app, slot_idx, image);
                        let app_weak3 = app.as_weak();
                        let state3 = state2.clone();
                        let timer = slint::Timer::default();
                        timer.start(
                            slint::TimerMode::SingleShot,
                            Duration::from_millis(1100),
                            move || {
                                state3.lock().unwrap().flipping[slot_idx] = false;
                                let Some(app) = app_weak3.upgrade() else {
                                    return;
                                };
                                copy_back_to_front(&app, slot_idx);
                            },
                        );
                        std::mem::forget(timer);
                    });
                } else {
                    state2.lock().unwrap().flipping[slot_idx] = false;
                }
            });
        },
    );
    *timer_holder.borrow_mut() = Some(timer);
}

pub fn stop_ambient_shuffle(timer_holder: &Rc<RefCell<Option<slint::Timer>>>) {
    if let Some(timer) = timer_holder.borrow_mut().take() {
        timer.stop();
    }
}

// Avatar slot helper functions to avoid the giant match repetition
fn set_avatar_image(app: &MainWindow, slot: usize, image: slint::Image) {
    match slot {
        0 => app.set_avatar_img_0(image),
        1 => app.set_avatar_img_1(image),
        2 => app.set_avatar_img_2(image),
        3 => app.set_avatar_img_3(image),
        4 => app.set_avatar_img_4(image),
        5 => app.set_avatar_img_5(image),
        6 => app.set_avatar_img_6(image),
        _ => {}
    }
}

fn set_avatar_loaded(app: &MainWindow, slot: usize, loaded: bool) {
    match slot {
        0 => app.set_avatar_loaded_0(loaded),
        1 => app.set_avatar_loaded_1(loaded),
        2 => app.set_avatar_loaded_2(loaded),
        3 => app.set_avatar_loaded_3(loaded),
        4 => app.set_avatar_loaded_4(loaded),
        5 => app.set_avatar_loaded_5(loaded),
        6 => app.set_avatar_loaded_6(loaded),
        _ => {}
    }
}

fn set_avatar_deal(app: &MainWindow, slot: usize, deal: bool) {
    match slot {
        0 => app.set_avatar_deal_0(deal),
        1 => app.set_avatar_deal_1(deal),
        2 => app.set_avatar_deal_2(deal),
        3 => app.set_avatar_deal_3(deal),
        4 => app.set_avatar_deal_4(deal),
        5 => app.set_avatar_deal_5(deal),
        6 => app.set_avatar_deal_6(deal),
        _ => {}
    }
}

fn set_avatar_back_and_flip(app: &MainWindow, slot: usize, image: slint::Image) {
    match slot {
        0 => {
            app.set_avatar_back_0(image);
            let v = app.get_avatar_flip_0();
            app.set_avatar_flip_0(!v);
        }
        1 => {
            app.set_avatar_back_1(image);
            let v = app.get_avatar_flip_1();
            app.set_avatar_flip_1(!v);
        }
        2 => {
            app.set_avatar_back_2(image);
            let v = app.get_avatar_flip_2();
            app.set_avatar_flip_2(!v);
        }
        3 => {
            app.set_avatar_back_3(image);
            let v = app.get_avatar_flip_3();
            app.set_avatar_flip_3(!v);
        }
        4 => {
            app.set_avatar_back_4(image);
            let v = app.get_avatar_flip_4();
            app.set_avatar_flip_4(!v);
        }
        5 => {
            app.set_avatar_back_5(image);
            let v = app.get_avatar_flip_5();
            app.set_avatar_flip_5(!v);
        }
        6 => {
            app.set_avatar_back_6(image);
            let v = app.get_avatar_flip_6();
            app.set_avatar_flip_6(!v);
        }
        _ => {}
    }
}

fn copy_back_to_front(app: &MainWindow, slot: usize) {
    match slot {
        0 => app.set_avatar_img_0(app.get_avatar_back_0()),
        1 => app.set_avatar_img_1(app.get_avatar_back_1()),
        2 => app.set_avatar_img_2(app.get_avatar_back_2()),
        3 => app.set_avatar_img_3(app.get_avatar_back_3()),
        4 => app.set_avatar_img_4(app.get_avatar_back_4()),
        5 => app.set_avatar_img_5(app.get_avatar_back_5()),
        6 => app.set_avatar_img_6(app.get_avatar_back_6()),
        _ => {}
    }
}
