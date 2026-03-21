<p align="center">
  <img src="assets/mello-logo.png" alt="m3llo" width="120" />
</p>

<h1 align="center">m3llo</h1>

<p align="center">
  <strong>Voice and game streaming for your crew. Runs like it's not there.</strong>
</p>

<p align="center">
  <a href="https://m3llo.app">m3llo.app</a> &nbsp;·&nbsp;
  <a href="#quick-start">Quick Start</a> &nbsp;·&nbsp;
  <a href="#self-hosting">Self-Hosting</a> &nbsp;·&nbsp;
  <a href="#architecture">Architecture</a> &nbsp;·&nbsp;
  <a href="#contributing">Contributing</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/status-alpha-orange" alt="Status: Alpha" />
  <img src="https://img.shields.io/badge/platform-windows%20%7C%20macos-blue" alt="Platform: Windows, macOS" />
  <img src="https://img.shields.io/badge/license-Apache%202.0-green" alt="License: Apache 2.0" />
  <img src="https://img.shields.io/badge/built%20with-rust%20%2B%20c%2B%2B-orange" alt="Built with Rust + C++" />
</p>

---

m3llo is a free, open-source voice and game streaming app for small groups of friends. Voice chat, game streaming, and text chat. That's the whole list. We're not adding to it.

Built in Rust and C++. Not because it's easy (we thought it would be easy, it wasn't), but because we don't want it affecting your FPS when in-game.

```
< 80MB install    < 80MB RAM in active voice    1080p60 stream    < 60ms WAN latency
```

---

## Features

### Voice

- Neural voice activity detection. The speaking ring lights up when you're actually talking, not when you sneeze or the dog barks
- ML-powered noise suppression via RNNoise. Background noise gone without touching your voice
- Echo cancellation, Opus codec at 48kHz
- Peer-to-peer mesh topology. Your voice travels directly to each crew member, no server in the middle
- End-to-end encrypted via DTLS 1.2
- Under 50ms latency on most home connections

### Streaming

- 1080p60, hardware-encoded on your GPU. NVENC, AMD AMF, or Intel QuickSync
- Your CPU barely notices. Under 1% CPU usage during active streams
- Sub-second latency over WAN. Under 20ms on LAN
- Game audio always reaches your crew, even when you're deafened
- Zero-copy GPU pipeline. Frames go GPU texture to network without touching system RAM

### Everything else

