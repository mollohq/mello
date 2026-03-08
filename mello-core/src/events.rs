use crate::crew::{Crew, Member};

#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub tag: String,
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub message_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub content: String,
    pub timestamp: String,
}

#[derive(Debug, Clone)]
pub enum Event {
    Restoring,
    LoggedIn { user: User },
    LoginFailed { reason: String },

    CrewCreated { crew: Crew },
    CrewCreateFailed { reason: String },
    CrewsLoaded { crews: Vec<Crew> },
    CrewJoined { crew_id: String },
    CrewLeft { crew_id: String },

    MemberJoined { crew_id: String, member: Member },
    MemberLeft { crew_id: String, member_id: String },
    PresenceUpdated { user_id: String, online: bool },

    MessageReceived { message: ChatMessage },

    VoiceStateChanged { in_call: bool },
    VoiceConnected { peer_id: String },
    VoiceDisconnected { peer_id: String },
    VoiceActivity { member_id: String, speaking: bool },

    SignalReceived { from: String, payload: String },

    Error { message: String },
}
