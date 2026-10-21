# MELLO Architecture Specification v1.0

> **Status:** LOCKED  
> **Last Updated:** 2026-03-07  
> **Authors:** Mello Team

---

## 1. Vision

Mello is a lightweight crew-based social platform with Parsec-tier streaming capabilities. Think Discord's social features meets Parsec's streaming quality — in a <100MB package, <100MB RAM usage.

**Tagline:** *Hang out with your crew. Jump into anyone's stream.*

---

## 2. Product Goals

| Goal | Target |
|------|--------|
| Client install size | <100MB |
| RAM (idle) | <50MB |
| RAM (in crew, voice active) | <80MB |
| RAM (watching stream) | <100MB |
| Voice latency (through the SFU, same region) | <50ms |
| Stream latency (LAN) | <20ms |
| Stream latency (WAN, 30ms ping) | <60ms |
| NAT traversal success | >90% |

---

## 3. High-Level Architecture

```
┌────────────────────────────────────────────────────────────────────────────┐
│                              MELLO CLIENT                                  │
│                                                                            │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │                         SLINT UI (Rust)                              │  │
│  │                                                                      │  │
│  │  Crew Panel │ Stream View │ Chat Panel │ Control Bar                 │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                                      │                                     │
│                                      │ Rust                                │
│                                      ▼                                     │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │                       MELLO-CORE (Rust)                              │  │
│  │                                                                      │  │
│  │  Nakama Client │ Crew Manager │ Voice Manager │ Stream Manager       │  │
│  │                                                                      │  │
│  │  Exports C API for mobile (post-beta)                                │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                                      │                                     │
│                                      │ FFI (C ABI)                         │
│                                      ▼                                     │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │                        LIBMELLO (C++)                                │  │
│  │                                                                      │  │
│  │  ┌────────────────┐ ┌────────────────┐ ┌──────────────────────────┐  │  │
│  │  │  Voice Engine  │ │ Stream Engine  │ │    Transport Layer       │  │  │
│  │  │                │ │                │ │                          │  │  │
│  │  │  - WASAPI      │ │ - DXGI Capture │ │  - libdatachannel        │  │  │
│  │  │  - RNNoise     │ │ - NVENC/AMF/QSV│ │  - ICE/STUN/TURN         │  │  │
│  │  │  - Silero VAD  │ │ - Decode       │ │  - DTLS encryption       │  │  │
│  │  │  - Opus        │ │                │ │  - Reliable/Unreliable   │  │  │
│  │  └────────────────┘ └────────────────┘ └──────────────────────────┘  │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                                                                            │
└───────────────────────────────────┬────────────────────────────────────────┘
                                    │
                    ┌───────────────┴───────────────┐
                    │                               │
                    ▼                               ▼
        ┌───────────────────────┐       ┌───────────────────────┐
        │    NAKAMA SERVER      │       │      SFU (mello-sfu)  │
        │                       │       │                       │
        │  - Authentication     │       │  - Voice sessions     │
        │  - Presence           │       │  - Stream delivery    │
        │  - Groups (Crews)     │       │  - RTP forwarding     │
        │  - Chat               │       │  - TURN relay fallback│
        │  - SFU tokens         │       │                       │
        └───────────────────────┘       └───────────────────────┘
```

---

## 4. Component Overview

### 4.1 Client (Slint UI)

- **Language:** Rust
- **Framework:** Slint
- **Responsibility:** All user-facing UI, renders video frames, handles input

### 4.2 mello-core

- **Language:** Rust
- **Responsibility:** Application logic, Nakama communication, orchestration
- **Exports:** C API for mobile platforms (post-beta)

### 4.3 libmello

- **Language:** C++
- **Responsibility:** Low-level audio/video/networking
- **Dependencies:** 
  - libdatachannel (peer transport to the SFU)
  - Opus (audio codec)
  - RNNoise (noise suppression)
  - Silero VAD (voice activity detection)
  - Hardware SDKs (NVENC, AMF, QSV)

### 4.4 Backend (Nakama)

- **Platform:** Heroic Labs Nakama
- **Responsibility:** Auth, presence, groups, chat, SFU tokens

---

## 5. Data Flows

### 5.1 Joining a Crew

