mod settings;

slint::include_modules!();

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use slint::Model;
use mello_core::{Client, Command, Config, Event};
use settings::Settings;

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    log::info!("Starting Mello...");

    let loopback = std::env::args().any(|a| a == "--loopback");

    let rt = tokio::runtime::Runtime::new()?;

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<Command>(256);
    let (event_tx, event_rx) = std::sync::mpsc::channel::<Event>();

    rt.spawn(async move {
        let mut client = Client::new(Config::default(), event_tx, loopback);
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

    let _ = cmd_tx.try_send(Command::TryRestore);

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
        let app_weak = app.as_weak();
        app.on_select_crew(move |crew_id| {
            let _ = cmd.try_send(Command::SelectCrew {
                crew_id: crew_id.to_string(),
            });
            // Also add/activate tab
            if let Some(app) = app_weak.upgrade() {
                update_crew_tab(&app, &crew_id);
            }
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
                app.set_crew_tabs(std::rc::Rc::new(slint::VecModel::from(Vec::<CrewTabData>::new())).into());
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

    // --- Crew tabs ---
    {
        let cmd = cmd_tx.clone();
        let app_weak = app.as_weak();
        app.on_tab_selected(move |tab_id| {
            let _ = cmd.try_send(Command::SelectCrew {
                crew_id: tab_id.to_string(),
            });
            if let Some(app) = app_weak.upgrade() {
                update_crew_tab(&app, &tab_id);
            }
        });
    }
    {
        let app_weak = app.as_weak();
        app.on_tab_closed(move |tab_id| {
            if let Some(app) = app_weak.upgrade() {
                remove_crew_tab(&app, &tab_id);
            }
        });
    }
    app.on_tab_add_clicked(|| {});

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

    // --- Presence ---
    app.on_presence_changed(move |status| {
        log::info!("Presence changed to {}", status);
    });

    // --- Event polling timer ---
    let app_weak = app.as_weak();
    let s = settings.clone();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(50), move || {
        while let Ok(event) = event_rx.try_recv() {
            if let Some(app) = app_weak.upgrade() {
                handle_event(&app, event, &s);
            }
        }
    });

    app.run()?;
    Ok(())
}

fn update_crew_tab(app: &MainWindow, crew_id: &str) {
    let current = app.get_crew_tabs();
    let mut tabs: Vec<CrewTabData> = (0..current.row_count())
        .map(|i| current.row_data(i).unwrap())
        .collect();

    let mut found = false;
    for tab in tabs.iter_mut() {
        let is_this = tab.id == crew_id;
        tab.active = is_this;
        if is_this { found = true; }
    }

    if !found {
        // Find crew name from crews list
        let crews = app.get_crews();
        let crew_name: slint::SharedString = (0..crews.row_count())
            .filter_map(|i| crews.row_data(i))
            .find(|c| c.id == crew_id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| crew_id.into());

        tabs.push(CrewTabData {
            id: crew_id.into(),
            name: crew_name,
            active: true,
        });
    }

    let rc = std::rc::Rc::new(slint::VecModel::from(tabs));
    app.set_crew_tabs(rc.into());
}

fn remove_crew_tab(app: &MainWindow, tab_id: &str) {
    let current = app.get_crew_tabs();
    let tabs: Vec<CrewTabData> = (0..current.row_count())
        .map(|i| current.row_data(i).unwrap())
        .filter(|t| t.id != tab_id)
        .collect();
    let rc = std::rc::Rc::new(slint::VecModel::from(tabs));
    app.set_crew_tabs(rc.into());
}

fn handle_event(app: &MainWindow, event: Event, settings: &Rc<RefCell<Settings>>) {
    match event {
        Event::Restoring => {
            app.set_login_loading(true);
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
            update_crew_tab(app, &crew_id);
            let empty: Vec<ChatMessageData> = vec![];
            let rc = std::rc::Rc::new(slint::VecModel::from(empty));
            app.set_messages(rc.into());
        }
        Event::CrewLeft { crew_id } => {
            log::info!("UI: left crew {}", crew_id);
            app.set_active_crew_id("".into());
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
        }
        Event::MemberLeft { member_id, .. } => {
            let current = app.get_members();
            let members: Vec<MemberData> = (0..current.row_count())
                .map(|i| current.row_data(i).unwrap())
                .filter(|m| m.id != member_id.as_str())
                .collect();
            let rc = std::rc::Rc::new(slint::VecModel::from(members));
            app.set_members(rc.into());
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
        }
        Event::MicLevel { level } => {
            app.set_mic_level(level);
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
