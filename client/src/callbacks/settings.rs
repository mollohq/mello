use mello_core::Command;
use slint::ComponentHandle;

use crate::app_context::AppContext;
use crate::platform;
use crate::Settings;

pub fn wire(ctx: &AppContext) {
    // --- Settings modal open ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        let s = ctx.settings.clone();
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
                let ptt_label: slint::SharedString = if let Some(ref key_str) = settings.ptt_key {
                    platform::hotkeys::parse_hotkey_string(key_str)
                        .map(|(_, label)| label)
                        .unwrap_or_else(|| "Unassigned".into())
                } else {
                    "Unassigned".into()
                }
                .into();
                app.set_settings_ptt_key_label(ptt_label);
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
    {
        let s = ctx.settings.clone();
        ctx.app.on_setting_changed_input_volume(move |v| {
            let mut settings = s.borrow_mut();
            settings.input_volume = v;
            settings.save();
        });
    }
    {
        let s = ctx.settings.clone();
        ctx.app.on_setting_changed_output_volume(move |v| {
            let mut settings = s.borrow_mut();
            settings.output_volume = v;
            settings.save();
        });
    }
    {
        let s = ctx.settings.clone();
        ctx.app.on_setting_changed_noise_suppression(move |v| {
            let mut settings = s.borrow_mut();
            settings.noise_suppression = v;
            settings.save();
        });
    }
    {
        let s = ctx.settings.clone();
        ctx.app.on_setting_changed_echo_cancellation(move |v| {
            let mut settings = s.borrow_mut();
            settings.echo_cancellation = v;
            settings.save();
        });
    }
    {
        let s = ctx.settings.clone();
        ctx.app.on_setting_changed_input_mode(move |is_ptt| {
            let mut settings = s.borrow_mut();
            settings.input_mode = if is_ptt {
                "push_to_talk".into()
            } else {
                "voice_activity".into()
            };
            settings.save();
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
                if let Some((hotkey, label)) = platform::hotkeys::slint_key_to_hotkey(
                    key_text.as_str(),
                    ctrl,
                    alt,
                    shift,
                    meta,
                ) {
                    let hotkey_str = hotkey.into_string();
                    match hk.borrow_mut().register_ptt(hotkey) {
                        Ok(_) => {
                            log::info!("PTT key registered: {} ({})", label, hotkey_str);
                            let mut settings = s.borrow_mut();
                            settings.ptt_key = Some(hotkey_str);
                            settings.save();
                            if let Some(app) = app_weak.upgrade() {
                                app.set_settings_ptt_key_label(label.into());
                            }
                        }
                        Err(e) => {
                            log::warn!("Failed to register PTT key: {}", e);
                        }
                    }
                } else {
                    log::warn!("Could not map key to hotkey: {:?}", key_text.as_str());
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
            hk.borrow_mut().unregister_ptt();
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
}