```
┌────────┐          ┌────────┐          ┌────────┐
│ User   │          │ Nakama │          │ Other  │
│        │          │        │          │ Members│
└───┬────┘          └───┬────┘          └───┬────┘
    │                   │                   │
    │ 1. Join crew      │                   │
    │──────────────────▶│                   │
    │                   │                   │
    │                   │ 2. Broadcast      │
    │                   │   presence        │
    │                   │──────────────────▶│
    │                   │                   │
    │ 3. Member list    │                   │
    │◀──────────────────│                   │
    │                   │                   │
    │ 4. voice_join: SFU endpoint + token   │
    │◀──────────────────│                   │
    │                   │                   │
    │ 5. Voice through the SFU session      │
    │◀═══════════════ SFU ═════════════════▶│
    │                   │                   │
```

### 5.2 Voice Flow (SFU)

```
Mic ──▶ WASAPI Capture
            │
            ▼
        Echo Cancel + AGC (WebRTC APM)
            │
            ▼
        Cheap RMS / noise-floor gate
            │
            ▼
        Voice Activity (Silero VAD) ──▶ UI indicator
            │
            ▼
        Active speech enhancement (RNNoise)
            │
            ▼
        Encode (Opus, 48kHz, 20ms frames)
            │
            ▼
        Send on the RTP audio track (libdatachannel)
            │
            ▼
        SFU forwards to each member on a receive slot
            │
    ┌───────┴───────┐
    ▼               ▼
  Member A        Member B  ... (one connection per member, to the SFU)
```

### 5.3 Stream Sharing

```
┌────────┐          ┌────────┐          ┌────────┐
│ Host   │          │ Nakama │          │Viewers │
└───┬────┘          └───┬────┘          └───┬────┘
    │                   │                   │
    │ 1. Start stream   │                   │
    │──────────────────▶│                   │
    │                   │                   │
    │                   │ 2. Broadcast      │
    │                   │  "X is streaming" │
    │                   │──────────────────▶│
    │                   │                   │
    │                   │ 3. Request watch  │
    │                   │◀──────────────────│
    │                   │                   │
    │ 4. start_stream / watch_stream:       │
    │    SFU endpoint + token for each side │
    │                   │                   │
    │ 5. Video stream through the SFU       │
    │════════════════ SFU ═════════════════▶│
    │                   │                   │
```

### 5.4 Text Chat

```
User ──▶ mello-core ──▶ Nakama Channel Message ──▶ All crew members
                              │
                              ▼
                        Persisted in Nakama
```

---

## 6. Voice Topology

Every voice channel is one SFU voice session. Each member has one connection to the SFU: one upload (the microphone) and one download per remote member (a receive slot). The SFU forwards RTP and does not mix or transcode.

```
        A ──┐
        B ──┤
        C ──┼──▶ SFU ──▶ each member
        D ──┤
        E ──┘
```

The SFU is the only voice and stream transport. `voice_join`, `start_stream` and `watch_stream` return `SFU_UNAVAILABLE` when the backend cannot sign an SFU token. The crew metadata flag `"sfu_enabled": false` is an operator kill switch; the default is on.

---

## 7. Platform Support

### 7.1 Beta

| Platform | Role | Status |
|----------|------|--------|
| Windows 10/11 | Host + View | Beta |
| macOS | Host + View | Beta |

### 7.2 Post-Beta

| Platform | Role | Status |
|----------|------|--------|
| Linux | Host + View | Planned |
| iOS | View + Voice only | Planned |
| Android | View + Voice only | Planned |

---

## 8. Mobile Architecture (Post-Beta)

```
┌────────────────────────────────────────────────────────┐
│                       iOS                              │
│  ┌──────────────────────────────────────────────────┐  │
│  │                   SwiftUI                        │  │
│  └──────────────────────────────────────────────────┘  │
│                         │                              │
│                         │ Swift ↔ C FFI                │
│                         ▼                              │
│  ┌──────────────────────────────────────────────────┐  │
│  │  mello-core.a (Rust static lib via cargo-lipo)   │  │
│  │  libmello.a (C++ static lib)                     │  │
│  └──────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────┘

┌────────────────────────────────────────────────────────┐
│                     Android                            │
│  ┌──────────────────────────────────────────────────┐  │
│  │                Jetpack Compose                   │  │
│  └──────────────────────────────────────────────────┘  │
│                         │                              │
│                         │ Kotlin ↔ JNI ↔ C FFI         │
│                         ▼                              │
│  ┌──────────────────────────────────────────────────┐  │
│  │  libmello_core.so (Rust via cargo-ndk)           │  │
│  │  libmello.so (C++)                               │  │
│  └──────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────┘
```

