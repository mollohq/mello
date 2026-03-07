slint::include_modules!();

use std::time::Duration;
use slint::Model;
use mello_core::{Client, Command, Config, Event};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    log::info!("Starting Mello...");

    let rt = tokio::runtime::Runtime::new()?;

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<Command>(256);
    let (event_tx, event_rx) = std::sync::mpsc::channel::<Event>();

    rt.spawn(async move {
        let mut client = Client::new(Config::default(), event_tx);
        client.run(cmd_rx).await;
    });

    let app = MainWindow::new()?;

    // --- Attempt session restore on startup ---
    let _ = cmd_tx.try_send(Command::TryRestore);

    // --- Wire login callback ---
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

    // --- Wire crew callbacks ---
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

    // --- Wire chat ---
    {
        let cmd = cmd_tx.clone();
        app.on_send_message(move |text| {
            let _ = cmd.try_send(Command::SendMessage {
                content: text.to_string(),
            });
        });
    }

    // --- Wire logout ---
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

    // --- Wire voice toggles ---
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

    // --- Event polling timer ---
    let app_weak = app.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(50), move || {
        while let Ok(event) = event_rx.try_recv() {
            if let Some(app) = app_weak.upgrade() {
                handle_event(&app, event);
            }
        }
    });

    app.run()?;
    Ok(())
}

fn handle_event(app: &MainWindow, event: Event) {
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
                id: c.id.into(),
                name: c.name.into(),
                member_count: c.member_count,
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
            app.set_active_crew_id(crew_id.into());
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
            let new_member = MemberData {
                id: member.id.into(),
                name: member.display_name.into(),
                online: true,
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
        Event::Error { message } => {
            log::error!("UI: error: {}", message);
        }
    }
}
