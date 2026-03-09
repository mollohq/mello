mod settings;

slint::include_modules!();

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use slint::Model;
use mello_core::{Client, Command, Config, Event};
use settings::Settings;
use uuid::Uuid;

fn nakama_config() -> Config {
    #[cfg(feature = "production")]
    return Config::production();

    #[cfg(not(feature = "production"))]
    Config::development()
}

fn make_initials(name: &str) -> String {
    let parts: Vec<&str> = name.split_whitespace().collect();
    match parts.len() {
        0 => "?".into(),
        1 => parts[0].chars().take(2).collect::<String>().to_uppercase(),
        _ => {
            let first = parts[0].chars().next().unwrap_or('?');
            let last = parts[parts.len() - 1].chars().next().unwrap_or('?');
            format!("{}{}", first, last).to_uppercase()
        }
    }
}

const HISTORY_LEN: usize = 30;

struct DebugHistory {
    levels: [f32; HISTORY_LEN],
    speaking: [bool; HISTORY_LEN],
    cursor: usize,
}

impl DebugHistory {
    fn new() -> Self {
        Self {
            levels: [0.0; HISTORY_LEN],
            speaking: [false; HISTORY_LEN],
            cursor: 0,
        }
    }

    fn push(&mut self, level: f32, spk: bool) {
        self.levels[self.cursor] = level;
        self.speaking[self.cursor] = spk;
        self.cursor = (self.cursor + 1) % HISTORY_LEN;
    }

