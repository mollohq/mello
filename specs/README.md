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
- `12-STREAMING.md` - Canonical H.264 RTP streaming architecture and release contract
- `13-VOICE-CHANNELS.md` - Multi-channel voice within a crew
- `14-VIDEO-PIPELINE.md` - Native capture, encode/decode, and GPU presentation details
- `15-DEBUG-TELEMETRY.md` - Debug logging, telemetry, on-demand diagnostic capture
- `16-CREW-EVENT-LEDGER.md`, `17-GAME-SENSING.md`, `18-GAME-TELEMETRY.md`,
  `19-FEED-CURATION-PERSONAL-STATS.md`, `22-GAME-UI-SURFACES.md` - the game
  stack; read the section below before any of them
- `features/SFU-INTEGRATION.md` - Client/backend integration with the SFU
- `EXTERNAL-SFU.md` - Bring-your-own / self-hosted SFU
- Additional feature specs under `features/`

## The game stack: 16, 17, 18, 19, 22

These five describe one pipeline and are easy to confuse. Each answers exactly
one question:

| Spec | Question |
|------|----------|
| **17** Game Sensing | *Which game, and for how long?* |
| **18** Game Telemetry | *How did it go?* |
| **16** Crew Event Ledger | *How is that stored and moved?* |
| **19** Feed Curation & Personal Stats | *Which sessions do we show?* |
| **22** Game UI Surfaces | *What does it look like?* |

Data flows in one direction:

```
SENSE (17, 18) ──▶ RECORD (16 ledger, user_game_stats) ──▶ CURATE (19) ──▶ PRESENT (22)
```

Use this rule to place a change:

| The code | The spec |
|---|---|
| Detects or records | 16, 17, 18 |
| Selects what survives | 19 |
| Draws | 22 |

Nothing in 17 or 18 refers to a feed card. Nothing in 19 refers to detection.

Spec 17 sections 6, 7 and 9, and the spec 18 surfacing table, moved into spec
22. Spec 17 went from 958 lines to 565.

The seams between them:

| From → To | Payload |
|---|---|
| 17 → presence (11) | `game { igdb_id, name, started_at }` — live, never stored |
| 17 → 18 | `Started`/`Stopped { game_id }` — wakes the right adapter |
| 18 → 17 | `MatchResult[]` — accumulates into the open session |
| 17+18 → 16 | one `game_session` event when the game exits |
| 18 → `user_game_stats` | private streak store; only `streak_after` crosses into the public ledger |
| 16 + `user_game_stats` → 19 | the only thing curation reads |
| 19 → 20 | `order / role / size / type` — curation never crosses the wire as content |

### Two rules that cut across all five

A surface must not show a play time that a crew member can disprove. Two rules
follow from this.

- **The overnight guard.** Show wall time. Show active time instead when active
  time is below one third of wall time. Three implementations apply this rule.
  Keep them in step. See spec 22 section 4.3.
- **Co-play duration.** Use `player_overlap_min` only. Do not use the actor's
  `duration_min`. See spec 16 section 2.

### Where the design rationale lives

Spec 17 sections 2 and 3 define the detection design. That design covers the
catalogue build, the resolution ladder and the installed-library scan.

[../plans/GAME-SENSING-V2.md](../plans/GAME-SENSING-V2.md) and
[../plans/GAME-SURFACING.md](../plans/GAME-SURFACING.md) record why the work was
done that way. The specs are authoritative. The plans are history.

---

**Voice state robustness (v0.3)** — resilience across sleep/wake, long sessions, dropped events, and reconnects — is documented across `02-MELLO-CORE.md`, `04-BACKEND.md`, `11-PRESENCE-CREW-STATE.md`, `10-AUDIO_PIPELINE.md`, `15-DEBUG-TELEMETRY.md`, and `features/SFU-INTEGRATION.md`. Test/diagnostic harnesses live in [`../TESTING.md`](../TESTING.md).

See each file for detailed specifications.
