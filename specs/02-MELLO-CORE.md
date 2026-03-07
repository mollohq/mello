# MELLO-CORE Specification

> **Component:** mello-core (Application Logic)  
> **Language:** Rust  
> **Status:** Beta Scope  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

mello-core is the Rust crate that contains all application logic. It sits between the UI layer (Slint) and the low-level C++ library (libmello). It handles Nakama communication, state management, and orchestrates voice/stream functionality.

**Key Responsibilities:**
- Nakama client (auth, presence, chat, groups, signaling)
- Crew and member state management
- Voice mesh coordination
- Stream session management
- C API export for mobile platforms (post-beta)

---

## 2. Project Structure

```
mello-core/
├── Cargo.toml
├── src/
│   ├── lib.rs                  # Public API
│   ├── client.rs               # Main client struct
│   ├── config.rs               # Configuration
│   │
│   ├── nakama/
│   │   ├── mod.rs
│   │   ├── client.rs           # WebSocket connection
│   │   ├── auth.rs             # Authentication
│   │   ├── presence.rs         # Presence tracking
│   │   ├── groups.rs           # Crew/group management
│   │   ├── chat.rs             # Real-time chat
│   │   └── signaling.rs        # P2P signaling via Nakama RT
│   │
│   ├── crew/
│   │   ├── mod.rs
│   │   ├── manager.rs          # Crew state management
│   │   ├── member.rs           # Member data structures
│   │   └── permissions.rs      # Who can do what
│   │
│   ├── voice/
│   │   ├── mod.rs
│   │   ├── manager.rs          # Voice session management
│   │   ├── mesh.rs             # P2P mesh coordination
│   │   └── state.rs            # Mute, deafen, VAD state
│   │
│   ├── stream/
│   │   ├── mod.rs
│   │   ├── manager.rs          # Stream session management
│   │   ├── host.rs             # Hosting a stream
│   │   └── viewer.rs           # Watching a stream
│   │
│   ├── events.rs               # Event types for UI callbacks
│   ├── error.rs                # Error types
│   │
│   └── ffi/
│       ├── mod.rs
│       └── c_api.rs            # C API for mobile (post-beta)
│
└── tests/
    ├── nakama_integration.rs
    └── voice_mesh.rs
```

---

## 3. Core Types

### 3.1 Client

```rust
// src/client.rs

use crate::nakama::NakamaClient;
use crate::crew::CrewManager;
use crate::voice::VoiceManager;
use crate::stream::StreamManager;
use crate::events::Event;

pub struct Client {
    nakama: NakamaClient,
    crews: CrewManager,
    voice: VoiceManager,
    stream: StreamManager,
    
    event_tx: mpsc::Sender<Event>,
    event_rx: mpsc::Receiver<Event>,
}

impl Client {
    pub async fn new(config: Config) -> Result<Self, Error> {
        // Initialize Nakama connection
        let nakama = NakamaClient::connect(&config.nakama_url).await?;
        
        // Initialize libmello
        let libmello = unsafe { mello_sys::mello_init() };
        
        let (event_tx, event_rx) = mpsc::channel(1024);
        
        Ok(Self {
            nakama,
            crews: CrewManager::new(),
            voice: VoiceManager::new(libmello, event_tx.clone()),
            stream: StreamManager::new(libmello, event_tx.clone()),
            event_tx,
            event_rx,
        })
    }
    
    /// Poll for events to update UI
    pub fn poll_event(&mut self) -> Option<Event> {
        self.event_rx.try_recv().ok()
    }
    
    /// Tick - call from main loop
    pub async fn tick(&mut self) {
        self.nakama.tick().await;
        self.voice.tick();
        self.stream.tick();
    }
}
```

### 3.2 Event Types

