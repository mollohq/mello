mod autolaunch;
mod deep_link;
mod notifications;
mod platform;
mod settings;

slint::include_modules!();

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;
use slint::Model;
use mello_core::{Client, Command, Config, Event};
use settings::Settings;
use platform::{StatusItem, VoiceState};
use uuid::Uuid;
use single_instance::SingleInstance;

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

    // --- Single instance enforcement ---
    let _instance = SingleInstance::new("app.mello.desktop")?;
    if !_instance.is_single() {
        eprintln!("Mello is already running.");
        std::process::exit(0);
    }

    // --- Deep link from argv ---
    if let Some(url) = deep_link::extract_deep_link() {
        if let Some(link) = deep_link::parse(&url) {
            log::info!("Deep link: {:?}", link);
            // TODO: route deep link to running instance or handle at startup
        }
    }

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

    // --- macOS: disable Slint's default menu bar so we can install our own ---
    #[cfg(target_os = "macos")]
    {
        let backend = i_slint_backend_winit::Backend::builder()
            .with_default_menu_bar(false)
            .build()?;
        slint::platform::set_platform(Box::new(backend))?;
    }

    let app = MainWindow::new()?;

    // --- macOS native menu bar ---
    #[cfg(target_os = "macos")]
    let _menu_bar = {
        let menu = platform::macos::build_menu_bar();
        menu.init_for_nsapp();
        menu // keep alive
    };

    // --- Tray / status item ---
    let status_item = Rc::new(RefCell::new(
        StatusItem::new().expect("failed to create tray icon"),
    ));

    // --- Global hotkey manager ---
    let _hotkey_mgr = Rc::new(RefCell::new(
        platform::hotkeys::HotkeyManager::new().expect("failed to init hotkey manager"),
    ));

    // --- Close → tray ---
    {
        let window_ref = app.as_weak();
        app.window().on_close_requested(move || {
            if let Some(w) = window_ref.upgrade() {
                w.hide().ok();
            }
            slint::CloseRequestResponse::KeepWindowShown
        });
    }

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
        log::info!("[auth] startup  onboarding_step={} device_id={:?}", s.onboarding_step, s.device_id);
        if s.onboarding_step > 3 {
            // Onboarding complete: normal session restore
            log::info!("[auth] onboarding done — attempting session restore");
            let _ = cmd_tx.try_send(Command::TryRestore);
        } else {
            // Onboarding needed: device auth first
            let device_id = match s.device_id.clone() {
                Some(id) => id,
                None => {
                    let id = Uuid::new_v4().to_string();
                    s.device_id = Some(id.clone());
                    s.save();
                    log::info!("[auth] generated new device_id={}", id);
                    id
                }
            };
            log::info!("[auth] onboarding in progress — device auth with id={}", device_id);
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
        let settings_ref = settings.clone();
        app.on_logout(move || {
            let _ = cmd.try_send(Command::Logout);
            if let Some(app) = app_weak.upgrade() {
                app.set_logged_in(false);
                app.set_user_name("".into());
                app.set_user_tag("".into());
                app.set_active_crew_id("".into());
                app.set_onboarding_step(1);
            }
            // Reset persisted onboarding state and re-trigger device auth
            let mut s = settings_ref.borrow_mut();
            s.onboarding_step = 1;
            s.save();
            log::info!("Logged out — returning to onboarding step 1");
            if let Some(ref device_id) = s.device_id {
                let _ = cmd.try_send(Command::DeviceAuth { device_id: device_id.clone() });
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
    // Track whether the user was manually muted before deafening,
    // so un-deafen can restore the prior mute state.
    let muted_before_deafen = Rc::new(Cell::new(false));
    {
        let cmd = cmd_tx.clone();
        let app_weak = app.as_weak();
        app.on_mic_toggle(move || {
            if let Some(app) = app_weak.upgrade() {
                let new_muted = !app.get_mic_muted();
                app.set_mic_muted(new_muted);
                let _ = cmd.try_send(Command::SetMute { muted: new_muted });
            }
        });
    }
    {
        let cmd = cmd_tx.clone();
        let app_weak = app.as_weak();
        let mbd = muted_before_deafen.clone();
        app.on_deafen_toggle(move || {
            if let Some(app) = app_weak.upgrade() {
                let new_deafened = !app.get_deafened();
                app.set_deafened(new_deafened);
                let _ = cmd.try_send(Command::SetDeafen { deafened: new_deafened });

                if new_deafened {
                    // Remember current mute state, then force mute
                    mbd.set(app.get_mic_muted());
                    if !app.get_mic_muted() {
                        app.set_mic_muted(true);
                        let _ = cmd.try_send(Command::SetMute { muted: true });
                    }
                } else {
                    // Restore: only unmute if user wasn't manually muted before
                    if !mbd.get() {
                        app.set_mic_muted(false);
                        let _ = cmd.try_send(Command::SetMute { muted: false });
                    }
                }
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
        let cmd = cmd_tx.clone();
        app.on_onboarding_continue(move |step| {
            if let Some(app) = app_weak.upgrade() {
                // When advancing to step 3, persist the nickname to Nakama
                if step == 3 {
                    let nickname = app.get_onboarding_nickname().to_string();
                    if !nickname.is_empty() {
                        log::info!("[auth] persisting display_name to Nakama: {}", nickname);
                        let _ = cmd.try_send(Command::UpdateProfile {
                            display_name: nickname.clone(),
                        });
                        app.set_user_name(nickname.into());
                    }
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
        let app_weak = app.as_weak();
        let s = settings.clone();
        let cmd = cmd_tx.clone();
        app.on_onboarding_login_requested(move || {
            if let Some(app) = app_weak.upgrade() {
                log::info!("[auth] sign-in pill — entering app as device user");
                app.set_logged_in(true);
                app.set_onboarding_step(4);
                let mut settings = s.borrow_mut();
                settings.onboarding_step = 4;
                settings.save();
                // Load user's crews so the sidebar populates and auto-select kicks in
                let _ = cmd.try_send(Command::LoadMyCrews);
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
    let status_ref = status_item.clone();
    let hotkey_ref = _hotkey_mgr.clone();
    let hotkey_cmd_tx = cmd_tx.clone();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(50), move || {
        // --- Core events ---
        while let Ok(event) = event_rx.try_recv() {
            if let Some(app) = app_weak.upgrade() {
                // Update tray icon based on voice state changes
                match &event {
                    Event::VoiceStateChanged { in_call } => {
                        let state = if *in_call {
                            VoiceState::Connected
                        } else {
                            VoiceState::Inactive
                        };
                        status_ref.borrow_mut().set_voice_state(state);
                    }
                    Event::VoiceActivity { speaking, .. } => {
                        if app.get_mic_muted() {
                            status_ref.borrow_mut().set_voice_state(VoiceState::Muted);
                        } else if *speaking {
                            status_ref.borrow_mut().set_voice_state(VoiceState::Speaking);
                        } else {
                            status_ref.borrow_mut().set_voice_state(VoiceState::Connected);
                        }
                    }
                    // Show OS notifications when window is hidden
                    Event::MemberJoined { member, .. } => {
                        if !app.window().is_visible() {
                            notifications::notify_member_joined(&member.display_name);
                        }
                    }
                    Event::MessageReceived { message } => {
                        if !app.window().is_visible() {
                            let crew_name = app.get_active_crew_id().to_string();
                            notifications::notify_message(
                                &crew_name,
                                &message.sender_name,
                                &message.content,
                            );
                        }
                    }
                    _ => {}
                }
                handle_event(&app, event, &s, &dbg_hist, &event_cmd_tx);
            }
        }

        // --- Tray icon click: toggle window visibility ---
        while let Some(event) = StatusItem::poll_tray_event() {
            if let tray_icon::TrayIconEvent::Click { .. } = event {
                if let Some(app) = app_weak.upgrade() {
                    if app.window().is_visible() {
                        app.hide().ok();
                    } else {
                        app.show().ok();
                    }
                }
            }
        }

        // --- Global hotkey events (PTT) ---
        while let Some(event) = platform::hotkeys::HotkeyManager::poll() {
            let mgr = hotkey_ref.borrow();
            if let Some(ptt_id) = mgr.ptt_id() {
                if event.id == ptt_id {
                    let pressed = event.state == global_hotkey::HotKeyState::Pressed;
                    // PTT: pressed = unmute, released = mute
                    let _ = hotkey_cmd_tx.try_send(Command::SetMute { muted: !pressed });
                }
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
            log::info!("[auth] restoring session…");
            app.set_login_loading(true);
        }
        Event::DeviceAuthed { user, created } => {
            log::info!("[auth] device-authed  user_id={} name={} tag={} created={}", user.id, user.display_name, user.tag, created);
            app.set_user_name(user.display_name.into());
            app.set_user_tag(user.tag.into());
            app.set_is_returning_user(!created);
            let step = app.get_onboarding_step();
            log::info!("[auth] onboarding_step={} is_returning_user={}", step, !created);
            if step == 0 || step == 1 {
                app.set_onboarding_step(1);
                let _ = cmd_tx.try_send(Command::DiscoverCrews);
            }
        }
        Event::DiscoverCrewsLoaded { crews } => {
            log::info!("[auth] discover-crews loaded  count={}", crews.len());
            let model: Vec<CrewData> = crews.into_iter().map(|c| CrewData {
                id: c.id.clone().into(),
                name: c.name.into(),
                description: c.description.into(),
                member_count: c.member_count,
                online_count: 0,
                ..Default::default()
            }).collect();
            let rc = Rc::new(slint::VecModel::from(model));
            app.set_discover_crews(rc.into());
        }
        Event::EmailLinked => {
            log::info!("[auth] email linked — onboarding complete");
            app.set_onboarding_step(4);
            app.set_logged_in(true);
            let mut s = settings.borrow_mut();
            s.onboarding_step = 4;
            s.save();
        }
        Event::EmailLinkFailed { reason } => {
            log::warn!("[auth] email-link-failed  reason={}", reason);
            app.set_link_error(reason.into());
        }
        Event::LoggedIn { user } => {
            log::info!("[auth] logged-in  user_id={} name={} tag={}", user.id, user.display_name, user.tag);
            app.set_logged_in(true);
            app.set_login_loading(false);
            app.set_user_name(user.display_name.into());
            app.set_user_tag(user.tag.into());
            // Persist onboarding as done so next startup skips it
            let mut s = settings.borrow_mut();
            if s.onboarding_step < 4 {
                s.onboarding_step = 4;
                s.save();
            }
        }
        Event::LoginFailed { reason } => {
            log::warn!("[auth] login-failed  reason={}", reason);
            app.set_login_loading(false);
            app.set_logged_in(false);
            app.set_login_error(reason.clone().into());

            // If restore failed (empty reason = expired token), fall back to
            // device auth so the user sees the login/onboarding screen.
            if reason.is_empty() {
                log::info!("[auth] restore failed — falling back to device auth");
                app.set_onboarding_step(1);
                let mut s = settings.borrow_mut();
                s.onboarding_step = 1;
                s.save();
                if let Some(ref device_id) = s.device_id {
                    let _ = cmd_tx.try_send(Command::DeviceAuth { device_id: device_id.clone() });
                }
            }
        }
        Event::CrewsLoaded { crews } => {
            let crew_ids: Vec<String> = crews.iter().map(|c| c.id.clone()).collect();

            // Merge: preserve any sidebar data that arrived before this event
            let current = app.get_crews();
            let mut existing: std::collections::HashMap<String, CrewData> = (0..current.row_count())
                .filter_map(|i| current.row_data(i))
                .map(|c| (c.id.to_string(), c))
                .collect();

            let model: Vec<CrewData> = crews.into_iter().map(|c| {
                if let Some(mut prev) = existing.remove(&c.id) {
                    // Keep sidebar data, update authoritative fields from CrewsLoaded
                    prev.name = c.name.into();
                    prev.description = c.description.into();
                    prev.member_count = c.member_count;
                    prev
                } else {
                    CrewData {
                        id: c.id.clone().into(),
                        name: c.name.into(),
                        description: c.description.into(),
                        member_count: c.member_count,
                        online_count: 0,
                        ..Default::default()
                    }
                }
            }).collect();
            let rc = std::rc::Rc::new(slint::VecModel::from(model));
            app.set_crews(rc.into());
            // Auto-select last active crew (or first if not found)
            if app.get_active_crew_id().is_empty() {
                let last = settings.borrow().last_crew_id.clone();
                let target = match &last {
                    Some(id) if crew_ids.contains(id) => {
                        log::info!("[auth] restoring last crew: {}", id);
                        Some(id.clone())
                    }
                    _ => {
                        crew_ids.first().map(|id| {
                            log::info!("[auth] auto-selecting first crew: {}", id);
                            id.clone()
                        })
                    }
                };
                if let Some(id) = target {
                    let _ = cmd_tx.try_send(Command::SelectCrew { crew_id: id });
                }
            }
        }
        Event::CrewCreated { crew } => {
            log::info!("UI: crew created: {}", crew.name);
        }
        Event::CrewCreateFailed { reason } => {
            log::warn!("UI: crew creation failed: {}", reason);
        }
        Event::CrewJoined { crew_id } => {
            log::info!("UI: joined crew {}", crew_id);
            // Clear voice bubbles on the previous crew — we left its channel
            // so we no longer receive presence updates for it. Keep member_count
            // (static from API) but zero out voice_count & speaking flags since
            // that data is now stale.
            let old_id = app.get_active_crew_id();
            if !old_id.is_empty() && old_id != crew_id.as_str() {
                let crews = app.get_crews();
                let cleared: Vec<CrewData> = (0..crews.row_count())
                    .map(|i| {
                        let mut c = crews.row_data(i).unwrap();
                        if c.id == old_id {
                            c.voice_count = 0;
                            c.v0_speaking = false;
                            c.v1_speaking = false;
                            c.v2_speaking = false;
                            c.v3_speaking = false;
                            // keep online_count & member_count — static data
                        }
                        c
                    })
                    .collect();
                app.set_crews(Rc::new(slint::VecModel::from(cleared)).into());
            }
            app.set_active_crew_id(crew_id.clone().into());
            let empty: Vec<ChatMessageData> = vec![];
            let rc = std::rc::Rc::new(slint::VecModel::from(empty));
            app.set_messages(rc.into());
            update_active_crew_card(app);
            // Persist last active crew
            let mut s = settings.borrow_mut();
            s.last_crew_id = Some(crew_id);
            s.save();
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

        // --- Presence & crew state events ---

        Event::CrewStateLoaded { state } => {
            log::info!("UI: crew state loaded for {} (online={}, total={})",
                state.crew_id, state.counts.online, state.counts.total);

            // Update the active crew card's online/voice/stream/message data
            let crews = app.get_crews();
            let updated: Vec<CrewData> = (0..crews.row_count())
                .map(|i| {
                    let mut c = crews.row_data(i).unwrap();
                    if c.id == state.crew_id.as_str() {
                        c.online_count = state.counts.online as i32;
                        let vlen = state.voice.members.len().min(4);
                        c.voice_count = vlen as i32;
                        // Populate voice chips from authoritative state
                        if let Some(m) = state.voice.members.get(0) {
                            c.v0_name = m.username.clone().into();
                            c.v0_initials = make_initials(&m.username).into();
                            c.v0_speaking = m.speaking.unwrap_or(false);
                        }
                        if let Some(m) = state.voice.members.get(1) {
                            c.v1_name = m.username.clone().into();
                            c.v1_initials = make_initials(&m.username).into();
                            c.v1_speaking = m.speaking.unwrap_or(false);
                        }
                        if let Some(m) = state.voice.members.get(2) {
                            c.v2_name = m.username.clone().into();
                            c.v2_initials = make_initials(&m.username).into();
                            c.v2_speaking = m.speaking.unwrap_or(false);
                        }
                        if let Some(m) = state.voice.members.get(3) {
                            c.v3_name = m.username.clone().into();
                            c.v3_initials = make_initials(&m.username).into();
                            c.v3_speaking = m.speaking.unwrap_or(false);
                        }
                        // Stream
                        if let Some(ref stream) = state.stream {
                            c.has_stream = stream.active;
                            c.stream_name = stream.title.clone().unwrap_or_default().into();
                        }
                        // Recent messages
                        c.msg_count = state.recent_messages.len().min(2) as i32;
                        if let Some(m) = state.recent_messages.get(0) {
                            c.m0_author = m.username.clone().into();
                            c.m0_text = m.preview.clone().into();
                        }
                        if let Some(m) = state.recent_messages.get(1) {
                            c.m1_author = m.username.clone().into();
                            c.m1_text = m.preview.clone().into();
                        }
                    }
                    c
                })
                .collect();
            app.set_crews(Rc::new(slint::VecModel::from(updated)).into());
        }
        Event::SidebarUpdated { crews: sidebar_crews } => {
            log::info!("UI: sidebar updated for {} crews", sidebar_crews.len());

            let current = app.get_crews();
            let mut updated: Vec<CrewData> = (0..current.row_count())
                .map(|i| current.row_data(i).unwrap())
                .collect();

            for sc in &sidebar_crews {
                let c = if let Some(c) = updated.iter_mut().find(|c| c.id == sc.crew_id.as_str()) {
                    c
                } else {
                    // Crew not yet in model (SidebarUpdated arrived before CrewsLoaded) — create stub
                    updated.push(CrewData {
                        id: sc.crew_id.clone().into(),
                        name: sc.name.clone().into(),
                        member_count: sc.counts.total as i32,
                        ..Default::default()
                    });
                    updated.last_mut().unwrap()
                };

                c.online_count = sc.counts.online as i32;
                if let Some(ref voice) = sc.voice {
                    let vlen = voice.members.len().min(4);
                    c.voice_count = vlen as i32;
                    if let Some(m) = voice.members.get(0) {
                        c.v0_name = m.username.clone().into();
                        c.v0_initials = make_initials(&m.username).into();
                    }
                    if let Some(m) = voice.members.get(1) {
                        c.v1_name = m.username.clone().into();
                        c.v1_initials = make_initials(&m.username).into();
                    }
                    if let Some(m) = voice.members.get(2) {
                        c.v2_name = m.username.clone().into();
                        c.v2_initials = make_initials(&m.username).into();
                    }
                    if let Some(m) = voice.members.get(3) {
                        c.v3_name = m.username.clone().into();
                        c.v3_initials = make_initials(&m.username).into();
                    }
                }
                // Stream
                if let Some(ref stream) = sc.stream {
                    c.has_stream = stream.active;
                    c.stream_name = stream.title.clone().unwrap_or_default().into();
                }
                // Recent messages
                c.msg_count = sc.recent_messages.len().min(2) as i32;
                if let Some(m) = sc.recent_messages.get(0) {
                    c.m0_author = m.username.clone().into();
                    c.m0_text = m.preview.clone().into();
                }
                if let Some(m) = sc.recent_messages.get(1) {
                    c.m1_author = m.username.clone().into();
                    c.m1_text = m.preview.clone().into();
                }
            }
            app.set_crews(Rc::new(slint::VecModel::from(updated)).into());
        }
        Event::CrewEventReceived { event } => {
            log::info!("UI: crew event {} in crew {}", event.event, event.crew_id);
            // Priority events like stream_started, voice_joined — refresh sidebar counts
            let _ = cmd_tx.try_send(Command::SetActiveCrew {
                crew_id: event.crew_id,
            });
        }
        Event::PresenceChanged { change } => {
            log::debug!("UI: presence change user={} in crew={}", change.user_id, change.crew_id);
            let active_id = app.get_active_crew_id();
            if active_id == change.crew_id.as_str() {
                let current = app.get_members();
                let is_online = change.presence.status != mello_core::presence::PresenceStatus::Offline;
                let members: Vec<MemberData> = (0..current.row_count())
                    .map(|i| {
                        let mut m = current.row_data(i).unwrap();
                        if m.id == change.user_id.as_str() {
                            m.online = is_online;
                        }
                        m
                    })
                    .collect();
                app.set_members(Rc::new(slint::VecModel::from(members)).into());
                update_active_crew_card(app);
            }
        }
        Event::VoiceUpdated { crew_id, members: voice_members } => {
            log::debug!("UI: voice update crew={} members={}", crew_id, voice_members.len());
            let active_id = app.get_active_crew_id();
            if active_id == crew_id.as_str() {
                // Update speaking state on members list
                let current = app.get_members();
                let members: Vec<MemberData> = (0..current.row_count())
                    .map(|i| {
                        let mut m = current.row_data(i).unwrap();
                        if let Some(vm) = voice_members.iter().find(|vm| vm.user_id == m.id.as_str()) {
                            m.speaking = vm.speaking.unwrap_or(false);
                        }
                        m
                    })
                    .collect();
                app.set_members(Rc::new(slint::VecModel::from(members)).into());
                update_active_crew_card(app);
            }
        }
        Event::MessagePreviewUpdated { crew_id, messages } => {
            log::debug!("UI: message preview for crew={} count={}", crew_id, messages.len());
            let current = app.get_crews();
            let mut updated: Vec<CrewData> = (0..current.row_count())
                .map(|i| current.row_data(i).unwrap())
                .collect();
            if let Some(c) = updated.iter_mut().find(|c| c.id == crew_id.as_str()) {
                c.msg_count = messages.len().min(2) as i32;
                if let Some(m) = messages.get(0) {
                    c.m0_author = m.username.clone().into();
                    c.m0_text = m.preview.clone().into();
                }
                if let Some(m) = messages.get(1) {
                    c.m1_author = m.username.clone().into();
                    c.m1_text = m.preview.clone().into();
                }
            }
            app.set_crews(Rc::new(slint::VecModel::from(updated)).into());
        }

        Event::Error { message } => {
            log::error!("UI: error: {}", message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Helper: initialise the Slint testing backend (no display needed).
    /// Only the first call per process actually sets the backend; subsequent
    /// calls are harmless no-ops.
    fn init_test_backend() {
        i_slint_backend_testing::init_no_event_loop();
    }

    /// Wire up mic-toggle callback on `app` exactly like main() does.
    fn wire_mic_toggle(app: &MainWindow, sent: Rc<Cell<Option<bool>>>) {
        let app_weak = app.as_weak();
        app.on_mic_toggle(move || {
            if let Some(app) = app_weak.upgrade() {
                let new_muted = !app.get_mic_muted();
                app.set_mic_muted(new_muted);
                sent.set(Some(new_muted));
            }
        });
    }

    /// Wire up deafen-toggle callback on `app` exactly like main() does,
    /// including the mute-coupling logic.
    fn wire_deafen_toggle(
        app: &MainWindow,
        sent_deafened: Rc<Cell<Option<bool>>>,
        sent_muted: Rc<Cell<Option<bool>>>,
        mbd: Rc<Cell<bool>>,
    ) {
        let app_weak = app.as_weak();
        app.on_deafen_toggle(move || {
            if let Some(app) = app_weak.upgrade() {
                let new_deafened = !app.get_deafened();
                app.set_deafened(new_deafened);
                sent_deafened.set(Some(new_deafened));

                if new_deafened {
                    mbd.set(app.get_mic_muted());
                    if !app.get_mic_muted() {
                        app.set_mic_muted(true);
                        sent_muted.set(Some(true));
                    }
                } else {
                    if !mbd.get() {
                        app.set_mic_muted(false);
                        sent_muted.set(Some(false));
                    }
                }
            }
        });
    }

    #[test]
    fn mic_toggle_sends_correct_muted_state() {
        init_test_backend();
        let app = MainWindow::new().unwrap();
        let sent = Rc::new(Cell::new(None::<bool>));
        wire_mic_toggle(&app, sent.clone());

        assert!(!app.get_mic_muted(), "should start unmuted");

        app.invoke_mic_toggle();
        assert_eq!(sent.get(), Some(true), "first toggle → muted=true");
        assert!(app.get_mic_muted());

        app.invoke_mic_toggle();
        assert_eq!(sent.get(), Some(false), "second toggle → muted=false");
        assert!(!app.get_mic_muted());
    }

    #[test]
    fn deafen_toggle_sends_correct_deafened_state() {
        init_test_backend();
        let app = MainWindow::new().unwrap();
        let sent_d = Rc::new(Cell::new(None::<bool>));
        let sent_m = Rc::new(Cell::new(None::<bool>));
        let mbd = Rc::new(Cell::new(false));
        wire_deafen_toggle(&app, sent_d.clone(), sent_m.clone(), mbd);

        assert!(!app.get_deafened(), "should start undeafened");

        app.invoke_deafen_toggle();
        assert_eq!(sent_d.get(), Some(true));
        assert!(app.get_deafened());

        app.invoke_deafen_toggle();
        assert_eq!(sent_d.get(), Some(false));
        assert!(!app.get_deafened());
    }

    #[test]
    fn deafen_auto_mutes_when_unmuted() {
        init_test_backend();
        let app = MainWindow::new().unwrap();
        let sent_d = Rc::new(Cell::new(None::<bool>));
        let sent_m = Rc::new(Cell::new(None::<bool>));
        let mbd = Rc::new(Cell::new(false));
        wire_deafen_toggle(&app, sent_d.clone(), sent_m.clone(), mbd);

        // Start: unmuted, undeafened
        assert!(!app.get_mic_muted());

        // Deafen → should also mute
        app.invoke_deafen_toggle();
        assert!(app.get_deafened());
        assert!(app.get_mic_muted(), "deafen should auto-mute");
        assert_eq!(sent_m.get(), Some(true), "SetMute(true) should be sent");
    }

    #[test]
    fn undeafen_restores_unmuted_when_was_not_muted() {
        init_test_backend();
        let app = MainWindow::new().unwrap();
        let sent_d = Rc::new(Cell::new(None::<bool>));
        let sent_m = Rc::new(Cell::new(None::<bool>));
        let mbd = Rc::new(Cell::new(false));
        wire_deafen_toggle(&app, sent_d.clone(), sent_m.clone(), mbd);

        // Deafen (auto-mutes)
        app.invoke_deafen_toggle();
        assert!(app.get_mic_muted());

        // Un-deafen → should restore unmuted
        sent_m.set(None);
        app.invoke_deafen_toggle();
        assert!(!app.get_deafened());
        assert!(!app.get_mic_muted(), "un-deafen should restore unmuted state");
        assert_eq!(sent_m.get(), Some(false));
    }

    #[test]
    fn undeafen_keeps_muted_when_was_manually_muted() {
        init_test_backend();
        let app = MainWindow::new().unwrap();
        let sent_mic = Rc::new(Cell::new(None::<bool>));
        let sent_d = Rc::new(Cell::new(None::<bool>));
        let sent_m_deafen = Rc::new(Cell::new(None::<bool>));
        let mbd = Rc::new(Cell::new(false));

        wire_mic_toggle(&app, sent_mic.clone());
        wire_deafen_toggle(&app, sent_d.clone(), sent_m_deafen.clone(), mbd);

        // Manually mute first
        app.invoke_mic_toggle();
        assert!(app.get_mic_muted());

        // Deafen — already muted, should NOT send extra SetMute
        sent_m_deafen.set(None);
        app.invoke_deafen_toggle();
        assert!(app.get_deafened());
        assert!(app.get_mic_muted());
        assert_eq!(sent_m_deafen.get(), None, "no extra SetMute when already muted");

        // Un-deafen — was manually muted, should stay muted
        app.invoke_deafen_toggle();
        assert!(!app.get_deafened());
        assert!(app.get_mic_muted(), "should stay muted since user muted before deafen");
    }
}