---

## 9. Tech Stack Summary

| Layer | Technology | License |
|-------|------------|---------|
| Desktop UI | Slint | MIT/Apache 2.0 |
| Core Logic | Rust | - |
| Low-level | C++ | - |
| Nakama Client | WebSocket (Rust) | - |
| Peer Transport (to the SFU) | libdatachannel | MPL 2.0 |
| Audio Codec | Opus | BSD |
| Noise Suppression | RNNoise | BSD |
| Voice Activity | Silero VAD | MIT |
| Echo cancellation | WebRTC AEC3 | BSD |
| Gain control | WebRTC AGC2 | BSD |
| Video Capture | DXGI Desktop Duplication | Windows |
| Video Encode | NVENC / AMF / QSV | Vendor SDK |
| Video Decode | DXVA2 / NVDEC | Vendor SDK |
| Backend | Nakama | Apache 2.0 |

---

## 10. Security

| Aspect | Implementation |
|--------|----------------|
| Media Encryption (client to SFU) | DTLS 1.2 (via libdatachannel) |
| Auth | Nakama (supports OAuth, email, device) |
| Signaling | WSS to Nakama |
| Secrets | Never stored in plaintext |

---

## 11. Beta Scope

### In Scope

- Windows client + macOS client, both host and view streams
- Crews up to 6 people with voice channels
- Voice chat (SFU, RNNoise, Silero VAD)
- Text chat (Nakama)
- Stream sharing (watch only, H.264 RTP, REMB congestion control)
- Hosted SFU relay for every crew; it carries all voice and streams
- Presence indicators and crew state
- Social login: Steam, Twitch, Google, Apple, Discord + email/device auth
- Onboarding flow with crew discovery
- Crew avatars, invite codes, user search
- Settings persistence (audio devices, UI preferences)

### Out of Scope (Post-Beta)

- Stream control / input passthrough
- Linux client
- Mobile clients (iOS/Android)
- Simulcast/SVC and server-side transcoding
- Recording
- Advanced permissions / roles

---

## 12. Repository Structure

```
mello/
├── CLAUDE.md                    # AI agent coding guidelines
├── specs/                       # Technical specifications (this folder)
├── designs/                     # HTML mockups for UI features
│
├── client/                      # Slint UI (Rust)
│   ├── Cargo.toml
│   ├── src/
│   │   ├── main.rs              # Entry point, AppContext init, GIF animators
│   │   ├── app_context.rs       # Shared state struct threaded through modules
│   │   ├── callbacks/           # Slint on_* callback wiring (auth, crew, voice, settings, …)
│   │   ├── handlers/            # Event→UI dispatchers (auth, crew, voice, chat, presence, …)
│   │   ├── poll_loop.rs         # Timer-driven event drain loop
│   │   ├── converters.rs        # Data → Slint model helpers
│   │   └── …                    # avatar, settings, updater, platform, etc.
│   └── ui/
│       ├── main.slint           # Root layout, property wiring
│       ├── theme.slint          # Design tokens (colors, fonts, radii)
│       ├── types.slint          # Shared Slint structs
│       ├── icons/               # SVG icons
│       ├── fonts/               # Font files
│       └── panels/              # All UI panels and modals
│
├── mello-core/                  # Core logic (Rust)
│   ├── Cargo.toml
│   └── src/
│       ├── client/              # Client struct & async command loop
│       │   ├── mod.rs           # Client struct, new(), run(), handle_command() dispatcher
│       │   ├── auth.rs          # Auth, social login, onboarding
│       │   ├── crew.rs          # Crew CRUD, discovery, avatars
│       │   ├── chat.rs          # Messages, GIF search
│       │   ├── voice.rs         # Voice join/leave, channel management
│       │   ├── streaming.rs     # Stream host/viewer orchestration
│       │   ├── stream_ffi.rs    # FFI structs & unsafe C callbacks
│       │   ├── presence.rs      # Profile updates, catchup, moments
│       │   └── connection.rs    # Session connect, protocol check, token refresh
│       ├── command.rs           # Command enum (UI → core)
│       ├── events.rs            # Event enum (core → UI)
│       ├── nakama/              # Nakama HTTP + WebSocket client
│       ├── voice/               # Voice session through the SFU
│       ├── stream/              # Stream manager, RTP sinks, congestion/recovery
│       ├── crew_state.rs        # Sidebar & crew state models
│       └── presence.rs          # Presence types
│
├── mello-sys/                   # FFI bindings (Rust ↔ C++)
│   ├── build.rs                 # bindgen from mello.h
│   └── src/
│
├── libmello/                    # Low-level (C++17)
│   ├── CMakeLists.txt
│   ├── include/mello.h          # Public C API (single header)
│   └── src/                     # voice/, stream/, transport/, util/
│
├── backend/                     # Nakama backend
│   ├── docker-compose.yml
│   └── nakama/data/modules/     # Go runtime modules
│       ├── main.go              # RPC + hook registration
│       ├── crews.go             # Crew CRUD, discover, avatars
│       ├── streaming.go         # Stream start/stop, thumbnails
│       ├── search_users.go      # User search RPC
│       ├── invite_codes.go      # Invite code generation & join
│       ├── voice_channels.go    # Voice channel CRUD
│       ├── crew_state.go        # Crew state streaming
│       ├── presence.go          # Presence hooks
│       └── signaling.go         # ICE server list (STUN, TURN)
│
└── tools/                       # Standalone test binaries
    ├── stream-host/
    └── sfu-stream-viewer-probe/
```

