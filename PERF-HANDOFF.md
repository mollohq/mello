# Performance Handoff — Mello Desktop (macOS)

**Date:** 2026-06-29  
**Goal:** Idle GUI RSS **<120 MB** (Discord-class) vs **545 MB** measured today.  
**Spec target:** `<50 MB` idle (`specs/01-CLIENT.md`) — we are **10× over**.

---

## 1. Measured reality (do not confuse these)

| Gate | What it measures | Idle RSS p95 | CPU p95 | Notes |
|------|------------------|--------------|---------|-------|
| `scripts/perf/run.sh` | Headless `mello-core` only | **~63 MB** | **~0.7%** | Not the product users run |
| `scripts/perf/run-gui.sh` | Release `target/release/mello` + Slint window | **~545 MB** | **~3.6%** | **This is the real gate** |
| Activity Monitor | Same as GUI gate | ~565 MB “real memory” | ~3% | User-reported; aligns with harness |
| Discord (user benchmark) | Full Electron client idle | **~120 MB** | low | What we must beat |

**Baseline committed:** `benchmarks/baselines/macos-arm64-gui.json` (`gui-idle` scenario).

---

## 2. Harness (how to reproduce)

```bash
# Backend + fixtures
docker compose -f mello/backend/docker-compose.yml up -d
../scripts/voice/prepare-fixtures.sh

# GUI gate (auto-loads voice fixtures → PERF_TEST_*)
cd mello && ./scripts/perf/run-gui.sh
./scripts/perf/run-gui.sh --write-baseline   # refresh baseline after intentional changes
```

Env: `MELLO_PERF_MODE=1` drives in-client scenario (`client/src/perf_mode.rs`). Harness samples child PID via `sampling.json` signal files.

Headless gate: `./scripts/perf/run.sh` — useful for core regressions only.

**Do not** baseline with `client-dev.sh` (debug + `SLINT_EMIT_DEBUG_INFO` + MCP) — inflates RSS and CPU.

---

## 3. Where the ~500 MB goes (ranked hypotheses)

### A. Slint + Skia renderer (largest fixed cost — likely 250–400 MB)

- `client/Cargo.toml`: `renderer-winit-skia` **and** `renderer-winit-software` both compiled in.
- Skia keeps GPU texture atlases, glyph caches, scene graph for the **entire** `MainWindow` tree at once.
- Discord’s 120 MB uses Chromium’s mature memory accounting + process model; not apples-to-apples but users don’t care.

**Fix direction:** Profile with Instruments (Allocations). Try `SLINT_BACKEND=winit-femtovg` or software-only builds for A/B. Consider lazy-loading heavy views (settings, onboarding, stream picker) into separate `Component` windows shown on demand. Long-term: native AppKit sidebar + minimal Slint content island.

### B. Avatar images duplicated at full resolution (high — fix in progress)

- `avatar::RENDER_SIZE = 280` (140px @2x) → **~313 KB RGBA per avatar**.
- `avatar_cache: HashMap<String, slint::Image>` — unbounded.
- Every chat row **clones** `sender_avatar` into `ChatMessageData` (`converters.rs`).
- `UserAvatarLoaded` rebuilds **entire chat** `VecModel` (`handlers/presence.rs`).
- Voice members, crew cards, stream cards each hold another clone of the same `slint::Image`.

**Fix direction:** `UI_CACHE_SIZE = 96` for all sidebar/chat/voice avatars; LRU cap ~64 entries; incremental row updates on avatar load (no full chat rebuild).

### C. Chat model rebuilds (medium–high)

- `refresh_chat_ui()` → full `chat_messages_to_slint` + new `VecModel` on **every** `MessageReceived`, edit, delete, avatar load.
- Each rebuild re-parses markdown (`StyledText::from_markdown`) for all messages.
- `fetch_gif_images_for_messages` decodes **all** GIF URLs in history.

**Fix direction:** ListView/windowed model (`specs/20` Phase 2). Incremental append for new messages. GIF fetch only for visible/recent rows. Markdown cache per `message_id`.

### D. GIF frame buffers (medium)

- `image_cache.rs`: up to **120 frames** per GIF, full resolution, stored as `slint::Image` per frame in `GifAnimator`.
- Chat GIF prefetch on every `refresh_chat_ui`.

**Fix direction:** Cap frames (30), downscale to max 320px wide, lazy fetch, stop animator when off-screen.

### E. Snapshot disk cache (low–medium)

- `snapshot_cache.rs`: **50 MB** disk budget; thumbnails at 480px.

### F. mello-core idle services (low for GUI RSS)

- Phase 1 gated voice_tick/stream_tick/game sensor — CPU wins, not the 500 MB gap.
- Headless idle ~63 MB proves core is lean; the gap is almost entirely **client process UI/renderer**.

### G. dev build trap

- `client-dev.sh`: debug build, `RUST_LOG=debug`, `SLINT_EMIT_DEBUG_INFO=1`, MCP server — **never use for perf baselines**.

---

## 4. Work already landed

