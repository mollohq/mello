use tokio::sync::mpsc;

use crate::command::Command;
use crate::config::Config;
use crate::events::Event;
use crate::nakama::NakamaClient;

pub struct Client {
    nakama: NakamaClient,
    event_tx: std::sync::mpsc::Sender<Event>,
    mic_muted: bool,
    deafened: bool,
}

impl Client {
    pub fn new(config: Config, event_tx: std::sync::mpsc::Sender<Event>) -> Self {
        Self {
            nakama: NakamaClient::new(config),
            event_tx,
            mic_muted: false,
            deafened: false,
        }
    }

    pub async fn run(&mut self, mut cmd_rx: mpsc::Receiver<Command>) {
        log::info!("Mello client started, waiting for commands...");
        while let Some(cmd) = cmd_rx.recv().await {
            self.handle_command(cmd).await;
        }
        log::info!("Mello client shutting down");
    }

    async fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::Login { email, password } => {
                self.handle_login(&email, &password).await;
            }
            Command::CreateCrew { name } => {
                self.handle_create_crew(&name).await;
            }
            Command::SelectCrew { crew_id } => {
                self.handle_select_crew(&crew_id).await;
            }
            Command::LeaveCrew => {
                self.handle_leave_crew().await;
            }
            Command::SendMessage { content } => {
                self.handle_send_message(&content).await;
            }
            Command::SetMute { muted } => {
                self.mic_muted = muted;
            }
            Command::SetDeafen { deafened } => {
                self.deafened = deafened;
            }
        }
    }

    async fn handle_login(&mut self, email: &str, password: &str) {
        match self.nakama.login_email(email, password).await {
            Ok(user) => {
                log::info!("Logged in as {} ({})", user.display_name, user.tag);

                if let Err(e) = self.nakama.connect_ws(self.event_tx.clone()).await {
                    log::error!("WebSocket connect failed: {}", e);
                    let _ = self.event_tx.send(Event::LoginFailed {
                        reason: format!("WebSocket failed: {}", e),
                    });
                    return;
                }

                let _ = self.event_tx.send(Event::LoggedIn { user });
                self.load_crews().await;
            }
            Err(e) => {
                log::error!("Login failed: {}", e);
                let _ = self.event_tx.send(Event::LoginFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn load_crews(&self) {
        match self.nakama.list_user_groups().await {
            Ok(crews) => {
                let _ = self.event_tx.send(Event::CrewsLoaded { crews });
            }
            Err(e) => {
                log::error!("Failed to load crews: {}", e);
            }
        }
    }

    async fn handle_create_crew(&mut self, name: &str) {
        match self.nakama.create_crew(name).await {
            Ok(crew) => {
                let crew_id = crew.id.clone();
                let _ = self.event_tx.send(Event::CrewCreated { crew });
                self.handle_select_crew(&crew_id).await;
                self.load_crews().await;
            }
            Err(e) => {
                log::error!("Failed to create crew: {}", e);
                let _ = self.event_tx.send(Event::CrewCreateFailed {
                    reason: e.to_string(),
                });
            }
        }
    }

    async fn handle_select_crew(&mut self, crew_id: &str) {
        if let Err(e) = self.nakama.leave_crew_channel().await {
            log::warn!("Failed to leave previous channel: {}", e);
        }

        if let Err(e) = self.nakama.join_crew_channel(crew_id).await {
            log::error!("Failed to join crew channel: {}", e);
            return;
        }

        let _ = self.event_tx.send(Event::CrewJoined {
            crew_id: crew_id.to_string(),
        });

        if let Ok(members) = self.nakama.list_group_users(crew_id).await {
            let user_ids: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
            if let Err(e) = self.nakama.follow_users(&user_ids).await {
                log::warn!("Failed to follow users: {}", e);
            }
        }
    }

    async fn handle_leave_crew(&mut self) {
        let crew_id = self.nakama.active_crew_id().map(String::from);
        if let Err(e) = self.nakama.leave_crew_channel().await {
            log::error!("Failed to leave crew: {}", e);
        }
        if let Some(id) = crew_id {
            let _ = self.event_tx.send(Event::CrewLeft { crew_id: id });
        }
    }

    async fn handle_send_message(&self, content: &str) {
        if let Err(e) = self.nakama.send_chat_message(content).await {
            log::error!("Failed to send message: {}", e);
        }
    }
}
