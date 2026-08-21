# Game Surfacing — spec 19 follow-up (the PR after gamesense-v2)

> **Status:** All work items implemented on `feat/game-surfacing` (2026-08-21). See §5 for per-item state.
> **Depends on:** `feat/gamesense-v2` merged (honest sessions, co-play `player_ids`, `active_min`, `igdb_id`, presence published)
> **Amends:** [19-FEED-CURATION-PERSONAL-STATS.md](../specs/19-FEED-CURATION-PERSONAL-STATS.md) §1 locked decision on routine sessions (see §2 below)

---

## 1. Why this PR is the payoff

gamesense-v2 makes m3llo *know* everything: every session, honest durations, who
played together. Almost none of it is visible. Today the feed **prunes** any
`game_session` without telemetry outcomes ([crew_feed.go](../backend/nakama/data/modules/crew_events.go)
`gameSessionQuality` scores only W/L/streak), the sidebar shows crew-level game
chips but no per-member "playing X" line, and `active_min` / co-play data reach
the ledger and stop there.

Discord's strength is live presence; its weakness is that it **forgets
everything**. This PR is where m3llo's memory becomes something a crew can see.

## 2. Spec conflict — resolved (operator approved hybrid, 2026-08-21)

Spec 19 locked: *"Routine sessions in feed: folded into the weekly recap (no
separate digest card)."* That was decided before v2, when a session without
telemetry was an empty card. v2 changes the input: a plain session now carries
duration, co-players, icon, and a real name — it is no longer empty.

**Recommendation (hybrid):** keep spec 19's adaptive-threshold machinery, but
teach it T0 signals so plain play can qualify:

- **Low-volume crews** (the common case for a new product): routine sessions
  get a compact per-session card — "ostkatt played Valorant · 4h 12m".
- **High-volume crews:** routine sessions collapse into **one daily rollup
  card** instead of spamming; the weekly recap keeps its premium aggregate.

This preserves the anti-flooding intent of the locked decision while making the
dream sentence actually appear in a feed.

## 3. Work items

### B0 — T0-aware notability gate (backend)
`gameSessionQuality` in [crew_feed.go](../backend/nakama/data/modules/crew_feed.go)
gains signals that need no telemetry: `duration_min` (with `active_min` sanity
guard), co-player count, clips/streams overlapping the session window, first
time this game appears in the crew. Remove the unconditional prune of
no-telemetry sessions; the adaptive threshold (spec 19 §3.2) decides instead.

### C1 — Compact T0 session card (client)
Small feed card: runtime icon + "ostkatt played Valorant" + duration footer +
co-player avatars. Reuse the `GameSessionCard` shell in
[crew_feed.slint](../client/ui/panels/crew_feed.slint) but with the W/L block
absent-safe — today it assumes a record exists (~line 1011). Card copy built in
[clip.rs](../client/src/handlers/clip.rs) where "X played Y" already exists.

### B1 — Daily crew rollup card (backend + client)
One card per crew per day aggregating routine play: per-member top lines
("ostkatt · Valorant · 4h"), co-play pairs, total crew hours. New card type
registered in `mapCardType` / `fillerPriority` / `fillerRole`; new Slint card.
This is what keeps loud crews' feeds clean once B0 opens the gate.

### C2 — Per-member "playing X" line (client)
Member presence (`presence.game`) already reaches the client but stops at
crew-level chips ([presence.rs](../mello-core/src/presence.rs) ~290). Map it
into the member rows in [crew_panel.slint](../client/ui/panels/crew_panel.slint)
— "▶ Valorant · 1h 23m", duration ticking from `started_at`. This is the
Discord-parity surface; everything else in this plan is beyond parity.

### B2 — Co-play overlap minutes (backend)
[coplay.go](../backend/nakama/data/modules/coplay.go) records *who* overlapped
but not *for how long*. Compute and store `overlap_min` per co-player on the
`game_session` event. Rule: **"played together for Xh" copy may only come from
`overlap_min`** — never from the actor's `duration_min`.

### A1 — T0 hours in the You strip / profile (backend + client)
`user_game_stats` gains `hours_wall` / `hours_active` per game, accumulated from
sessions (works for every game, zero telemetry needed). You strip shows "12h
this week" when no W/L record exists instead of requiring telemetry
([stats.rs](../client/src/handlers/stats.rs) currently picks games by record).

### P — Activity-sharing consent (operator decision, 2026-08-21)
Onboarding gains a checkbox at the nickname step: **"Show crew what I'm
playing"**, default **on**. The flag gates presence publish and
`game_session_end` (and therefore every surface above), and is flippable later
in Settings. Per-game hide is out of scope for now.

## 4. Copy rules (small, but they protect the tagline)

- "played for" uses **wall time** (`duration_min`). If `active_min` is under a
  third of wall time (left open overnight), show the active figure instead —
  an inflated number a crewmate can disprove erodes the whole memory pitch.
- Co-play copy only from `overlap_min` (B2).
- Never render an empty stat slot (spec 19 §3.5) — a T0 card shows duration and
  people, not blank W/L.

## 5. Build order — each step shippable, ordered by visible value

All items are implemented on `feat/game-surfacing`, in this order:

1. **B0 + C1** — done. The dream sentence appears in the feed for typical crews.
2. **C2** — done. Member presence line; Discord parity in the sidebar.
3. **B1** — done. Daily rollup; scales the feed to loud crews.
4. **B2** — done, as data only. `player_overlap_min` is computed and stored on
   the `game_session` event, but no surface renders together-time copy yet. That
   copy is still unwritten, and when it is written it must read `overlap_min` —
   see the rule in §4.
5. **A1** — done. T0 hours in the You strip; every game gets stats, not just the
   nine telemetry titles.

The §4 overnight guard now has three implementations that must stay in step:
`game_session_duration_display` (per session, clip.rs), `weekly_time_text` (per
week, stats.rs) and `reportableMinutes` (rollup aggregation, crew_feed.go). The
rollup originally summed raw wall time, which contradicted the very cards it
summarized.

### Not yet verified on real hardware

The surfaces above are covered by pure-Go and testkit tests, but the feed cards,
the member line and the consent toggle have not been exercised against a live
backend with real crew traffic.

## 6. Out of scope

Rich notable session card (spec 19 B2, telemetry-driven), deep profile view,
weekly-recap awards/leaderboard enrichment, T1 launcher stats, catalogue
backend. Testing follows TESTING.md: pure-Go curation tests for the scorer,
threshold and rollup; testkit UI tests for the new cards and member line.
