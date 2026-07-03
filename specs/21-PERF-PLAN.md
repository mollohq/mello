# 21 — Performance Plan: "Beat Discord"

> **Status:** in progress (branch `perf/beat-discord`)
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md), [01-CLIENT.md](./01-CLIENT.md), [20-PERF-HARNESS.md](./20-PERF-HARNESS.md)
> **Supersedes the diagnosis in** `PERF-HANDOFF.md` (its "545 MB / Skia renderer" framing and P1 avatar focus were measured to be wrong).

## 1. Measured reality (macOS, release build, idle in a crew)

Verified with `footprint`, `vmmap`, `MallocStackLogging` + `malloc_history`, and `powermetrics`:

- **phys_footprint ≈ 398 MB** (not 545 — `ps rss` over-counted shared pages by ~150 MB).
- **~125 wakeups/s** vs Discord's ~7 (raw CPU is comparable — wakeups are the real gap).
- Dominant costs, from symbolicated allocation stacks:
  - **~180 MB** — the **non-virtualized chat feed** rendered through macOS Skia/Metal text (`chat_panel.slint` `Flickable` + `for msg in messages` → `StyledText` → `SkDynamicMemoryWStream`, redrawn every DisplayLink frame).
  - **~60 MB** — Metal swapchain drawables (inherent to GPU rendering).
  - **~34 MB** — tokio blocking-pool thread stacks (16 threads).
  - **~8 MB** — game-telemetry `tiny_http` listener (running with no game).
  - **~5 MB** — Silero VAD/ONNX. **Not a problem — leave it.**
  - **~100 wakeups/s** — CoreAudio playback render thread started eagerly at startup (`AudioPipeline::initialize()`), before any VC join.

## 2. Targets (hard gates)

| Metric | Today | Ship gate | Stretch |
|---|--:|--:|--:|
| Idle phys_footprint (in crew, not VC) | 398 MB | < 180 MB | < 130 MB |
| Idle wakeups/s | 125 | < 15 | < 8 |
| Idle CPU-ms/s | 11 | < 3 | < 1.5 |
| In-VC phys_footprint | ~440 MB | < 230 MB | < 200 MB |
| In-VC wakeups/s | high | < 70 | — |

**Always measure `phys_footprint` + wakeups, never `ps rss`/`ps pcpu`.**

## 3. Phases (each lands as its own commit; each has a measurable exit gate)

### Phase 0 — Fix the ruler (measurement foundation)
- 0.1 Sample `phys_footprint` (via `proc_pid_rusage` `ri_phys_footprint`), not `ps rss`.
- 0.2 Add wakeups/s sampler (`ri_pkg_idle_wkups` + `ri_interrupt_wkups` deltas).
- 0.3 Replace `ps pcpu` with `ri_user_time + ri_system_time` deltas.
- 0.4 Wire `MelloStats` on (currently dead: `emit_process_stats=false` everywhere); show footprint in debug panel (spec 15).
- 0.5 Add a `gui-voice` scenario (join VC + injected WAV); implement inject in GUI perf mode.
- **Exit:** `run-gui.sh` reports footprint + wakeups + true CPU, with `gui-idle` and `gui-voice` baselines committed.

### Phase 1 — Chat virtualization (~180 MB) 🎯
Migrate the `chat_panel.slint` feed from `Flickable + for` to Slint `ListView`.
- 1.1 Bottom-anchoring (replace `alignment: end`).
- 1.2 Scroll-to-bottom + "new messages" pill via ListView `viewport-y`.
- 1.3 History-prepend position stability via viewport-height delta (drop the `56px` hack).
- 1.4 Keep incremental row patching; make new messages append (no full-model rebuild / markdown re-parse).
- 1.5 (optional) cap the live model to ~500 rows, re-fetch on scroll-up.
- **Exit:** `SkDynamicMemoryWStream` chat allocation gone; idle footprint ~398 → ~215 MB. QA scroll/history/edit/delete/gif.

### Phase 2 — Lazy audio pipeline (~100 wakeups/s) 🎯
Split `AudioPipeline::initialize()` (libmello); defer playback-start / capture / VAD / AEC to first `JoinVoice`; tear down on `LeaveVoice`.
- New C ABI `mello_audio_engage()/disengage()` (or fold into join/leave). **Read `specs/03-LIBMELLO.md` first.**
- Pre-warm on crew-select if first-join latency is noticeable.
- **Exit:** idle wakeups lose the ~100/s audio floor; Silero/audio buffers gone when not in VC.

### Phase 3 — Remaining idle wakeups
- 3.1 Event-driven core events (`slint::invoke_from_event_loop`) replacing the 100 ms poll; slow residual timer only for tray/menu.
- 3.2 Conditional PTT `CGEventTap` — install only in push-to-talk mode; tear down on VAD.
- 3.3 GIF animator: stop the timer when no GIFs active/visible.
- 3.4 Verify the DisplayLink quiesces at idle.
- **Exit:** idle wakeups < 15/s.

### Phase 4 — Memory & thread trims
- 4.1 Right-size tokio (`worker_threads(2)`, `max_blocking_threads(4)`).
- 4.2 Defer telemetry HTTP listener until a supported game is detected (keep config install eager).
- 4.3 One shared `reqwest::Client` (12 per-request sites).
- 4.4 Clip ring buffer lazy / smaller.
- 4.5 Verify double- not triple-buffering; keep Skia/Metal (do NOT switch to software).
- **Exit:** idle footprint < 180 MB.

### Phase 5 — Correctness fixes (found in review)
- 5.1 Clip-playback stall: set `clip_was_playing` optimistically on play (`clip.rs`).
- 5.2 Remove `unwrap()` in `converters::set_voice_member_speaking`.
- 5.3 Fix `rss_via_ps` cfg (macOS-only) to avoid Linux dead-code warning.

## 4. Previous agent's changes — keep / fix / supersede

Nothing needs a full revert. Keep the harness, `tick_gating`, incremental UI updates, `stream_frame_timer`, game-sensor deferral, GIF lazy timer, avatar/GIF caps, snapshot trim. **Fix:** clip race (5.1), `unwrap` (5.2), GIF stop path (3.3), wire `MelloStats` (0.4), real LRU eviction in `avatar::cache_insert`. **Supersede:** poll_loop 100 ms → event-driven (3.1). **Extend:** game-sensor deferral → also defer the telemetry listener (4.2).

## 5. Sequencing

0 → 1 → 2 → 3 → 4/5. Re-baseline after each phase; a phase is "done" only when its exit gate passes in `run-gui.sh`. Expected: 398 MB/125 wk → P1 ~215/~110 → P2 ~200/~20 → P3–4 ~140/<10.