    fn get(&self, i: usize) -> (f32, bool) {
        let idx = (self.cursor + i) % HISTORY_LEN;
        (self.levels[idx], self.speaking[idx])
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    log::info!("Starting Mello...");

    let loopback = std::env::args().any(|a| a == "--loopback");

    if std::env::args().any(|a| a == "--reset") {
        log::info!("--reset flag detected, wiping all settings");
        Settings::default().save();
    }

    let rt = tokio::runtime::Runtime::new()?;

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<Command>(256);
    let (event_tx, event_rx) = std::sync::mpsc::channel::<Event>();

    rt.spawn(async move {
        let mut client = Client::new(nakama_config(), event_tx, loopback);
        client.run(cmd_rx).await;
    });

    let app = MainWindow::new()?;

    let settings = Rc::new(RefCell::new(Settings::load()));

    app.global::<Theme>().set_dark(settings.borrow().dark_theme);

    // Apply saved audio device selections
    {
        let s = settings.borrow();
        if let Some(ref id) = s.capture_device_id {
            let _ = cmd_tx.try_send(Command::SetCaptureDevice { id: id.clone() });
        }
        if let Some(ref id) = s.playback_device_id {
            let _ = cmd_tx.try_send(Command::SetPlaybackDevice { id: id.clone() });
        }
    }

    // Decide startup path based on onboarding state
    {
        let mut s = settings.borrow_mut();
        if s.onboarding_step > 3 {
            // Onboarding complete: normal session restore
            let _ = cmd_tx.try_send(Command::TryRestore);
        } else {
            // Onboarding needed: device auth first
            let device_id = match s.device_id.clone() {
                Some(id) => id,
                None => {
                    let id = Uuid::new_v4().to_string();
                    s.device_id = Some(id.clone());
                    s.save();
                    id
                }
            };
            let _ = cmd_tx.try_send(Command::DeviceAuth { device_id });
        }
        app.set_onboarding_step(s.onboarding_step as i32);
    }

    // --- Login ---
    {
        let cmd = cmd_tx.clone();
        let app_weak = app.as_weak();
        app.on_login(move |email, password| {
            if let Some(app) = app_weak.upgrade() {
                app.set_login_loading(true);
                app.set_login_error("".into());
            }
            let _ = cmd.try_send(Command::Login {
                email: email.to_string(),
                password: password.to_string(),
            });
        });
    }

    // --- Crew selection ---
    {
        let cmd = cmd_tx.clone();
        app.on_select_crew(move |crew_id| {
            let _ = cmd.try_send(Command::SelectCrew {
                crew_id: crew_id.to_string(),
            });
        });
    }
    {
        let cmd = cmd_tx.clone();
        app.on_create_crew(move |name| {
            let _ = cmd.try_send(Command::CreateCrew {
                name: name.to_string(),
            });
        });
    }

    // --- Chat ---
    {
        let cmd = cmd_tx.clone();
        app.on_send_message(move |text| {
            let _ = cmd.try_send(Command::SendMessage {
                content: text.to_string(),
            });
        });
    }

    // --- Logout ---
    {
        let cmd = cmd_tx.clone();
        let app_weak = app.as_weak();
        app.on_logout(move || {
            let _ = cmd.try_send(Command::Logout);
            if let Some(app) = app_weak.upgrade() {
                app.set_logged_in(false);
                app.set_user_name("".into());
                app.set_user_tag("".into());
                app.set_active_crew_id("".into());
            }
        });
    }

    // --- Voice toggles ---
    {
        let cmd = cmd_tx.clone();
        app.on_voice_toggle(move || {
            let _ = cmd.try_send(Command::LeaveVoice);
        });
    }
    {
        let cmd = cmd_tx.clone();
        let app_weak = app.as_weak();
        app.on_mic_toggle(move || {
            if let Some(app) = app_weak.upgrade() {
                let _ = cmd.try_send(Command::SetMute {
                    muted: app.get_mic_muted(),
                });
            }
        });
    }
    {
        let cmd = cmd_tx.clone();
        let app_weak = app.as_weak();
        app.on_deafen_toggle(move || {
            if let Some(app) = app_weak.upgrade() {
                let _ = cmd.try_send(Command::SetDeafen {
                    deafened: app.get_deafened(),
                });
            }
        });
    }

    // --- Theme toggle ---
    {
        let app_weak = app.as_weak();
        let s = settings.clone();
        app.on_theme_toggled(move || {
            if let Some(app) = app_weak.upgrade() {
                let new_dark = !app.global::<Theme>().get_dark();
                app.global::<Theme>().set_dark(new_dark);
                let mut settings = s.borrow_mut();
                settings.dark_theme = new_dark;
                settings.save();
            }
        });
    }

    // --- Settings ---
    {
        let cmd = cmd_tx.clone();
        let app_weak = app.as_weak();
        app.on_settings_requested(move || {
            let _ = cmd.try_send(Command::ListAudioDevices);
            if let Some(app) = app_weak.upgrade() {
                app.set_settings_open(true);
            }
        });
    }
    {
        let cmd = cmd_tx.clone();
        let s = settings.clone();
        app.on_capture_device_selected(move |id| {
            let id_str = id.to_string();
            let _ = cmd.try_send(Command::SetCaptureDevice { id: id_str.clone() });
            let mut settings = s.borrow_mut();
            settings.capture_device_id = Some(id_str);
            settings.save();
        });
    }
    {
        let cmd = cmd_tx.clone();
        let s = settings.clone();
        app.on_playback_device_selected(move |id| {
            let id_str = id.to_string();
            let _ = cmd.try_send(Command::SetPlaybackDevice { id: id_str.clone() });
            let mut settings = s.borrow_mut();
            settings.playback_device_id = Some(id_str);
            settings.save();
        });
    }
    {
        let cmd = cmd_tx.clone();
        let app_weak = app.as_weak();
        app.on_mic_test_toggled(move || {
            if let Some(app) = app_weak.upgrade() {
                let enabled = app.get_mic_testing();
                let _ = cmd.try_send(Command::SetLoopback { enabled });
            }
        });
    }

    // --- Debug toggle ---
    {
        let cmd = cmd_tx.clone();
        let app_weak = app.as_weak();
        app.on_debug_toggled(move || {
            if let Some(app) = app_weak.upgrade() {
                let enabled = app.get_debug_open();
                let _ = cmd.try_send(Command::SetDebugMode { enabled });
            }
        });
    }

    // --- Onboarding: crew selected ---
    {
        let cmd = cmd_tx.clone();
        let app_weak = app.as_weak();
        let s = settings.clone();
        app.on_onboarding_crew_selected(move |crew_id| {
            let _ = cmd.try_send(Command::JoinCrew {
                crew_id: crew_id.to_string(),
            });
            let _ = cmd.try_send(Command::ListAudioDevices);
            if let Some(app) = app_weak.upgrade() {
                app.set_onboarding_step(2);
                let mut settings = s.borrow_mut();
                settings.onboarding_step = 2;
                settings.save();
            }
        });
    }
    // --- Onboarding: create crew ---
    {
        let cmd = cmd_tx.clone();
        let app_weak = app.as_weak();
        let s = settings.clone();
        app.on_onboarding_create_crew(move |name| {
            let _ = cmd.try_send(Command::CreateCrew {
                name: name.to_string(),
            });
            let _ = cmd.try_send(Command::ListAudioDevices);
            if let Some(app) = app_weak.upgrade() {
                app.set_onboarding_step(2);
                let mut settings = s.borrow_mut();
                settings.onboarding_step = 2;
                settings.save();
            }
        });
    }
    // --- Onboarding: continue to step ---
    {
        let app_weak = app.as_weak();
        let s = settings.clone();
        app.on_onboarding_continue(move |step| {
            if let Some(app) = app_weak.upgrade() {
                app.set_onboarding_step(step);
                let mut settings = s.borrow_mut();
                settings.onboarding_step = step as u8;
                settings.save();
            }
            
        });
    }
    // --- Onboarding: link email ---
    {
        let cmd = cmd_tx.clone();
        app.on_onboarding_link_email(move |email, password| {
            let _ = cmd.try_send(Command::LinkEmail {
                email: email.to_string(),
                password: password.to_string(),
            });
        });
    }
    // --- Onboarding: skip identity ---
    {
        let app_weak = app.as_weak();
        let s = settings.clone();
        app.on_onboarding_skip_identity(move || {
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
        let cmd = cmd_tx.clone();
        let s = settings.clone();
        app.on_onboarding_capture_device_selected(move |id| {
            let id_str = id.to_string();
            let _ = cmd.try_send(Command::SetCaptureDevice { id: id_str.clone() });
            let mut settings = s.borrow_mut();
            settings.capture_device_id = Some(id_str);
            settings.save();
        });
    }
    {
        let cmd = cmd_tx.clone();
        let s = settings.clone();
        app.on_onboarding_playback_device_selected(move |id| {
            let id_str = id.to_string();
            let _ = cmd.try_send(Command::SetPlaybackDevice { id: id_str.clone() });
            let mut settings = s.borrow_mut();
            settings.playback_device_id = Some(id_str);
            settings.save();
        });
    }

    // --- Presence ---
    app.on_presence_changed(move |status| {
        log::info!("Presence changed to {}", status);
    });

    // --- Event polling timer ---
    let app_weak = app.as_weak();
    let s = settings.clone();
    let dbg_hist = Rc::new(RefCell::new(DebugHistory::new()));
    let event_cmd_tx = cmd_tx.clone();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(50), move || {
        while let Ok(event) = event_rx.try_recv() {
            if let Some(app) = app_weak.upgrade() {
                handle_event(&app, event, &s, &dbg_hist, &event_cmd_tx);
            }
        }
    });

    app.run()?;
    Ok(())
}

fn update_active_crew_card(app: &MainWindow) {
    let active_id = app.get_active_crew_id();
    if active_id.is_empty() { return; }

    let members = app.get_members();
    let online_members: Vec<MemberData> = (0..members.row_count())
        .filter_map(|i| members.row_data(i))
        .filter(|m| m.online)
        .collect();

    let online_count = online_members.len().max(1) as i32;
    let voice_count = online_members.len().min(4) as i32;

    let crews = app.get_crews();
    let updated: Vec<CrewData> = (0..crews.row_count())
        .map(|i| {
            let mut c = crews.row_data(i).unwrap();
            if c.id == active_id {
                c.online_count = online_count;
                c.voice_count = voice_count;

                if let Some(m) = online_members.get(0) {
                    c.v0_initials = m.initials.clone();
                    c.v0_name = m.name.clone();
                    c.v0_speaking = m.speaking;
                }
                if let Some(m) = online_members.get(1) {
                    c.v1_initials = m.initials.clone();
                    c.v1_name = m.name.clone();
                    c.v1_speaking = m.speaking;
                }
                if let Some(m) = online_members.get(2) {
                    c.v2_initials = m.initials.clone();
                    c.v2_name = m.name.clone();
                    c.v2_speaking = m.speaking;
                }
                if let Some(m) = online_members.get(3) {
                    c.v3_initials = m.initials.clone();
                    c.v3_name = m.name.clone();
                    c.v3_speaking = m.speaking;
                }
            }
            c
        })
        .collect();
    app.set_crews(Rc::new(slint::VecModel::from(updated)).into());
}

fn set_level_history(app: &MainWindow, hist: &DebugHistory) {
    macro_rules! set_lh {
        ($($i:literal),*) => {
            $(
                let (level, spk) = hist.get($i);
                paste::paste! {
                    app.[<set_lh $i>](level);
                    app.[<set_sh $i>](spk);
                }
            )*
        };
    }
    set_lh!(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29);
}

fn handle_event(app: &MainWindow, event: Event, settings: &Rc<RefCell<Settings>>, dbg_hist: &Rc<RefCell<DebugHistory>>, cmd_tx: &tokio::sync::mpsc::Sender<Command>) {
    match event {
        Event::Restoring => {
            app.set_login_loading(true);
        }
        Event::DeviceAuthed { user } => {
            log::info!("UI: device authed as {}", user.id);
            app.set_user_name(user.display_name.into());
            app.set_user_tag(user.tag.into());
            let step = app.get_onboarding_step();
            if step == 0 || step == 1 {
                app.set_onboarding_step(1);
                let _ = cmd_tx.try_send(Command::DiscoverCrews);
            }
        }
        Event::DiscoverCrewsLoaded { crews } => {
            let model: Vec<CrewData> = crews.into_iter().map(|c| CrewData {
                id: c.id.clone().into(),
                name: c.name.into(),
                member_count: c.member_count,
                online_count: 0,
                ..Default::default()
            }).collect();
            let rc = Rc::new(slint::VecModel::from(model));
            app.set_discover_crews(rc.into());
        }
        Event::EmailLinked => {
            log::info!("UI: email linked successfully");
            app.set_onboarding_step(4);
            app.set_logged_in(true);
            let mut s = settings.borrow_mut();
            s.onboarding_step = 4;
            s.save();
        }
        Event::EmailLinkFailed { reason } => {
            log::warn!("UI: email link failed: {}", reason);
            app.set_link_error(reason.into());
        }
        Event::LoggedIn { user } => {
            log::info!("UI: logged in as {}", user.display_name);
            app.set_logged_in(true);
            app.set_login_loading(false);
            app.set_user_name(user.display_name.into());
            app.set_user_tag(user.tag.into());
        }
        Event::LoginFailed { reason } => {
            log::warn!("UI: login failed: {}", reason);
            app.set_login_loading(false);
            app.set_login_error(reason.into());
        }
        Event::CrewsLoaded { crews } => {
            let model: Vec<CrewData> = crews.into_iter().map(|c| CrewData {
                id: c.id.clone().into(),
                name: c.name.into(),
                member_count: c.member_count,
                online_count: 0,
                ..Default::default()
            }).collect();
            let rc = std::rc::Rc::new(slint::VecModel::from(model));
            app.set_crews(rc.into());
        }
        Event::CrewCreated { crew } => {
            log::info!("UI: crew created: {}", crew.name);
        }
        Event::CrewCreateFailed { reason } => {
            log::warn!("UI: crew creation failed: {}", reason);
        }
        Event::CrewJoined { crew_id } => {
            log::info!("UI: joined crew {}", crew_id);
            app.set_active_crew_id(crew_id.clone().into());
            let empty: Vec<ChatMessageData> = vec![];
            let rc = std::rc::Rc::new(slint::VecModel::from(empty));
            app.set_messages(rc.into());
            update_active_crew_card(app);
        }
        Event::CrewLeft { crew_id } => {
            log::info!("UI: left crew {}", crew_id);
            // Reset online count for the crew we're leaving
            let crews = app.get_crews();
            let updated: Vec<CrewData> = (0..crews.row_count())
                .map(|i| {
                    let mut c = crews.row_data(i).unwrap();
                    if c.id == crew_id.as_str() {
                        c.online_count = 0;
                    }
                    c
                })
                .collect();
            app.set_crews(Rc::new(slint::VecModel::from(updated)).into());
            app.set_active_crew_id("".into());
        }
        Event::MessagesLoaded { messages } => {
            let msgs: Vec<ChatMessageData> = messages.into_iter().map(|m| {
                ChatMessageData {
                    sender_name: m.sender_name.into(),
                    text: m.content.into(),
                    timestamp: m.timestamp.into(),
                }
            }).collect();
            let rc = std::rc::Rc::new(slint::VecModel::from(msgs));
            app.set_messages(rc.into());
        }
        Event::MessageReceived { message } => {
            let current = app.get_messages();
            let new_msg = ChatMessageData {
                sender_name: message.sender_name.into(),
                text: message.content.into(),
                timestamp: message.timestamp.into(),
            };
            let mut msgs: Vec<ChatMessageData> = (0..current.row_count())
                .map(|i| current.row_data(i).unwrap())
                .collect();
            msgs.push(new_msg);
            let rc = std::rc::Rc::new(slint::VecModel::from(msgs));
            app.set_messages(rc.into());
        }
        Event::MemberJoined { member, .. } => {
            let current = app.get_members();
            let initials = make_initials(&member.display_name);
            let new_member = MemberData {
                id: member.id.into(),
                name: member.display_name.into(),
                initials: initials.into(),
                online: true,
                speaking: false,
            };
            let mut members: Vec<MemberData> = (0..current.row_count())
                .map(|i| current.row_data(i).unwrap())
                .collect();
            if !members.iter().any(|m| m.id == new_member.id) {
                members.push(new_member);
            }
            let rc = std::rc::Rc::new(slint::VecModel::from(members));
            app.set_members(rc.into());
            update_active_crew_card(app);
        }
        Event::MemberLeft { member_id, .. } => {
            let current = app.get_members();
            let members: Vec<MemberData> = (0..current.row_count())
                .map(|i| current.row_data(i).unwrap())
                .filter(|m| m.id != member_id.as_str())
                .collect();
            let rc = std::rc::Rc::new(slint::VecModel::from(members));
            app.set_members(rc.into());
            update_active_crew_card(app);
        }
        Event::PresenceUpdated { user_id, online } => {
            let current = app.get_members();
            let members: Vec<MemberData> = (0..current.row_count())
                .map(|i| {
                    let mut m = current.row_data(i).unwrap();
                    if m.id == user_id.as_str() {
                        m.online = online;
                    }
                    m
                })
                .collect();
            let rc = std::rc::Rc::new(slint::VecModel::from(members));
            app.set_members(rc.into());
        }
        Event::VoiceStateChanged { in_call } => {
            app.set_in_voice(in_call);
            log::info!("UI: voice state changed, in_call={}", in_call);
        }
        Event::VoiceConnected { peer_id } => {
            log::info!("UI: voice connected to {}", peer_id);
        }
        Event::VoiceDisconnected { peer_id } => {
            log::info!("UI: voice disconnected from {}", peer_id);
        }
        Event::VoiceActivity { member_id, speaking } => {
            let current = app.get_members();
            let members: Vec<MemberData> = (0..current.row_count())
                .map(|i| {
                    let mut m = current.row_data(i).unwrap();
                    if m.id == member_id.as_str() {
                        m.speaking = speaking;
                    }
                    m
                })
                .collect();
            let rc = std::rc::Rc::new(slint::VecModel::from(members));
            app.set_members(rc.into());
            update_active_crew_card(app);
        }
        Event::MicLevel { level } => {
            app.set_mic_level(level);
        }
        Event::AudioDebugStats {
            input_level, silero_vad_prob, rnnoise_prob,
            is_speaking, is_capturing, is_muted, is_deafened, packets_encoded,
        } => {
            app.set_dbg_input_level(input_level);
            app.set_dbg_silero_prob(silero_vad_prob);
            app.set_dbg_rnnoise_prob(rnnoise_prob);
            app.set_dbg_speaking(is_speaking);
            app.set_dbg_capturing(is_capturing);
            app.set_dbg_muted(is_muted);
            app.set_dbg_deafened(is_deafened);
            app.set_dbg_packets(packets_encoded as i32);

            let mut hist = dbg_hist.borrow_mut();
            hist.push(input_level, is_speaking);
            set_level_history(app, &hist);
        }
        Event::AudioDevicesListed { capture, playback } => {
            let cap: Vec<AudioDeviceData> = capture.iter().map(|d| AudioDeviceData {
                id: d.id.clone().into(),
                name: d.name.clone().into(),
                is_default: d.is_default,
            }).collect();
            let play: Vec<AudioDeviceData> = playback.iter().map(|d| AudioDeviceData {
                id: d.id.clone().into(),
                name: d.name.clone().into(),
                is_default: d.is_default,
            }).collect();
            app.set_capture_devices(Rc::new(slint::VecModel::from(cap)).into());
            app.set_playback_devices(Rc::new(slint::VecModel::from(play)).into());

            let s = settings.borrow();
            if let Some(ref saved_id) = s.capture_device_id {
                if let Some(dev) = capture.iter().find(|d| &d.id == saved_id) {
                    app.set_selected_capture_id(saved_id.as_str().into());
                    app.set_selected_capture_name(dev.name.as_str().into());
                }
            }
            if let Some(ref saved_id) = s.playback_device_id {
                if let Some(dev) = playback.iter().find(|d| &d.id == saved_id) {
                    app.set_selected_playback_id(saved_id.as_str().into());
                    app.set_selected_playback_name(dev.name.as_str().into());
                }
            }
        }
        Event::SignalReceived { .. } => {
            // Handled internally by the client, not the UI
        }
        Event::Error { message } => {
            log::error!("UI: error: {}", message);
        }
    }
}