- Text chat with GIF support
- Crew presence, see who's online and what they're playing
- Login with Discord OAuth or email
- Up to 6 live participants per channel (P2P limit, see [Self-Hosting](#self-hosting) for more)

---

## Quick Start

**Prerequisites:**

- Rust 1.75+
- CMake 3.20+
- Visual Studio 2022 (Windows) with C++ workload

```bash
# Clone
git clone https://github.com/mollohq/mello.git
cd mello

# Start backend (requires Docker)
cd backend && docker compose up -d

# Run client
cd .. && cargo run -p mello-client
```

Nakama console available at `http://localhost:7351` (admin / admin)

Full setup instructions and troubleshooting in [/docs/getting-started.md](./docs/getting-started.md).

---

## Self-Hosting

m3llo is fully self-hostable. The entire client and backend is Apache 2.0.

For those of you who never really got over losing Ventrilo. For the tinkerers who need another project for that dusty Raspberry Pi in the drawer. We're one of you ourselves.

### What you get

| | Self-hosted | m3llo.app |
|---|---|---|
| Voice chat | Up to 6 per channel | Up to 6 per channel |
| Game streaming | Up to 6 viewers | Up to 6 viewers |
| Text chat | Full history on your hardware | Full history |
| Your data | Never leaves your server | Europe-based, GDPR |
| Setup | Docker + config | Sign in and go |
| Cost | Free | Free |
| Extended limits | Optional add-on | Optional add-on |

### Why 6 participants?

Self-hosted instances use peer-to-peer connections. Your stream goes directly to each viewer, which means your upload bandwidth is the bottleneck. 6 is the practical ceiling before that becomes a problem.

Planning on running a larger community? Extended limits are available as an optional add-on using our custom streaming infrastructure. No strings attached, no mandatory subscription. [Get in touch](https://m3llo.app) when you're ready.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                         CLIENT                              │
│                                                             │
│   ┌───────────┐    ┌─────────────┐    ┌──────────────┐     │
│   │  Slint UI │    │ mello-core  │    │  libmello    │     │
│   │  (Rust)   │◄──►│  (Rust)     │◄──►│  (C++)       │     │
│   │           │    │             │    │              │     │
│   │  Native   │    │  App logic  │    │  Voice       │     │
│   │  UI       │    │  Nakama SDK │    │  Stream      │     │
│   │           │    │             │    │  Transport   │     │
│   └───────────┘    └─────────────┘    └──────────────┘     │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                         BACKEND                             │
│         Nakama (Auth, Chat, Presence, P2P Signaling)        │
│                        PostgreSQL                           │
└─────────────────────────────────────────────────────────────┘
```

### Voice pipeline

```
Mic
  → WASAPI capture
  → Speex AEC (echo cancellation)
  → RNNoise (noise suppression)
  → Silero VAD (voice activity detection)
  → Opus encode (48kHz, 20ms frames)
  → libdatachannel (P2P, DTLS encrypted)
  → Peer
```

### Video pipeline (zero-copy)

```
Game renders frame
  → DXGI Desktop Duplication (GPU texture, no copy to RAM)
  → Color conversion on GPU (BGRA to NV12)
  → Hardware encode (NVENC / AMF / QSV)
  → Network

Frames stay on GPU memory until they hit the network.
Result: under 1% CPU, under 20ms LAN latency.
```

### Stack

| Component | Technology | Notes |
|-----------|------------|-------|
| UI | [Slint](https://slint.dev) | Rust-native, no Electron |
| Client logic | Rust | mello-core |
| Media layer | C++ | libmello |
| P2P transport | [libdatachannel](https://github.com/paullouisageneau/libdatachannel) | WebRTC, ICE, DTLS |
| Audio codec | Opus | BSD licensed |
| Noise suppression | RNNoise | BSD licensed |
| Voice activity | Silero VAD | MIT licensed |
| Backend | [Nakama](https://heroiclabs.com/nakama/) + PostgreSQL | Apache 2.0 |

---

## Project Structure

```
mello/
├── client/             # Slint UI (Rust)
├── mello-core/         # App logic (Rust)
├── mello-sys/          # FFI bindings (Rust)
├── libmello/           # Media layer (C++)
│   └── src/
│       ├── audio/      # Capture, VAD, AEC, noise suppression, Opus
│       ├── video/      # DXGI capture, hardware encode/decode
│       └── transport/  # WebRTC, ICE, DTLS
├── backend/
│   └── nakama/         # Server modules (Go)
└── specs/              # Design documents, read before contributing
```

---

## Contributing

We build m3llo in public. Contributions welcome.

Read the relevant spec in `/specs` before opening a PR. Agents and contributors alike, specs are the source of truth.

- Bugs: open an issue
- Ideas: start a discussion
- Code: PRs welcome, read specs first
- Docs: always needed

```bash
cargo fmt       # format
cargo clippy    # lint
```

---

## Community

We hang out on m3llo itself. Come find us.

- **m3llo crew:** [m3llo.app/crew/m3llo](https://m3llo.app/crew/m3llo)
- **Reddit:** [r/m3llo_app](https://reddit.com/r/m3llo_app)
- **Bluesky:** [@m3llo.app](https://bsky.app/profile/m3llo.app)

---

## License

Apache 2.0. See [LICENSE](LICENSE).

Extended limits for large-scale streaming on self-hosted instances require server infrastructure not included in this repo. Available as an optional add-on at [m3llo.app](https://m3llo.app).

---

<p align="center">
  <sub>Made in Göteborg, Sweden by <a href="https://github.com/mollohq">Mollo Tech AB</a></sub>
</p>