| Area | Files | Effect |
|------|-------|--------|
| Headless perf harness | `tools/perf-harness/`, `scripts/perf/run.sh` | Core regression smoke |
| GUI perf harness v2 | `client/src/perf_mode.rs`, `run-gui.sh`, `scenarios-gui/gui_idle.json` | Real product gate |
| Adaptive core timers | `mello-core/client/tick_gating.rs` | Idle CPU ~3%→~0.7% headless |
| PTT/voice incremental UI | `converters.rs`, `handlers/voice.rs` | Less VecModel churn |
| Stream frame timer gate | `stream_frame_timer.rs` | No 16ms tick when not watching |
| Game sensor deferral | `game_services.rs` | Post-auth only |
| GIF lazy timer | `gif_animator.rs` | Timer only when GIFs active |
| Shared scenario JSON | `tools/perf-scenarios/` | Headless + GUI scenarios |

---

## 5. Prioritized roadmap (do in this order)

### P0 — Measure before optimizing
1. Instruments Allocations on release `mello` during `gui-idle` sample window.
2. Log `MelloStats.process_rss_mb` in debug panel (spec 15) — compare to `ps` sampler.
3. A/B: `SLINT_BACKEND=winit-software` vs skia — record RSS delta.

### P1 — Quick RSS wins (target: −100 to −200 MB)
1. ✅ Avatar downscale + cache cap (`avatar.rs`, `presence.rs`, `crew.rs`).
2. ✅ Stop full chat rebuild on avatar load — patch rows by `sender_id`.
3. ✅ GIF frame cap + downscale; prefetch only recent messages.
4. Reduce initial chat fetch 50→25 (`mello-core/client/crew.rs`).
5. Snapshot cache 50 MB→15 MB.

### P2 — Structural (target: −200+ MB, needed for <120 MB)
1. **Chat virtualization** — `ListView` + windowed model; stop storing 50+ full `ChatMessageData` with cloned images.
2. **Lazy view loading** — don’t instantiate settings/stream modals until opened.
3. **Renderer swap or hybrid** — femtovg/software A/B; consider Metal-native stream path (spec Phase 2).
4. **Single avatar instance** — store `user_id` in chat row, resolve avatar from cache at render time (Slint may need property indirection).

### P3 — CPU polish
1. Adaptive poll loop interval (50 ms active / 200 ms idle).
2. macOS native stream presenter — kill RGBA triple-copy (`specs` stream work).

---

## 6. Key files map

```
mello/
  client/src/
    main.rs              # perf_mode fan-out, skip restore/updater in perf
    perf_mode.rs         # GUI scenario driver
    poll_loop.rs         # 50ms UI poll — candidate for adaptive gate
    converters.rs        # chat_messages_to_slint, GIF prefetch
    handlers/chat.rs     # refresh_chat_ui on every message
    handlers/presence.rs # CrewStateLoaded, UserAvatarLoaded, avatar→full chat rebuild
    avatar.rs            # RENDER_SIZE, rasterize
    image_cache.rs       # GIF decode
    gif_animator.rs      # in-memory frame vectors
    snapshot_cache.rs    # 50MB disk cache
  mello-core/src/client/
    crew.rs              # load_initial_chat_history(50)
    tick_gating.rs       # voice/stream periodic ticks
    game_services.rs     # deferred game/telemetry
  tools/perf-harness/    # run + run-gui
  scripts/perf/          # run.sh, run-gui.sh
  benchmarks/baselines/  # macos-arm64.json (headless), macos-arm64-gui.json
  specs/20-PERF-HARNESS.md
```

---

## 7. Landmines (read before coding)

1. **Don’t benchmark debug `client-dev.sh`** — use `run-gui.sh` (release).
2. **`PERF_TEST_*` vs `VOICE_TEST_*`** — `run-gui.sh` maps fixtures; harness skips if email empty.
3. **Headless PASS ≠ product PASS** — 63 MB headless is meaningless for Discord comparison.
4. **Full `VecModel` rebuilds** — cloning `slint::Image` in every row multiplies avatar memory.
5. **`UserAvatarLoaded` chat rebuild** — catastrophic; patch rows instead.
6. **Don’t add deps** without asking (CLAUDE.md).
7. **Compile before claiming done** — `cargo build --release -p mello-client -p perf-harness`.
8. **Event fan-out** — perf_mode needs clone of `event_rx`; don’t steal from poll_loop.

---

## 8. Success criteria

| Milestone | Metric | Command |
|-----------|--------|---------|
| Phase 1 | GUI idle RSS p95 **<400 MB** | `./scripts/perf/run-gui.sh` |
| Phase 2 | GUI idle RSS p95 **<200 MB** | + chat virtualization |
| Ship bar | GUI idle RSS p95 **<120 MB** | Discord parity |
| Spec | `<50 MB` idle | May require non-Skia renderer or native UI shell |

---

## 9. Git state at handoff

- Branch: `main` (local perf work, uncommitted)
- GUI baseline: `gui-idle` rss_p95=544.86, cpu_p95=3.6
- Report artifact: `scripts/perf/artifacts/20260629T222355Z-perf-gui-33326/`

---

## 10. First command for the next agent

```bash
cd mello
./scripts/perf/run-gui.sh                    # confirm gate passes
# After P1 changes:
./scripts/perf/run-gui.sh --write-baseline   # only if RSS improved intentionally
cargo build --release -p mello-client
```

Read `CLAUDE.md`, `specs/20-PERF-HARNESS.md`, and this file before touching code.
