# MELLO-CORE Specification

> **Component:** mello-core (Application Logic)  
> **Language:** Rust  
> **Status:** Beta Scope  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

mello-core is the Rust crate that contains all application logic. It sits between the UI layer (Slint client) and the low-level C++ library (libmello). It handles Nakama communication, crew/voice/stream orchestration, and all state management.

**Key Responsibilities:**
- Nakama client (auth, presence, chat, groups, signaling, RPCs)
- Crew lifecycle (create, join, select, leave, discover)
- Voice mesh coordination via libmello FFI
- Stream session management (host/viewer, ABR, FEC)
- Presence and crew state tracking

---

## 2. Source Layout

```
mello-core/src/
├── lib.rs                  # Public exports
├── client.rs               # Main Client struct, async command loop
├── command.rs              # Command enum (UI → core)
├── events.rs               # Event enum (core → UI)
├── config.rs               # Nakama URL, http_key config
├── error.rs                # Error types
├── session.rs              # Refresh token persistence
│
├── nakama/
│   ├── mod.rs              # NakamaClient struct
│   ├── client.rs           # HTTP REST + WebSocket implementation
│   └── types.rs            # Request/response structs for RPCs
│
├── voice/
│   ├── mod.rs              # VoiceManager (mute/deafen/VAD state)
│   └── mesh.rs             # P2P mesh coordination, signaling
│
├── stream/
│   ├── mod.rs
│   ├── config.rs           # Presets (Potato/Low/Medium/High/Ultra)
│   ├── packet.rs           # Binary packet format (video/audio/control)
│   ├── fec.rs              # Forward error correction (XOR parity)
│   ├── abr.rs              # Adaptive bitrate (loss-based step up/down)
│   ├── host.rs             # Stream host pipeline
│   └── viewer.rs           # Stream viewer pipeline
│
├── crew_state.rs           # Sidebar state, voice channel models
├── presence.rs             # PresenceStatus, Activity enums
│
├── auth_discord.rs         # Discord OAuth flow
├── auth_google.rs          # Google OAuth flow
└── oauth.rs                # Shared OAuth utilities
```

---

## 3. Command/Event Architecture

The core runs a `tokio` async loop that receives `Command` variants from the UI thread and processes them. Results and state changes flow back as `Event` variants via `std::sync::mpsc::Sender`.

```
UI thread                          Core async loop (tokio)
─────────                          ────────────────────────
on_create_crew() ──Command──▶      client.handle_create_crew()
                                       │
                                       ├── nakama.create_crew() [HTTP POST RPC]
                                       │
                  ◀──Event───      Event::CrewCreated { crew, invite_code }
app.set_crews(...)
```

### Command categories

| Category | Examples |
|----------|---------|
| Auth | `TryRestore`, `DeviceAuth`, `Login`, `Logout`, `AuthSteam`, `LinkGoogle` |
| Onboarding | `DiscoverCrews`, `FinalizeOnboarding` |
| Crews | `LoadMyCrews`, `JoinCrew`, `CreateCrew`, `SelectCrew`, `LeaveCrew` |
| Social | `SearchUsers`, `JoinByInviteCode`, `FetchCrewAvatars` |
| Chat | `SendMessage` |
| Voice | `JoinVoice`, `LeaveVoice`, `SetMute`, `SetDeafen` |
| Streaming | `StartStream`, `StopStream`, `WatchStream`, `StopWatching` |
| Voice channels | `CreateVoiceChannel`, `RenameVoiceChannel`, `DeleteVoiceChannel` |
| Presence | `UpdatePresence`, `SetActiveCrew`, `SubscribeSidebar` |
| Devices | `ListAudioDevices`, `SetCaptureDevice`, `SetPlaybackDevice` |

### Event categories

| Category | Examples |
|----------|---------|
| Auth | `Authenticated`, `LoggedOut`, `OnboardingReady`, `OnboardingFailed` |
| Crews | `CrewsLoaded`, `CrewCreated`, `CrewJoined`, `DiscoverCrewsLoaded` |
| Social | `UserSearchResults`, `CrewAvatarLoaded` |
| Chat | `ChatHistory`, `ChatMessage` |
| Voice | `VoiceConnected`, `VoiceMemberJoined`, `VoiceActivity`, `VoiceSfuDisconnected` |
| Streaming | `StreamStarted`, `StreamFrame`, `StreamEnded` |
| State | `CrewStateUpdate`, `SidebarUpdate`, `VoiceChannelsUpdated` |
| Errors | `Error { message }`, `CrewCreateFailed` |

---

## 4. Nakama Client

`NakamaClient` communicates with Nakama via two channels:

| Channel | Transport | Used for |
|---------|-----------|----------|
| REST | HTTP/HTTPS | Authentication, account updates, storage reads/writes, RPC calls |
| WebSocket | WSS | Real-time presence, chat messages, P2P signaling, notifications |

### Custom RPCs