```rust
// src/events.rs

use crate::crew::{CrewId, MemberId, Member};
use crate::stream::StreamInfo;

#[derive(Debug, Clone)]
pub enum Event {
    // Connection
    Connected,
    Disconnected { reason: String },
    
    // Auth
    LoggedIn { user: User },
    LoggedOut,
    
    // Crew
    CrewJoined { crew: Crew },
    CrewLeft { crew_id: CrewId },
    MemberJoined { crew_id: CrewId, member: Member },
    MemberLeft { crew_id: CrewId, member_id: MemberId },
    MemberUpdated { crew_id: CrewId, member: Member },
    
    // Presence
    PresenceUpdated { member_id: MemberId, status: PresenceStatus },
    
    // Voice
    VoiceConnected { crew_id: CrewId },
    VoiceDisconnected { crew_id: CrewId },
    VoiceActivity { member_id: MemberId, speaking: bool },
    
    // Stream
    StreamStarted { crew_id: CrewId, host_id: MemberId, info: StreamInfo },
    StreamEnded { crew_id: CrewId, host_id: MemberId },
    StreamFrame { frame: VideoFrame },
    
    // Chat
    MessageReceived { crew_id: CrewId, message: ChatMessage },
    
    // Errors
    Error { code: ErrorCode, message: String },
}
```

### 3.3 Crew Types

```rust
// src/crew/member.rs

pub type CrewId = String;
pub type MemberId = String;

#[derive(Debug, Clone)]
pub struct Crew {
    pub id: CrewId,
    pub name: String,
    pub members: Vec<Member>,
    pub max_members: u32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub id: MemberId,
    pub name: String,
    pub tag: String,           // e.g., "#001"
    pub avatar_url: Option<String>,
    pub presence: PresenceStatus,
    pub voice_state: VoiceState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceStatus {
    Online,
    Idle,
    DoNotDisturb,
    Offline,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VoiceState {
    pub connected: bool,
    pub muted: bool,
    pub deafened: bool,
    pub speaking: bool,
}
```

---

## 4. Nakama Integration

### 4.1 Client Connection

```rust
// src/nakama/client.rs

use tokio_tungstenite::{connect_async, WebSocketStream};
use futures::{SinkExt, StreamExt};

pub struct NakamaClient {
    socket: WebSocketStream<...>,
    session: Option<Session>,
    handlers: EventHandlers,
}

impl NakamaClient {
    pub async fn connect(url: &str) -> Result<Self, Error> {
        let (socket, _) = connect_async(url).await?;
        
        Ok(Self {
            socket,
            session: None,
            handlers: EventHandlers::new(),
        })
    }
    
    pub async fn authenticate_email(
        &mut self,
        email: &str,
        password: &str,
    ) -> Result<Session, Error> {
        // ... Nakama auth flow
    }
    
    pub async fn authenticate_discord(
        &mut self,
        token: &str,
    ) -> Result<Session, Error> {
        // ... Discord OAuth flow
    }
}
```

### 4.2 Presence

```rust
// src/nakama/presence.rs

impl NakamaClient {
    pub async fn update_presence(&mut self, status: PresenceStatus) -> Result<(), Error> {
        let msg = PresenceUpdate {
            status: status.to_string(),
        };
        self.send(msg).await
    }
    
    pub async fn subscribe_presence(&mut self, crew_id: &str) -> Result<(), Error> {
        // Subscribe to presence updates for crew members
    }
}
```

### 4.3 P2P Signaling

```rust
// src/nakama/signaling.rs

/// ICE candidate exchange via Nakama real-time messaging

#[derive(Debug, Serialize, Deserialize)]
pub enum SignalMessage {
    Offer {
        from: MemberId,
        to: MemberId,
        sdp: String,
    },
    Answer {
        from: MemberId,
        to: MemberId,
        sdp: String,
    },
    IceCandidate {
        from: MemberId,
        to: MemberId,
        candidate: String,
        sdp_mid: String,
        sdp_mline_index: u32,
    },
}

impl NakamaClient {
    pub async fn send_signal(&mut self, crew_id: &str, signal: SignalMessage) -> Result<(), Error> {
        // Send via Nakama channel message (reliable)
        let data = serde_json::to_string(&signal)?;
        self.send_channel_message(crew_id, &data).await
    }
    
    pub fn on_signal<F>(&mut self, handler: F)
    where
        F: Fn(SignalMessage) + Send + 'static,
    {
        self.handlers.on_signal = Some(Box::new(handler));
    }
}
```

