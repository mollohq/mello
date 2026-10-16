use std::rc::Rc;

use mello_core::Command;
use slint::{ComponentHandle, Model};

use crate::app_context::AppContext;
use crate::member_audio::MemberAudio;
use crate::{MainWindow, VoiceChannelData, VoiceChannelMember};

fn update_local_vc_mute_state(app: &MainWindow) {
    let my_id = app.get_user_id();
    let muted = app.get_mic_muted();
    let deafened = app.get_deafened();
    let channels = app.get_voice_channels();
    let updated: Vec<VoiceChannelData> = (0..channels.row_count())
        .map(|i| {
            let mut ch = channels.row_data(i).unwrap();
            let members: Vec<VoiceChannelMember> = (0..ch.members.row_count())
                .map(|j| {
                    let mut m = ch.members.row_data(j).unwrap();
                    if m.id == my_id.as_str() {
                        m.muted = muted;
                        m.deafened = deafened;
                    }
                    m
                })
                .collect();
            ch.members = Rc::new(slint::VecModel::from(members)).into();
            ch
        })
        .collect();
    app.set_voice_channels(Rc::new(slint::VecModel::from(updated)).into());
}

/// Show the stored settings of `user_id` on its rows, and in the volume
/// popover when it is open for that member.
fn show_member_audio(app: &MainWindow, user_id: &str, audio: MemberAudio) {
    crate::converters::set_voice_member_audio(app, user_id, audio);
    if app.get_volume_member_id() == user_id {
        let mut m = app.get_volume_member();
        m.volume = i32::from(audio.volume_percent);
        m.local_muted = audio.muted_locally;
        app.set_volume_member(m);
    }
}

/// The local user has no volume of their own, and an empty id is no member.
fn is_other_member(app: &MainWindow, user_id: &str) -> bool {
    if user_id.is_empty() || user_id == app.get_user_id().as_str() {
        log::warn!("UI: member volume ignored for {:?}", user_id);
        return false;
    }
    true
}

pub fn wire(ctx: &AppContext) {
    // --- Member volume popover: volume and local mute, for this user only ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        let table = ctx.member_audio.clone();
        ctx.app.on_member_volume_changed(move |user_id, percent| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            if !is_other_member(&app, &user_id) {
                return;
            }
            let command = table.borrow_mut().set_volume(&user_id, percent);
            let _ = cmd.send(command);
            show_member_audio(&app, &user_id, table.borrow().get(&user_id));
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        let table = ctx.member_audio.clone();
        ctx.app.on_member_local_mute_changed(move |user_id, muted| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            if !is_other_member(&app, &user_id) {
                return;
            }
            log::info!("UI: member {} muted for you = {}", user_id, muted);
            let command = table.borrow_mut().set_muted_locally(&user_id, muted);
            let _ = cmd.send(command);
            show_member_audio(&app, &user_id, table.borrow().get(&user_id));
        });
    }

    // --- Voice toggle (leave) ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_voice_toggle(move || {
            // Show the hangup at once. The core confirms with
            // VoiceStateChanged { in_call: false }, which clears the channel
            // membership. The button must respond even when the core is busy.
            if let Some(app) = app_weak.upgrade() {
                app.set_in_voice(false);
            }
            let _ = cmd.send(Command::LeaveVoice);
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
                let _ = cmd.send(Command::SetMute { muted: new_muted });
                let _ = cmd.send(Command::BroadcastMuteState {
                    muted: new_muted,
                    deafened: app.get_deafened(),
                });
                update_local_vc_mute_state(&app);
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
                let _ = cmd.send(Command::SetDeafen {
                    deafened: new_deafened,
                });

                if new_deafened {
                    mbd.set(app.get_mic_muted());
                    if !app.get_mic_muted() {
                        app.set_mic_muted(true);
                        let _ = cmd.send(Command::SetMute { muted: true });
                    }
                } else {
                    if !mbd.get() {
                        app.set_mic_muted(false);
                        let _ = cmd.send(Command::SetMute { muted: false });
                    }
                }
                let _ = cmd.send(Command::BroadcastMuteState {
                    muted: app.get_mic_muted(),
                    deafened: new_deafened,
                });
                update_local_vc_mute_state(&app);
            }
        });
    }

    // --- Voice channel callbacks ---
    {
        let cmd = ctx.cmd_tx.clone();
        let settings = ctx.settings.clone();
        ctx.app.on_join_voice_channel(move |channel_id| {
            log::info!("UI: join voice channel '{}'", channel_id);
            let ptt = settings.borrow().input_mode == "push_to_talk";
            let _ = cmd.send(Command::SetPushToTalk { enabled: ptt });
            let _ = cmd.send(Command::JoinVoice {
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
                let _ = cmd.send(Command::CreateVoiceChannel { crew_id, name });
            }
        });
    }

    // --- Mic permission ---
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_request_mic_permission(move || {
            log::info!("UI: requesting mic permission");
            let _ = cmd.send(Command::RequestMicPermission);
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
