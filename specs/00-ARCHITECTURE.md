# MELLO Architecture Specification v1.0

> **Status:** LOCKED  
> **Last Updated:** 2026-03-07  
> **Authors:** Mello Team

---

## 1. Vision

Mello is a lightweight crew-based social platform with Parsec-tier streaming capabilities. Think Discord's social features meets Parsec's streaming quality — in a <25MB, <100MB RAM package.

**Tagline:** *Hang out with your crew. Jump into anyone's stream.*

---

## 2. Product Goals

| Goal | Target |
|------|--------|
| Client install size | <25MB |
| RAM (idle) | <50MB |
| RAM (in crew, voice active) | <80MB |
| RAM (watching stream) | <100MB |
| Voice latency (P2P) | <50ms |
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
        │    NAKAMA SERVER      │       │      P2P NETWORK      │
        │                       │       │                       │
        │  - Authentication     │       │  - Voice mesh (≤6)    │
        │  - Presence           │       │  - Stream delivery    │
        │  - Groups (Crews)     │       │  - Direct P2P         │
        │  - Chat               │       │  - TURN relay fallback│
        │  - P2P Signaling      │       │                       │
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
  - libdatachannel (P2P transport)
  - Opus (audio codec)
  - RNNoise (noise suppression)
  - Silero VAD (voice activity detection)
  - Hardware SDKs (NVENC, AMF, QSV)

### 4.4 Backend (Nakama)

- **Platform:** Heroic Labs Nakama
- **Responsibility:** Auth, presence, groups, chat, P2P signaling

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
    │ 4. For each member: ICE exchange      │
    │◀─────────────────────────────────────▶│
    │                   │                   │
    │ 5. P2P voice connections established  │
    │◀═════════════════════════════════════▶│
    │                   │                   │
```

### 5.2 Voice Flow (P2P Mesh)

```
Mic ──▶ WASAPI Capture
            │
            ▼
        Echo Cancel (Speex AEC)
            │
            ▼
        Noise Suppress (RNNoise)
            │
            ▼
        Voice Activity (Silero VAD) ──▶ UI indicator
            │
            ▼
        Encode (Opus, 48kHz, 20ms frames)
            │
            ▼
        Send to each peer (libdatachannel, unreliable)
            │
            ▼
    ┌───────┴───────┐
    ▼               ▼
  Peer A          Peer B  ... (full mesh, max 5 connections for 6 people)
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
    │ 4. ICE exchange via Nakama            │
    │◀─────────────────────────────────────▶│
    │                   │                   │
    │ 5. Video stream (P2P, per viewer)     │
    │══════════════════════════════════════▶│
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

## 6. Voice Mesh Topology

### 6.1 Small Crews (≤6 people) — Full Mesh

```
        A ◀────────▶ B
        ▲ ╲        ╱ ▲
        │  ╲      ╱  │
        │   ╲    ╱   │
        │    ╲  ╱    │
        ▼     ╲╱     ▼
        C ◀───╳────▶ D
              ╲╱
        E ◀────────▶ F

Connections per person: n-1
Total connections: n(n-1)/2

6 people = 15 connections total
Each person maintains 5 connections
```

### 6.2 Large Crews (>6 people) — SFU (Post-Beta)

```
        A ──┐
        B ──┤
        C ──┼──▶ SFU Server ──▶ All
        D ──┤
        E ──┘

Each person: 1 upload, 1 download
Server mixes/forwards
```

---

## 7. Platform Support

### 7.1 Beta

| Platform | Role | Status |
|----------|------|--------|
| Windows 10/11 | Host + View | Beta |

### 7.2 Post-Beta

| Platform | Role | Status |
|----------|------|--------|
| macOS | Host + View | Planned |
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
| P2P Transport | libdatachannel | MPL 2.0 |
| Audio Codec | Opus | BSD |
| Noise Suppression | RNNoise | BSD |
| Voice Activity | Silero VAD | MIT |
| Echo Cancellation | Speex AEC | BSD |
| Video Capture | DXGI Desktop Duplication | Windows |
| Video Encode | NVENC / AMF / QSV | Vendor SDK |
| Video Decode | DXVA2 / NVDEC | Vendor SDK |
| Backend | Nakama | Apache 2.0 |

---

## 10. Security

| Aspect | Implementation |
|--------|----------------|
| P2P Encryption | DTLS 1.2 (via libdatachannel) |
| Auth | Nakama (supports OAuth, email, device) |
| Signaling | WSS to Nakama |
| Secrets | Never stored in plaintext |

---

## 11. Beta Scope

### In Scope

- Windows client
- Crews up to 6 people
- Voice chat (P2P mesh, RNNoise, Silero VAD)
- Text chat (Nakama)
- Stream sharing (watch only)
- Presence indicators
- Login (Discord OAuth + email)

### Out of Scope (Post-Beta)

- Stream control / input passthrough
- macOS / Linux clients
- Mobile clients
- SFU for large groups
- Recording
- Advanced permissions

---

## 12. Repository Structure

```
mello/
├── README.md
├── ARCHITECTURE.md              # This document
├── specs/
│   ├── 01-CLIENT.md
│   ├── 02-MELLO-CORE.md
│   ├── 03-LIBMELLO.md
│   └── 04-BACKEND.md
│
├── client/                      # Slint UI (Rust)
│   ├── Cargo.toml
│   ├── src/
│   └── ui/                      # .slint files
│
├── mello-core/                  # Core logic (Rust)
│   ├── Cargo.toml
│   └── src/
│
├── mello-core-sys/              # FFI bindings (Rust)
│   ├── Cargo.toml
│   ├── build.rs
│   └── src/
│
├── libmello/                    # Low-level (C++)
│   ├── CMakeLists.txt
│   ├── include/
│   │   └── mello.h
│   └── src/
│       ├── voice/
│       ├── stream/
│       └── transport/
│
└── backend/                     # Nakama config & server code
    ├── docker-compose.yml
    ├── nakama/
    │   └── data/
    └── modules/                 # Custom Nakama modules (Go/Lua/TS)
```

---

## 13. Success Metrics (Beta)

| Metric | Target |
|--------|--------|
| Voice quality score (MOS) | >4.0 |
| Stream frame drops | <1% |
| P2P connection success | >90% |
| Crash-free sessions | >99% |
| Cold start time | <3 seconds |

---

## 14. References

- [Parsec Technology](https://parsec.app/technology)
- [Nakama Documentation](https://heroiclabs.com/docs/nakama/)
- [libdatachannel](https://github.com/paullouisageneau/libdatachannel)
- [RNNoise](https://github.com/xiph/rnnoise)
- [Silero VAD](https://github.com/snakers4/silero-vad)
- [Slint](https://slint.dev/)

---

## 15. Logging

All layers have logging infrastructure -- **use it liberally**. This is a complex multi-threaded, multi-language, real-time application. When something breaks, logs are often the only way to diagnose it.

- **libmello (C++):** Use `MELLO_LOG_INFO/WARN/ERROR/DEBUG(tag, fmt, ...)` from `util/log.hpp`. Writes to stderr.
- **mello-core (Rust):** Use `log::info!/warn!/error!/debug!` via `env_logger`. Also writes to stderr.
- **Both streams are interleaved**, so timestamps and context from both layers appear together in one output.

Log at every critical juncture: device init, pipeline state changes, encode/decode errors, connection state, packet flow. Debug-level periodic stats (e.g. every N packets) are encouraged for hot paths.

---

*This document is the north star. All implementation decisions should align with it.*
