use std::rc::Rc;
use std::time::Duration;

use mello_core::{Command, Event};
use slint::ComponentHandle;

use crate::app_context::AppContext;
use crate::notifications;
use crate::platform::{self, StatusItem, VoiceState};
use crate::updater::UpdateEvent;

pub fn start(
    ctx: &AppContext,
    event_rx: std::sync::mpsc::Receiver<Event>,
    update_event_rx: std::sync::mpsc::Receiver<UpdateEvent>,
) -> slint::Timer {
    let poll_ctx = AppContext {
        app: ctx.app.clone_strong(),
        cmd_tx: ctx.cmd_tx.clone(),
        settings: ctx.settings.clone(),
        rt: ctx.rt.clone(),
        active_voice_channel: ctx.active_voice_channel.clone(),
        new_crew_avatar_b64: ctx.new_crew_avatar_b64.clone(),
        invited_users: ctx.invited_users.clone(),
        discover_cursor: ctx.discover_cursor.clone(),
        discover_loading: ctx.discover_loading.clone(),
        chat_messages: ctx.chat_messages.clone(),
        avatar_state: ctx.avatar_state.clone(),
        profile_avatar_state: ctx.profile_avatar_state.clone(),
        avatar_shuffle_timer: ctx.avatar_shuffle_timer.clone(),
        muted_before_deafen: ctx.muted_before_deafen.clone(),
        updater: ctx.updater.clone(),
        hotkey_mgr: ctx.hotkey_mgr.clone(),
        status_item: ctx.status_item.clone(),
        gif_popover_anim: ctx.gif_popover_anim.clone(),
        gif_chat_anim: ctx.gif_chat_anim.clone(),
        dbg_hist: ctx.dbg_hist.clone(),
        avatar_cache: ctx.avatar_cache.clone(),
        hud_manager: ctx.hud_manager.clone(),
        fg_monitor: ctx.fg_monitor.clone(),
    };

    let saved_timer = Rc::new(slint::Timer::default());
    let saved_timer_ref = saved_timer.clone();
    let saved_app_weak = ctx.app.as_weak();

    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(50),
        move || {
            // --- Update events ---
            while let Ok(ue) = update_event_rx.try_recv() {
                match ue {
                    UpdateEvent::CheckComplete {
                        update_available,
                        version,
                        download_size,
                        ..
                    } => {
                        if update_available {
                            poll_ctx.app.set_update_available(true);
                            if let Some(v) = version {
                                poll_ctx.app.set_update_version(v.into());
                            }
                            log::info!("Update available, size: {:?} bytes", download_size);
                        }
                    }
                    UpdateEvent::DownloadProgress { progress } => {
                        poll_ctx.app.set_update_download_progress(progress);
                    }
                    UpdateEvent::Error(msg) => {
                        log::warn!("Update error: {}", msg);
                        poll_ctx.app.set_update_available(false);
                    }
                    UpdateEvent::CheckStarted => {}
                }
            }

            // --- Core events ---
            while let Ok(event) = event_rx.try_recv() {
                // Update tray icon based on voice state changes
                match &event {
                    Event::VoiceStateChanged { in_call } => {
                        let state = if *in_call {
                            VoiceState::Connected
                        } else {
                            VoiceState::Inactive
                        };
                        poll_ctx.status_item.borrow_mut().set_voice_state(state);
                        poll_ctx.fg_monitor.borrow_mut().set_in_voice(*in_call);
                    }
                    Event::VoiceActivity { speaking, .. } => {
                        if poll_ctx.app.get_mic_muted() {
                            poll_ctx
                                .status_item
                                .borrow_mut()
                                .set_voice_state(VoiceState::Muted);
                        } else if *speaking {
                            poll_ctx
                                .status_item
                                .borrow_mut()
                                .set_voice_state(VoiceState::Speaking);
                        } else {
                            poll_ctx
                                .status_item
                                .borrow_mut()
                                .set_voice_state(VoiceState::Connected);
                        }
                    }
                    Event::GameDetected { pid, .. } => {
                        poll_ctx
                            .fg_monitor
                            .borrow_mut()
                            .set_game_active(true, Some(*pid));
                    }
                    Event::GameEnded { .. } => {
                        poll_ctx
                            .fg_monitor
                            .borrow_mut()
                            .set_game_active(false, None);
                    }
                    Event::MemberJoined { member, .. } => {
                        if !poll_ctx.app.window().is_visible() {
                            notifications::notify_member_joined(&member.display_name);
                        }
                    }
                    _ => {}
                }
                // Push HUD state on voice-related events
                let should_push_hud = matches!(
                    &event,
                    Event::VoiceStateChanged { .. }
                        | Event::VoiceActivity { .. }
                        | Event::VoiceJoined { .. }
                        | Event::VoiceUpdated { .. }
                        | Event::GameDetected { .. }
                        | Event::GameEnded { .. }
                        | Event::MessageReceived { .. }
                        | Event::CrewStateLoaded { .. }
                );

                crate::handlers::handle_event(&poll_ctx, event);

                if should_push_hud && poll_ctx.hud_manager.is_enabled() {
                    let mode = poll_ctx.fg_monitor.borrow().current_mode();
                    let state = crate::hud_state_builder::build_hud_state(&poll_ctx, mode);
                    poll_ctx.hud_manager.push_state(state);
                }
            }

            // --- Tray icon left-click: toggle window visibility ---
            while let Some(event) = StatusItem::poll_tray_event() {
                if let tray_icon::TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    button_state: tray_icon::MouseButtonState::Down,
                    ..
                } = event
                {
                    if poll_ctx.app.window().is_visible() {
                        poll_ctx.app.hide().ok();
                    } else {
                        poll_ctx.app.show().ok();
                    }
                }
            }

            // --- Tray context-menu + menu bar events ---
            while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                let id = event.id().as_ref();
                match id {
                    "tray_open" => {
                        poll_ctx.app.show().ok();
                    }
                    "tray_mute" => {
                        let new_muted = !poll_ctx.app.get_mic_muted();
                        poll_ctx.app.set_mic_muted(new_muted);
                        let _ = poll_ctx
                            .cmd_tx
                            .try_send(Command::SetMute { muted: new_muted });
                        poll_ctx
                            .status_item
                            .borrow_mut()
                            .set_mute_checked(new_muted);
                    }
                    "tray_leave" => {
                        let _ = poll_ctx.cmd_tx.try_send(Command::LeaveVoice);
                    }
                    "tray_quit" => {
                        log::info!("[quit] tray quit");
                        slint::quit_event_loop().ok();
                    }
                    _ =>
                    {
                        #[cfg(target_os = "macos")]
                        match id {
                            "prefs" => {
                                let _ = poll_ctx.cmd_tx.try_send(Command::ListAudioDevices);
                                let settings = poll_ctx.settings.borrow();
                                poll_ctx
                                    .app
                                    .set_settings_start_on_boot(settings.start_on_boot);
                                poll_ctx
                                    .app
                                    .set_settings_start_minimized(settings.start_minimized);
                                poll_ctx
                                    .app
                                    .set_settings_close_to_tray(settings.close_to_tray);
                                poll_ctx
                                    .app
                                    .set_settings_auto_connect(settings.auto_connect);
                                poll_ctx
                                    .app
                                    .set_settings_minimize_on_join(settings.minimize_on_join);
                                poll_ctx
                                    .app
                                    .set_settings_hw_acceleration(settings.hardware_acceleration);
                                poll_ctx
                                    .app
                                    .set_settings_input_volume(settings.input_volume);
                                poll_ctx
                                    .app
                                    .set_settings_output_volume(settings.output_volume);
                                poll_ctx
                                    .app
                                    .set_settings_noise_suppression(settings.noise_suppression);
                                poll_ctx
                                    .app
                                    .set_settings_echo_cancellation(settings.echo_cancellation);
                                poll_ctx
                                    .app
                                    .set_settings_ptt_mode(settings.input_mode == "push_to_talk");
                                poll_ctx
                                    .app
                                    .set_settings_vad_threshold(settings.vad_threshold);
                                let ptt_label: slint::SharedString =
                                    if let Some(ref key_str) = settings.ptt_key {
                                        platform::hotkeys::parse_hotkey_string(key_str)
                                            .map(|(_, label)| label)
                                            .unwrap_or_else(|| "Unassigned".into())
                                    } else {
                                        "Unassigned".into()
                                    }
                                    .into();
                                poll_ctx.app.set_settings_ptt_key_label(ptt_label);
                                poll_ctx.app.set_settings_open(true);
                            }
                            "mute" => {
                                let new_muted = !poll_ctx.app.get_mic_muted();
                                poll_ctx.app.set_mic_muted(new_muted);
                                let _ = poll_ctx
                                    .cmd_tx
                                    .try_send(Command::SetMute { muted: new_muted });
                            }
                            "deafen" => {
                                let new_deafened = !poll_ctx.app.get_deafened();
                                poll_ctx.app.set_deafened(new_deafened);
                                let _ = poll_ctx.cmd_tx.try_send(Command::SetDeafen {
                                    deafened: new_deafened,
                                });
                                if new_deafened {
                                    poll_ctx
                                        .muted_before_deafen
                                        .set(poll_ctx.app.get_mic_muted());
                                    if !poll_ctx.app.get_mic_muted() {
                                        poll_ctx.app.set_mic_muted(true);
                                        let _ = poll_ctx
                                            .cmd_tx
                                            .try_send(Command::SetMute { muted: true });
                                    }
                                } else if !poll_ctx.muted_before_deafen.get() {
                                    poll_ctx.app.set_mic_muted(false);
                                    let _ =
                                        poll_ctx.cmd_tx.try_send(Command::SetMute { muted: false });
                                }
                            }
                            "github" => {
                                if let Err(e) = open::that("https://github.com/mollohq/mello") {
                                    log::warn!("Failed to open GitHub URL: {}", e);
                                }
                            }
                            "check_updates" => {
                                if let Some(ref mut u) = *poll_ctx.updater.borrow_mut() {
                                    u.check_for_updates();
                                } else if let Err(e) =
                                    open::that("https://github.com/mollohq/mello/releases")
                                {
                                    log::warn!("Failed to open releases URL: {}", e);
                                }
                            }
                            _ => {
                                log::debug!("Unhandled menu event: {}", id);
                            }
                        }
                    }
                }
            }

            // --- Global hotkey events (PTT) ---
            while let Some(event) = platform::hotkeys::HotkeyManager::poll() {
                let mgr = poll_ctx.hotkey_mgr.borrow();
                if let Some(ptt_id) = mgr.ptt_id() {
                    if event.id == ptt_id {
                        let pressed = event.state == global_hotkey::HotKeyState::Pressed;
                        let _ = poll_ctx
                            .cmd_tx
                            .try_send(Command::SetMute { muted: !pressed });
                    }
                }
            }

            // --- HUD foreground monitor + state push ---
            if poll_ctx.hud_manager.is_enabled() {
                // Update foreground monitor with current voice state
                {
                    let mut fg = poll_ctx.fg_monitor.borrow_mut();
                    fg.set_in_voice(poll_ctx.app.get_in_voice());
                }

                let main_visible = poll_ctx.app.window().is_visible();
                let mode_changed = poll_ctx.fg_monitor.borrow_mut().evaluate(main_visible);
                if let Some(new_mode) = mode_changed {
                    let state = crate::hud_state_builder::build_hud_state(&poll_ctx, new_mode);
                    poll_ctx.hud_manager.push_state(state);
                }
            }

            // --- HUD actions ---
            while let Some(action) = poll_ctx.hud_manager.poll_action() {
                match action {
                    crate::hud_manager::HudAction::Action { action } => match action {
                        crate::hud_manager::HudActionKind::MuteToggle => {
                            let new_muted = !poll_ctx.app.get_mic_muted();
                            poll_ctx.app.set_mic_muted(new_muted);
                            let _ = poll_ctx
                                .cmd_tx
                                .try_send(Command::SetMute { muted: new_muted });
                            poll_ctx
                                .status_item
                                .borrow_mut()
                                .set_mute_checked(new_muted);
                        }
                        crate::hud_manager::HudActionKind::LeaveVoice => {
                            let _ = poll_ctx.cmd_tx.try_send(Command::LeaveVoice);
                        }
                        crate::hud_manager::HudActionKind::OpenCrew => {
                            poll_ctx.app.show().ok();
                            platform::bring_main_window_to_front();
                        }
                        crate::hud_manager::HudActionKind::OpenStream => {
                            poll_ctx.app.show().ok();
                            platform::bring_main_window_to_front();
                        }
                        crate::hud_manager::HudActionKind::DeafenToggle => {
                            log::info!("[hud] deafen toggle requested (not yet wired)");
                        }
                        crate::hud_manager::HudActionKind::OpenSettings => {
                            poll_ctx.app.show().ok();
                            platform::bring_main_window_to_front();
                        }
                    },
                }
            }

            // --- "Saved ✓" indicator: auto-hide after 2s ---
            if poll_ctx.app.get_settings_show_saved() && !saved_timer_ref.running() {
                let hide_weak = saved_app_weak.clone();
                saved_timer_ref.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_secs(2),
                    move || {
                        if let Some(app) = hide_weak.upgrade() {
                            app.set_settings_show_saved(false);
                        }
                    },
                );
            }
        },
    );
    timer
}