---

## 5. Voice Management

### 5.1 Voice Manager

```rust
// src/voice/manager.rs

use mello_sys::*;

pub struct VoiceManager {
    libmello: *mut MelloContext,
    event_tx: mpsc::Sender<Event>,
    
    // State
    crew_id: Option<CrewId>,
    connections: HashMap<MemberId, PeerConnection>,
    local_muted: bool,
    local_deafened: bool,
}

impl VoiceManager {
    pub fn join_crew(&mut self, crew_id: &CrewId, members: &[Member]) {
        self.crew_id = Some(crew_id.clone());
        
        // Start local audio capture
        unsafe {
            mello_voice_start_capture(self.libmello);
        }
        
        // Initiate P2P connections to each member
        for member in members {
            self.connect_to_member(member);
        }
    }
    
    pub fn leave_crew(&mut self) {
        // Stop capture
        unsafe {
            mello_voice_stop_capture(self.libmello);
        }
        
        // Close all connections
        self.connections.clear();
        self.crew_id = None;
    }
    
    pub fn set_mute(&mut self, muted: bool) {
        self.local_muted = muted;
        unsafe {
            mello_voice_set_mute(self.libmello, muted);
        }
    }
    
    pub fn set_deafen(&mut self, deafened: bool) {
        self.local_deafened = deafened;
        unsafe {
            mello_voice_set_deafen(self.libmello, deafened);
        }
    }
    
    fn connect_to_member(&mut self, member: &Member) {
        // Create P2P connection via libmello
        // Exchange ICE candidates via Nakama signaling
    }
    
    pub fn tick(&mut self) {
        // Poll VAD state from libmello
        for (member_id, conn) in &self.connections {
            let speaking = unsafe {
                mello_voice_is_speaking(conn.handle)
            };
            
            // Emit event if state changed
            if conn.last_speaking != speaking {
                self.event_tx.send(Event::VoiceActivity {
                    member_id: member_id.clone(),
                    speaking,
                }).ok();
            }
        }
    }
}
```

### 5.2 Mesh Coordination

```rust
// src/voice/mesh.rs

/// Full mesh topology for ≤6 people
/// 
/// For N people, each person maintains N-1 connections.
/// Total connections in mesh: N*(N-1)/2
///
/// Example with 4 people:
///     A ←→ B
///     ↕ ╲╱ ↕
///     C ←→ D
///
/// A connects to: B, C, D (3 connections)
/// Total: 6 connections

pub struct VoiceMesh {
    local_id: MemberId,
    peers: HashMap<MemberId, PeerState>,
}

pub struct PeerState {
    member_id: MemberId,
    connection: Option<PeerConnection>,
    ice_state: IceConnectionState,
    last_audio_time: Instant,
}

impl VoiceMesh {
    /// Called when a new member joins the crew
    pub fn on_member_joined(&mut self, member: &Member, signaler: &mut Signaler) {
        // Deterministic: lower ID initiates
        let should_initiate = self.local_id < member.id;
        
        if should_initiate {
            // Create offer
            let offer = self.create_offer(&member.id);
            signaler.send_offer(&member.id, offer);
        }
        // Otherwise, wait for their offer
    }
    
    /// Called when a member leaves
    pub fn on_member_left(&mut self, member_id: &MemberId) {
        if let Some(peer) = self.peers.remove(member_id) {
            peer.connection.close();
        }
    }
}
```

---

## 6. Stream Management

### 6.1 Stream Manager

