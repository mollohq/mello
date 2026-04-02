use std::rc::Rc;

use mello_core::Command;
use slint::{ComponentHandle, Model};

use crate::VoiceChannelData;
use crate::app_context::AppContext;

pub fn wire(ctx: &AppContext) {
    // --- Voice toggle (leave) ---
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_voice_toggle(move || {
            let _ = cmd.try_send(Command::LeaveVoice);
        });
    }

    // --- Mic toggle ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_mic_toggle(move || {
            if let Some(app) = app_weak.upgrade() {
                let new_muted = !app.get_mic_muted();
                app.set_mic_muted(new_muted);
                let _ = cmd.try_send(Command::SetMute { muted: new_muted });
            }
        });
    }

    // --- Deafen toggle ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        let mbd = ctx.muted_before_deafen.clone();
        ctx.app.on_deafen_toggle(move || {
            if let Some(app) = app_weak.upgrade() {
                let new_deafened = !app.get_deafened();
                app.set_deafened(new_deafened);
                let _ = cmd.try_send(Command::SetDeafen {
                    deafened: new_deafened,
                });

                if new_deafened {
                    mbd.set(app.get_mic_muted());
                    if !app.get_mic_muted() {
                        app.set_mic_muted(true);
                        let _ = cmd.try_send(Command::SetMute { muted: true });
                    }
                } else {
                    if !mbd.get() {
                        app.set_mic_muted(false);
                        let _ = cmd.try_send(Command::SetMute { muted: false });
                    }
                }
            }
        });
    }

    // --- Voice channel callbacks ---
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_join_voice_channel(move |channel_id| {
            log::info!("UI: join voice channel '{}'", channel_id);
            let _ = cmd.try_send(Command::JoinVoice {
                channel_id: channel_id.to_string(),
            });
        });
    }
    {
        let app_weak = ctx.app.as_weak();
        ctx.app.on_toggle_voice_channel(move |channel_id| {
            log::info!("UI: toggle voice channel '{}'", channel_id);
            if let Some(app) = app_weak.upgrade() {
                let current = app.get_voice_channels();
                log::info!("UI: current voice channels count={}", current.row_count());
                let updated: Vec<VoiceChannelData> = (0..current.row_count())
                    .map(|i| {
                        let mut ch = current.row_data(i).unwrap();
                        if ch.id == channel_id {
                            log::info!(
                                "UI: toggling '{}' expanded {} -> {}",
                                ch.name,
                                ch.expanded,
                                !ch.expanded
                            );
                            ch.expanded = !ch.expanded;
                        }
                        ch
                    })
                    .collect();
                app.set_voice_channels(Rc::new(slint::VecModel::from(updated)).into());
            }
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_create_voice_channel(move |name| {
            let name = name.trim().to_string();
            if name.is_empty() {
                return;
            }
            log::info!("UI: create voice channel '{}'", name);
            if let Some(app) = app_weak.upgrade() {
                let crew_id = app.get_active_crew_id().to_string();
                let _ = cmd.try_send(Command::CreateVoiceChannel { crew_id, name });
            }
        });
    }

    // --- Mic permission ---
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_request_mic_permission(move || {
            log::info!("UI: requesting mic permission");
            let _ = cmd.try_send(Command::RequestMicPermission);
        });
    }
    {
        ctx.app.on_open_mic_settings(move || {
            log::info!("UI: opening mic settings");
            #[cfg(target_os = "macos")]
            {
                let _ = std::process::Command::new("open")
                    .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")
                    .spawn();
            }
        });
    }
}