| RPC name | Auth required | Description |
|----------|---------------|-------------|
| `create_crew` | Yes | Create crew group, store avatar, generate invite code, notify invitees |
| `discover_crews` | No (http_key) | Paginated list of public crews (cursor-based) |
| `get_crew_avatar` | No (http_key fallback) | Read crew avatar base64 from storage |
| `search_users` | Yes | Search users by display name, friends listed first |
| `join_by_invite_code` | Yes | Look up invite code, join the associated crew |
| `get_ice_servers` | Yes | Return STUN/TURN server config with time-limited credentials |
| `start_stream` | Yes | Announce stream start to crew members |
| `stop_stream` | Yes | Announce stream end |
| `upload_thumbnail` | Yes | Store stream thumbnail in Nakama storage |

### Session management

On startup, the client attempts `TryRestore`: loads the refresh token from disk (`session.rs`), calls Nakama's session refresh endpoint, and if successful reconnects the WebSocket. If the token is expired or missing, the client shows the onboarding/login screen.

Device auth generates a random 32-hex-char device ID and calls `authenticate_device`. The refresh token is persisted for next launch.

---

## 5. Crew Lifecycle

```
DiscoverCrews ──▶ browse public crews (paginated, bento grid)
                        │
            ┌───────────┴───────────┐
            ▼                       ▼
    JoinCrew (by ID)         CreateCrew (name, desc, avatar, visibility, invites)
            │                       │
            ▼                       ▼
      SelectCrew ◀──────────  CrewCreated event
            │
            ▼
    Active crew: load chat history, subscribe to crew state stream,
    join crew channel for chat, subscribe sidebar presence
            │
            ▼
      LeaveCrew ──▶ unsubscribe, leave group
```

**Avatars:** Crew avatars are base64-encoded JPEG (resized to 256x256 on the client using the `image` crate). Stored in Nakama storage collection `crew_avatars`, key = crew ID, value = `{"data":"<base64>"}`, owned by system user. Fetched via `get_crew_avatar` RPC (works without auth for onboarding).

**Invite codes:** 8-character alphanumeric codes (e.g. `9SYB-3N3K`) generated server-side, stored in Nakama storage. Users can join via `JoinByInviteCode` command.

---

## 6. Voice System

Voice is managed by `VoiceManager` which wraps libmello's C FFI:

- **Mesh topology (P2P):** Full mesh for ≤6 users. Each pair exchanges SDP offers/answers via Nakama channel messages. Lower user ID initiates the offer (deterministic).
- **SFU topology:** For crews with SFU enabled, voice goes through the SFU server. The SFU prefixes each forwarded packet with `[1-byte len][sender_id]` so the client demuxes into per-sender jitter buffers and Opus decoders (see EXTERNAL-SFU.md §5.3). Without this, interleaved sequence numbers from different senders cause the jitter buffer to drop packets in 3+ user calls.
- **Audio pipeline:** Mic → WASAPI capture → RNNoise denoise → Silero VAD → Opus encode → send to each peer via libdatachannel (unreliable channel).
- **Mute/Deafen:** `SetMute` stops sending audio (capture continues for local VAD). `SetDeafen` stops playback.
- **VAD callbacks:** libmello fires speaking state changes via C callback; mello-core forwards these as `VoiceActivity` events to the UI.
- **SFU reconnect:** On `SfuEvent::Disconnected`, the voice manager resets to `Disconnected` mode and emits `VoiceSfuDisconnected`. The client's `voice_tick` detects this and auto-reconnects with exponential backoff (2s base, max 5 attempts). On failure, it emits `VoiceStateChanged { in_call: false }`.

---

## 7. Stream System

Streaming uses a host/viewer model over P2P connections:

- **Host pipeline:** DXGI capture → GPU color convert → hardware encode (NVENC/AMF/QSV) → packetize → send per-viewer via libdatachannel.
- **Viewer pipeline:** Receive packets → FEC recovery → hardware decode → GPU color convert → RGBA frame → `StreamFrame` event → Slint `Image`.
- **ABR:** Loss-based adaptive bitrate. Step down 25% if >5% loss; step up 10% if <1% loss for 10 seconds. Configurable presets from Potato (3Mbps) to Ultra (50Mbps).
- **FEC:** XOR-based parity packets. One parity per N data packets (configurable). Recovers single-packet losses without retransmission.

See [12-STREAMING.md](./12-STREAMING.md) and [14-VIDEO-PIPELINE.md](./14-VIDEO-PIPELINE.md) for full details.

---

## 8. Presence & Crew State

Real-time state flows through Nakama's streaming system (see [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md)):

- **Sidebar state:** Each crew the user belongs to has a sidebar stream. Members' online/idle/offline status, current activity (voice, streaming), and message previews flow through these streams.
- **Crew state:** When a crew is selected, the client subscribes to its crew state stream which carries voice channel membership, streaming status, and member presence changes.
- **Voice channels:** CRUD operations via dedicated commands. Default "General" channel created with each new crew. Channel state (members, who's speaking) flows via the crew state stream.

---

## 9. Dependencies

Key crate dependencies (see `mello-core/Cargo.toml` for versions):

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `reqwest` | HTTP client for Nakama REST API |
| `tungstenite` | WebSocket client for Nakama real-time |
| `serde` / `serde_json` | Serialization |
| `mello-sys` | FFI bindings to libmello (C++) |
| `log` | Logging facade |
| `rand` | Device ID generation |
| `base64` | Avatar encoding |
| `image` | Avatar resize/format conversion |

---

*This spec defines mello-core. For low-level implementation, see [03-LIBMELLO.md](./03-LIBMELLO.md).*