```rust
// src/stream/manager.rs

pub struct StreamManager {
    libmello: *mut MelloContext,
    event_tx: mpsc::Sender<Event>,
    
    // Host state
    hosting: Option<HostSession>,
    
    // Viewer state
    watching: Option<ViewerSession>,
}

impl StreamManager {
    /// Start hosting a stream
    pub fn start_stream(&mut self, config: StreamConfig) -> Result<(), Error> {
        if self.hosting.is_some() {
            return Err(Error::AlreadyStreaming);
        }
        
        let handle = unsafe {
            mello_stream_start_host(
                self.libmello,
                config.width,
                config.height,
                config.fps,
                config.bitrate,
            )
        };
        
        self.hosting = Some(HostSession {
            handle,
            viewers: vec![],
            config,
        });
        
        Ok(())
    }
    
    /// Stop hosting
    pub fn stop_stream(&mut self) {
        if let Some(session) = self.hosting.take() {
            unsafe {
                mello_stream_stop_host(session.handle);
            }
        }
    }
    
    /// Start watching someone's stream
    pub fn watch_stream(&mut self, host_id: &MemberId) -> Result<(), Error> {
        if self.watching.is_some() {
            return Err(Error::AlreadyWatching);
        }
        
        let handle = unsafe {
            mello_stream_start_view(self.libmello)
        };
        
        self.watching = Some(ViewerSession {
            handle,
            host_id: host_id.clone(),
        });
        
        Ok(())
    }
    
    /// Get latest decoded frame
    pub fn get_frame(&self) -> Option<VideoFrame> {
        self.watching.as_ref().and_then(|session| {
            let mut frame = MelloFrame::default();
            let success = unsafe {
                mello_stream_get_frame(session.handle, &mut frame)
            };
            
            if success {
                Some(VideoFrame::from_mello(frame))
            } else {
                None
            }
        })
    }
}
```

### 6.2 Video Frame Type

```rust
// src/stream/manager.rs

#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,      // RGBA pixels
    pub timestamp: u64,      // Microseconds
}

impl VideoFrame {
    fn from_mello(frame: MelloFrame) -> Self {
        let size = (frame.width * frame.height * 4) as usize;
        let data = unsafe {
            std::slice::from_raw_parts(frame.data, size).to_vec()
        };
        
        Self {
            width: frame.width,
            height: frame.height,
            data,
            timestamp: frame.timestamp,
        }
    }
}
```

---

## 7. Public API

```rust
// src/lib.rs

pub use client::Client;
pub use config::Config;
pub use events::Event;
pub use crew::{Crew, Member, CrewId, MemberId, PresenceStatus, VoiceState};
pub use stream::{StreamConfig, StreamInfo, VideoFrame};
pub use error::{Error, ErrorCode};

// Re-export for convenience
pub mod prelude {
    pub use crate::{
        Client, Config, Event,
        Crew, Member, CrewId, MemberId,
        StreamConfig, VideoFrame,
        Error,
    };
}
```

---

## 8. Client API Summary

```rust
impl Client {
    // Lifecycle
    pub async fn new(config: Config) -> Result<Self, Error>;
    pub fn poll_event(&mut self) -> Option<Event>;
    pub async fn tick(&mut self);
    
    // Auth
    pub async fn login_email(&mut self, email: &str, password: &str) -> Result<User, Error>;
    pub async fn login_discord(&mut self, token: &str) -> Result<User, Error>;
    pub async fn logout(&mut self);
    
    // Crews
    pub async fn get_my_crews(&self) -> Result<Vec<Crew>, Error>;
    pub async fn join_crew(&mut self, crew_id: &str) -> Result<Crew, Error>;
    pub async fn leave_crew(&mut self);
    pub async fn create_crew(&mut self, name: &str) -> Result<Crew, Error>;
    pub async fn invite_to_crew(&mut self, crew_id: &str, user_id: &str) -> Result<(), Error>;
    
    // Presence
    pub async fn set_presence(&mut self, status: PresenceStatus) -> Result<(), Error>;
    
    // Voice
    pub fn voice_set_mute(&mut self, muted: bool);
    pub fn voice_set_deafen(&mut self, deafened: bool);
    pub fn voice_is_connected(&self) -> bool;
    
    // Streaming
    pub fn stream_start(&mut self, config: StreamConfig) -> Result<(), Error>;
    pub fn stream_stop(&mut self);
    pub fn stream_watch(&mut self, host_id: &MemberId) -> Result<(), Error>;
    pub fn stream_stop_watching(&mut self);
    pub fn stream_get_frame(&self) -> Option<VideoFrame>;
    
    // Chat
    pub async fn send_message(&mut self, crew_id: &str, content: &str) -> Result<(), Error>;
}
```

