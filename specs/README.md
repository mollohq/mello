# Mello Specifications

This directory contains the technical specifications for Mello.

- `00-ARCHITECTURE.md` - High-level architecture (north star)
- `01-CLIENT.md` - Desktop client (Slint UI)
- `02-MELLO-CORE.md` - Core logic (Rust)
- `03-LIBMELLO.md` - Low-level library (C++)
- `04-BACKEND.md` - Backend infrastructure (Nakama)
- `05-GETTING-STARTED.md` - Development setup guide
- `10-AUDIO_PIPELINE.md` - End-to-end voice/audio pipeline and SFU voice lifecycle
- `11-PRESENCE-CREW-STATE.md` - Presence, crew state, real-time push
- `13-VOICE-CHANNELS.md` - Multi-channel voice within a crew
- `15-DEBUG-TELEMETRY.md` - Debug logging, telemetry, on-demand diagnostic capture
- `features/SFU-INTEGRATION.md` - Client/backend integration with the SFU
- `EXTERNAL-SFU.md` - Bring-your-own / self-hosted SFU
- Additional feature specs under `features/`

**Voice state robustness (v0.3)** — resilience across sleep/wake, long sessions, dropped events, and reconnects — is documented across `02-MELLO-CORE.md`, `04-BACKEND.md`, `11-PRESENCE-CREW-STATE.md`, `10-AUDIO_PIPELINE.md`, `15-DEBUG-TELEMETRY.md`, and `features/SFU-INTEGRATION.md`. Test/diagnostic harnesses live in [`../TESTING.md`](../TESTING.md).

See each file for detailed specifications.
