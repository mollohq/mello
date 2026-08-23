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
  `19-FEED-CURATION-PERSONAL-STATS.md`, `20-GAME-UI-SURFACES.md` - the game
  stack; read the section below before any of them
- `features/SFU-INTEGRATION.md` - Client/backend integration with the SFU
- `EXTERNAL-SFU.md` - Bring-your-own / self-hosted SFU
- Additional feature specs under `features/`

## The game stack: 16, 17, 18, 19, 20

These five describe one pipeline and are easy to confuse. Each answers exactly
one question:

| Spec | Question |
|------|----------|
| **17** Game Sensing | *Which game, and for how long?* |
| **18** Game Telemetry | *How did it go?* |
| **16** Crew Event Ledger | *How is that stored and moved?* |
| **19** Feed Curation & Personal Stats | *Which of it is worth showing?* |
| **20** Game UI Surfaces | *What does it look like?* |

Data flows one way, and the boundary is worth stating plainly:

```
SENSE (17, 18) ──▶ RECORD (16 ledger, user_game_stats) ──▶ CURATE (19) ──▶ PRESENT (20)
```

**If it detects or records, it belongs to 16/17/18. If it decides what survives,
it belongs to 19. If it draws, it belongs to 20.** Nothing in 17 or 18 should
know a feed card exists; nothing in 19 should care how a game was detected.

This separation is now executed, not just declared. Spec 17's sidebar, bottom
bar and Slint sections, and spec 18's surfacing table, were marked "pending
relocation" for several versions — they moved into 20, and 17 shrank from 958
lines to 567 as a result.

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

Both exist because a number a crewmate can disprove destroys the product's
premise, and both have bitten already:

- **The overnight guard.** Play time uses wall time, unless active time is under
  a third of it. Three implementations must stay in step — see 20 §4.3.
- **Co-play duration comes only from `player_overlap_min`**, never the actor's
  `duration_min`. See 16 §2.

### Where the design rationale lives

Detection design (catalogue build, resolution ladder, installed-library scan) is
specified in **17 §2–§3**. The longer narrative of why it was built that way is
in [../plans/GAME-SENSING-V2.md](../plans/GAME-SENSING-V2.md), and the surfacing
work in [../plans/GAME-SURFACING.md](../plans/GAME-SURFACING.md). The specs are
authoritative; the plans are history.

---

**Voice state robustness (v0.3)** — resilience across sleep/wake, long sessions, dropped events, and reconnects — is documented across `02-MELLO-CORE.md`, `04-BACKEND.md`, `11-PRESENCE-CREW-STATE.md`, `10-AUDIO_PIPELINE.md`, `15-DEBUG-TELEMETRY.md`, and `features/SFU-INTEGRATION.md`. Test/diagnostic harnesses live in [`../TESTING.md`](../TESTING.md).

See each file for detailed specifications.