---

## 9. C FFI (Post-Beta)

```rust
// src/ffi/c_api.rs

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

/// Opaque handle
pub struct MelloClient(Client);

#[no_mangle]
pub extern "C" fn mello_client_new(config_json: *const c_char) -> *mut MelloClient {
    let config_str = unsafe { CStr::from_ptr(config_json).to_str().unwrap() };
    let config: Config = serde_json::from_str(config_str).unwrap();
    
    let rt = tokio::runtime::Runtime::new().unwrap();
    let client = rt.block_on(Client::new(config)).unwrap();
    
    Box::into_raw(Box::new(MelloClient(client)))
}

#[no_mangle]
pub extern "C" fn mello_client_destroy(client: *mut MelloClient) {
    if !client.is_null() {
        unsafe { Box::from_raw(client) };
    }
}

#[no_mangle]
pub extern "C" fn mello_crew_join(
    client: *mut MelloClient,
    crew_id: *const c_char,
) -> bool {
    let client = unsafe { &mut *client };
    let crew_id = unsafe { CStr::from_ptr(crew_id).to_str().unwrap() };
    
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(client.0.join_crew(crew_id)).is_ok()
}

#[no_mangle]
pub extern "C" fn mello_voice_set_mute(client: *mut MelloClient, muted: bool) {
    let client = unsafe { &mut *client };
    client.0.voice_set_mute(muted);
}

// ... more exports
```

---

## 10. Dependencies

```toml
[package]
name = "mello-core"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["lib", "staticlib", "cdylib"]

[dependencies]
# Async
tokio = { version = "1", features = ["full"] }
futures = "0.3"

# WebSocket (for Nakama)
tokio-tungstenite = { version = "0.21", features = ["native-tls"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# FFI to libmello
mello-sys = { path = "../mello-sys" }

# Utilities
log = "0.4"
thiserror = "1"
chrono = { version = "0.4", features = ["serde"] }

[dev-dependencies]
tokio-test = "0.4"
```

---

## 11. Error Handling

```rust
// src/error.rs

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Not connected to server")]
    NotConnected,
    
    #[error("Authentication failed: {0}")]
    AuthFailed(String),
    
    #[error("Crew not found: {0}")]
    CrewNotFound(String),
    
    #[error("Already in a crew")]
    AlreadyInCrew,
    
    #[error("Already streaming")]
    AlreadyStreaming,
    
    #[error("Already watching a stream")]
    AlreadyWatching,
    
    #[error("P2P connection failed: {0}")]
    P2PFailed(String),
    
    #[error("libmello error: {0}")]
    LibmelloError(String),
    
    #[error("Network error: {0}")]
    NetworkError(#[from] tokio_tungstenite::tungstenite::Error),
    
    #[error("Serialization error: {0}")]
    SerdeError(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    NotConnected = 1,
    AuthFailed = 2,
    CrewNotFound = 3,
    AlreadyInCrew = 4,
    AlreadyStreaming = 5,
    P2PFailed = 6,
    LibmelloError = 7,
    NetworkError = 8,
    Unknown = 99,
}
```

---

## 12. Testing Strategy

| Test Type | Scope | Approach |
|-----------|-------|----------|
| Unit | Pure functions | Standard `#[test]` |
| Integration | Nakama | Docker Nakama + test client |
| Mocking | libmello | Feature flag for mock FFI |

```rust
// tests/nakama_integration.rs

#[tokio::test]
async fn test_join_crew() {
    let config = Config::test_config();
    let mut client = Client::new(config).await.unwrap();
    
    client.login_email("test@test.com", "password").await.unwrap();
    
    let crew = client.join_crew("test-crew").await.unwrap();
    assert_eq!(crew.name, "Test Crew");
}
```

---

*This spec defines mello-core. For low-level implementation, see [03-LIBMELLO.md](./03-LIBMELLO.md).*