---

## 13. Onboarding Flow

New users go through a 3-step onboarding before entering the main app:

1. **Discover Crews (Step 1):** A mini-discover page shows public crews in a bento grid layout (fetched via `discover_crews` RPC using `http_key`, no auth required). Users can select an existing crew to join or click "Create Your Own Crew" which opens the new-crew modal. Crew details (name, description, avatar, visibility) are stored locally — actual creation is deferred until after auth.
2. **Profile Setup (Step 2):** User picks a nickname and avatar.
3. **Identity Linking (Step 3):** Optional social login (Steam, Google, Discord, etc.) or email linking. Clicking "Continue" triggers `FinalizeOnboarding` which performs device authentication, creates the user account, creates/joins the chosen crew, and transitions to the main app.

---

## 14. Crew Creation Flow

Crew creation uses the `create_crew` RPC which: creates a Nakama group, optionally stores a base64 avatar in Nakama storage, generates an invite code, and sends Nakama notifications to any invited users. The crew avatar is stored as `{"data":"<base64>"}` in the `crew_avatars` storage collection (system-owned). Clients fetch avatars via the `get_crew_avatar` RPC.

---

## 15. Success Metrics (Beta)

| Metric | Target |
|--------|--------|
| Voice quality score (MOS) | >4.0 |
| Stream frame drops | <1% |
| Media connection success (client to SFU) | >90% |
| Crash-free sessions | >99% |
| Cold start time | <3 seconds |

---

## 16. References

- [Parsec Technology](https://parsec.app/technology)
- [Nakama Documentation](https://heroiclabs.com/docs/nakama/)
- [libdatachannel](https://github.com/paullouisageneau/libdatachannel)
- [RNNoise](https://github.com/xiph/rnnoise)
- [Silero VAD](https://github.com/snakers4/silero-vad)
- [Slint](https://slint.dev/)

---

## 17. Logging

All layers have logging infrastructure -- **use it liberally**. This is a complex multi-threaded, multi-language, real-time application. When something breaks, logs are often the only way to diagnose it.

- **libmello (C++):** Use `MELLO_LOG_INFO/WARN/ERROR/DEBUG(tag, fmt, ...)` from `util/log.hpp`. Writes to stderr.
- **mello-core (Rust):** Use `log::info!/warn!/error!/debug!` via `env_logger`. Also writes to stderr.
- **Both streams are interleaved**, so timestamps and context from both layers appear together in one output.

Log at every critical juncture: device init, pipeline state changes, encode/decode errors, connection state, packet flow. Debug-level periodic stats (e.g. every N packets) are encouraged for hot paths.

---

*This document is the north star. All implementation decisions should align with it.*
