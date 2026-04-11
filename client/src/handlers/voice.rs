use std::rc::Rc;

use mello_core::{Command, Event};
use slint::Model;

use crate::app_context::AppContext;
use crate::converters::{
    channel_to_ui, channels_to_ui, set_level_history, update_active_crew_card, voice_members_to_ui,
    VoiceUiCtx,
};
use crate::{AudioDeviceData, MemberData, VoiceChannelData, VoiceChannelMember};

pub fn handle(ctx: &AppContext, event: Event) {
    match event {
        Event::VoiceStateChanged { in_call } => {
            ctx.app.set_in_voice(in_call);
            log::info!("UI: voice state changed, in_call={}", in_call);

            if !in_call {
                *ctx.active_voice_channel.borrow_mut() = String::new();
                ctx.app.set_voice_channel_name(slint::SharedString::new());

                let my_id = ctx.app.get_user_id();
                let current = ctx.app.get_voice_channels();
                let updated: Vec<VoiceChannelData> = (0..current.row_count())
                    .map(|i| {
                        let mut ch = current.row_data(i).unwrap();
                        ch.active = false;
                        let members: Vec<VoiceChannelMember> = (0..ch.members.row_count())
                            .filter_map(|j| {
                                let m = ch.members.row_data(j).unwrap();
                                if m.id == my_id {
                                    None
                                } else {
                                    Some(m)
                                }
                            })
                            .collect();
                        ch.member_count = members.len() as i32;
                        ch.members = Rc::new(slint::VecModel::from(members)).into();
                        ch
                    })
                    .collect();
                ctx.app
                    .set_voice_channels(Rc::new(slint::VecModel::from(updated)).into());
            }
        }
        Event::VoiceConnected { peer_id } => {
            log::info!("UI: voice connected to {}", peer_id);
        }
        Event::VoiceDisconnected { peer_id } => {
            log::info!("UI: voice disconnected from {}", peer_id);
        }
        Event::VoiceActivity {
            member_id,
            speaking,
        } => {
            let _ = ctx.cmd_tx.try_send(Command::VoiceSpeaking { speaking });

            let current = ctx.app.get_members();
            let members: Vec<MemberData> = (0..current.row_count())
                .map(|i| {
                    let mut m = current.row_data(i).unwrap();
                    if m.id == member_id.as_str() {
                        m.speaking = speaking;
                    }
                    m
                })
                .collect();
            let rc = Rc::new(slint::VecModel::from(members));
            ctx.app.set_members(rc.into());
            update_active_crew_card(&ctx.app);

            let current_channels = ctx.app.get_voice_channels();
            let mut changed = false;
            let updated_channels: Vec<VoiceChannelData> = (0..current_channels.row_count())
                .map(|i| {
                    let mut ch = current_channels.row_data(i).unwrap();
                    let ch_members: Vec<VoiceChannelMember> = (0..ch.members.row_count())
                        .map(|j| ch.members.row_data(j).unwrap())
                        .collect();
                    if ch_members.iter().any(|m| m.id == member_id.as_str()) {
                        let new_members: Vec<VoiceChannelMember> = ch_members
                            .into_iter()
                            .map(|mut m| {
                                if m.id == member_id.as_str() {
                                    m.speaking = speaking;
                                    changed = true;
                                }
                                m
                            })
                            .collect();
                        ch.members = Rc::new(slint::VecModel::from(new_members)).into();
                    }
                    ch
                })
                .collect();
            if changed {
                ctx.app
                    .set_voice_channels(Rc::new(slint::VecModel::from(updated_channels)).into());
            }
        }
        Event::MicPermissionChanged { granted, denied } => {
            log::info!(
                "UI: mic permission changed, granted={} denied={}",
                granted,
                denied
            );
            ctx.app.set_mic_permission_granted(granted);
            ctx.app.set_mic_permission_denied(denied);
        }
        Event::MicLevel { level } => {
            ctx.app.set_mic_level(level);
        }
        Event::AudioDebugStats {
            input_level,
            silero_vad_prob,
            rnnoise_prob,
            is_speaking,
            is_capturing,
            is_muted,
            is_deafened,
            echo_cancellation_enabled,
            agc_enabled,
            noise_suppression_enabled,
            packets_encoded,
            aec_capture_frames,
            aec_render_frames,
            incoming_streams,
            underrun_count,
            rtp_recv_total,
            pipeline_delay_ms,
            rtt_ms,
        } => {
            ctx.app.set_dbg_input_level(input_level);
            ctx.app.set_dbg_silero_prob(silero_vad_prob);
            ctx.app.set_dbg_rnnoise_prob(rnnoise_prob);
            ctx.app.set_dbg_speaking(is_speaking);
            ctx.app.set_dbg_capturing(is_capturing);
            ctx.app.set_dbg_muted(is_muted);
            ctx.app.set_dbg_deafened(is_deafened);
            ctx.app.set_dbg_aec_enabled(echo_cancellation_enabled);
            ctx.app.set_dbg_agc_enabled(agc_enabled);
            ctx.app.set_dbg_ns_enabled(noise_suppression_enabled);
            ctx.app.set_dbg_packets(packets_encoded as i32);
            ctx.app.set_dbg_aec_capture(aec_capture_frames as i32);
            ctx.app.set_dbg_aec_render(aec_render_frames as i32);
            ctx.app.set_dbg_incoming_streams(incoming_streams);
            ctx.app.set_dbg_underruns(underrun_count);
            ctx.app.set_dbg_rtp_recv(rtp_recv_total);
            ctx.app.set_dbg_delay_ms(pipeline_delay_ms);
            ctx.app.set_dbg_rtt_ms(rtt_ms);

            let mut hist = ctx.dbg_hist.borrow_mut();
            hist.push(input_level, is_speaking);
            set_level_history(&ctx.app, &hist);
        }
        Event::AudioDevicesListed { capture, playback } => {
            let cap: Vec<AudioDeviceData> = capture
                .iter()
                .map(|d| AudioDeviceData {
                    id: d.id.clone().into(),
                    name: d.name.clone().into(),
                    is_default: d.is_default,
                })
                .collect();
            let play: Vec<AudioDeviceData> = playback
                .iter()
                .map(|d| AudioDeviceData {
                    id: d.id.clone().into(),
                    name: d.name.clone().into(),
                    is_default: d.is_default,
                })
                .collect();
            ctx.app
                .set_capture_devices(Rc::new(slint::VecModel::from(cap)).into());
            ctx.app
                .set_playback_devices(Rc::new(slint::VecModel::from(play)).into());

            let s = ctx.settings.borrow();
            if let Some(ref saved_id) = s.capture_device_id {
                if let Some(dev) = capture.iter().find(|d| &d.id == saved_id) {
                    ctx.app.set_selected_capture_id(saved_id.as_str().into());
                    ctx.app.set_selected_capture_name(dev.name.as_str().into());
                }
            }
            if let Some(ref saved_id) = s.playback_device_id {
                if let Some(dev) = playback.iter().find(|d| &d.id == saved_id) {
                    ctx.app.set_selected_playback_id(saved_id.as_str().into());
                    ctx.app.set_selected_playback_name(dev.name.as_str().into());
                }
            }
        }
        Event::VoiceJoined {
            crew_id,
            channel_id,
            members: voice_members,
        } => {
            log::info!(
                "UI: voice joined channel={} in crew={} members={}",
                channel_id,
                crew_id,
                voice_members.len()
            );
            let prev_channel = ctx.active_voice_channel.borrow().clone();
            *ctx.active_voice_channel.borrow_mut() = channel_id.clone();
            let active_id = ctx.app.get_active_crew_id();
            if active_id == crew_id.as_str() {
                let current_channels = ctx.app.get_voice_channels();
                for i in 0..current_channels.row_count() {
                    let ch = current_channels.row_data(i).unwrap();
                    if ch.id == channel_id.as_str() {
                        ctx.app.set_voice_channel_name(ch.name.clone());
                        break;
                    }
                }
                let my_id = ctx.app.get_user_id();
                let current_channels = ctx.app.get_voice_channels();
                let updated_channels: Vec<VoiceChannelData> = (0..current_channels.row_count())
                    .map(|i| {
                        let mut ch = current_channels.row_data(i).unwrap();
                        let is_joined = ch.id == channel_id.as_str();
                        let was_active = ch.id.as_str() == prev_channel && !prev_channel.is_empty();
                        ch.active = is_joined;
                        if is_joined {
                            ch.expanded = true;
                            let uav = ctx.app.get_user_avatar();
                            let vctx = VoiceUiCtx {
                                local_user_id: &my_id,
                                user_avatar: &uav,
                                has_user_avatar: ctx.app.get_has_user_avatar(),
                                cache: &ctx.avatar_cache.borrow(),
                                local_muted: ctx.app.get_mic_muted(),
                                local_deafened: ctx.app.get_deafened(),
                            };
                            let ch_members = voice_members_to_ui(&voice_members, &vctx);
                            ch.member_count = ch_members.len() as i32;
                            ch.members = Rc::new(slint::VecModel::from(ch_members)).into();
                        } else {
                            let members: Vec<VoiceChannelMember> = (0..ch.members.row_count())
                                .filter_map(|j| {
                                    let m = ch.members.row_data(j).unwrap();
                                    if m.id == my_id {
                                        None
                                    } else {
                                        Some(m)
                                    }
                                })
                                .collect();
                            ch.member_count = members.len() as i32;
                            ch.members = Rc::new(slint::VecModel::from(members)).into();
                            if was_active {
                                ch.expanded = false;
                            }
                        }
                        ch
                    })
                    .collect();
                ctx.app
                    .set_voice_channels(Rc::new(slint::VecModel::from(updated_channels)).into());
            }
        }
        Event::VoiceUpdated {
            crew_id,
            channel_id,
            members: voice_members,
        } => {
            let active_id = ctx.app.get_active_crew_id();
            if active_id == crew_id.as_str() {
                for vm in &voice_members {
                    if vm.speaking.unwrap_or(false) {
                        log::info!("{} speaking=true", vm.username);
                    }
                }
                let current = ctx.app.get_members();
                let members: Vec<MemberData> = (0..current.row_count())
                    .map(|i| {
                        let mut m = current.row_data(i).unwrap();
                        if let Some(vm) =
                            voice_members.iter().find(|vm| vm.user_id == m.id.as_str())
                        {
                            m.speaking = vm.speaking.unwrap_or(false);
                        }
                        m
                    })
                    .collect();
                ctx.app
                    .set_members(Rc::new(slint::VecModel::from(members)).into());
                update_active_crew_card(&ctx.app);

                let local_id = ctx.app.get_user_id();
                let current_channels = ctx.app.get_voice_channels();
                let updated_channels: Vec<VoiceChannelData> = (0..current_channels.row_count())
                    .map(|i| {
                        let mut ch = current_channels.row_data(i).unwrap();
                        if ch.id == channel_id.as_str() {
                            let uav = ctx.app.get_user_avatar();
                            let vctx = VoiceUiCtx {
                                local_user_id: &local_id,
                                user_avatar: &uav,
                                has_user_avatar: ctx.app.get_has_user_avatar(),
                                cache: &ctx.avatar_cache.borrow(),
                                local_muted: ctx.app.get_mic_muted(),
                                local_deafened: ctx.app.get_deafened(),
                            };
                            let ch_members = voice_members_to_ui(&voice_members, &vctx);
                            ch.member_count = ch_members.len() as i32;
                            ch.members = Rc::new(slint::VecModel::from(ch_members)).into();
                        }
                        ch
                    })
                    .collect();
                ctx.app
                    .set_voice_channels(Rc::new(slint::VecModel::from(updated_channels)).into());
            }
        }
        Event::VoiceChannelsUpdated { crew_id, channels } => {
            log::debug!(
                "UI: voice channels updated crew={} count={}",
                crew_id,
                channels.len()
            );
            let active_id = ctx.app.get_active_crew_id();
            if active_id == crew_id.as_str() {
                let avc_id = ctx.active_voice_channel.borrow().clone();
                let local_id = ctx.app.get_user_id();
                let uav = ctx.app.get_user_avatar();
                let vctx = VoiceUiCtx {
                    local_user_id: &local_id,
                    user_avatar: &uav,
                    has_user_avatar: ctx.app.get_has_user_avatar(),
                    cache: &ctx.avatar_cache.borrow(),
                    local_muted: ctx.app.get_mic_muted(),
                    local_deafened: ctx.app.get_deafened(),
                };
                let vc_data = channels_to_ui(&channels, &avc_id, &vctx);
                ctx.app
                    .set_voice_channels(Rc::new(slint::VecModel::from(vc_data)).into());
            }
        }
        Event::VoiceChannelCreated { crew_id, channel } => {
            log::info!(
                "UI: voice channel created in crew={}: {}",
                crew_id,
                channel.name
            );
            let active_id = ctx.app.get_active_crew_id();
            if active_id == crew_id.as_str() {
                let current = ctx.app.get_voice_channels();
                let mut channels: Vec<VoiceChannelData> = (0..current.row_count())
                    .map(|i| current.row_data(i).unwrap())
                    .collect();
                let local_id = ctx.app.get_user_id();
                let uav = ctx.app.get_user_avatar();
                let vctx = VoiceUiCtx {
                    local_user_id: &local_id,
                    user_avatar: &uav,
                    has_user_avatar: ctx.app.get_has_user_avatar(),
                    cache: &ctx.avatar_cache.borrow(),
                    local_muted: ctx.app.get_mic_muted(),
                    local_deafened: ctx.app.get_deafened(),
                };
                channels.push(channel_to_ui(
                    &channel,
                    &ctx.active_voice_channel.borrow(),
                    &vctx,
                ));
                ctx.app
                    .set_voice_channels(Rc::new(slint::VecModel::from(channels)).into());
            }
        }
        Event::VoiceChannelRenamed {
            crew_id,
            channel_id,
            name,
        } => {
            log::info!(
                "UI: voice channel renamed in crew={}: {} -> {}",
                crew_id,
                channel_id,
                name
            );
            let active_id = ctx.app.get_active_crew_id();
            if active_id == crew_id.as_str() {
                if *ctx.active_voice_channel.borrow() == channel_id {
                    ctx.app.set_voice_channel_name(name.as_str().into());
                }
                let current = ctx.app.get_voice_channels();
                let updated: Vec<VoiceChannelData> = (0..current.row_count())
                    .map(|i| {
                        let mut ch = current.row_data(i).unwrap();
                        if ch.id == channel_id.as_str() {
                            ch.name = name.clone().into();
                        }
                        ch
                    })
                    .collect();
                ctx.app
                    .set_voice_channels(Rc::new(slint::VecModel::from(updated)).into());
            }
        }
        Event::VoiceChannelDeleted {
            crew_id,
            channel_id,
        } => {
            log::info!(
                "UI: voice channel deleted in crew={}: {}",
                crew_id,
                channel_id
            );
            let active_id = ctx.app.get_active_crew_id();
            if active_id == crew_id.as_str() {
                let current = ctx.app.get_voice_channels();
                let updated: Vec<VoiceChannelData> = (0..current.row_count())
                    .map(|i| current.row_data(i).unwrap())
                    .filter(|ch| ch.id != channel_id.as_str())
                    .collect();
                ctx.app
                    .set_voice_channels(Rc::new(slint::VecModel::from(updated)).into());
            }
        }
        Event::VoiceSfuDisconnected { crew_id, reason } => {
            log::warn!("SFU voice disconnected: crew={} reason={}", crew_id, reason);
        }
        Event::VoiceMembershipChanged { crew_id } => {
            log::debug!("SFU voice membership changed in crew {}", crew_id);
            let _ = ctx.cmd_tx.try_send(Command::SetActiveCrew { crew_id });
        }
        _ => {}
    }
}
