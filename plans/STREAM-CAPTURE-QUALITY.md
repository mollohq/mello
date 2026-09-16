# Stream Capture Quality — Borderless + Geometry Ladder + Pause UX

> **Status:** Parts 1 + 3 implemented on branch, macOS-gated (`check.sh` green).
> Pending Windows validation — see `HANDOFF-STREAM-BORDERLESS-PAUSE.md`.
> Part 2 (geometry ladder) still design-only.
> **Scope:** Hookless only. No DLL injection, no Present hook, no overlay hook.
> **Specs:** `12-STREAMING.md`, `14-VIDEO-PIPELINE.md`.
> **Branch:** `feat/stream-borderless-ladder`.

## 0. Verification (2026-09-09)

Codebase state on `main`:

| Claim | Result |
|---|---|
| Yellow border flag missing | Confirmed. No `IsBorderRequired` in `libmello/`. `capture_wgc.cpp:97` sets only `IsCursorCaptureEnabled(false)`. |
| FSE detection is heuristic | Confirmed. `capture_process.cpp:83` uses cover >= 90% + `WS_OVERLAPPEDWINDOW == 0`. No `SHQueryUserNotificationState`. |
| Framerate ladder exists | Confirmed. `mello-core/src/stream/ladder.rs` (`FramerateLadder`), `manager.rs:606 tick_framerate_ladder`, `video_pipeline.cpp:386 set_output_fps`, `encoder_nvenc.cpp:520 set_framerate`. |
| Geometry ladder missing | Confirmed. FFI has only `mello_stream_set_bitrate` (`mello.h:646`) and `mello_stream_set_framerate` (`mello.h:658`). No `set_resolution`. No rung-change message. |
| DComp auto-resize exists | Confirmed. `client/src/dcomp_presenter.rs:330-351` detects texture dim change and calls `ResizeBuffers`. Viewer can absorb geometry change without new plumbing, but needs explicit signal for determinism. |

Old plans were stale: `STREAM-QUALITY-ROADMAP` Phases 0–2 landed, Phase 3 lists game audio (done), dual rendition, AV1, macOS — not this work. `ADAPTIVE-QUALITY-LADDER` Stage 1 landed, Stage 2 stayed design-only. Both are superseded by this file.

---

## Part 1 — Borderless capture + FSE reliability

Goal: no yellow border on Windows 11, correct backend on FSE, no behavior change on Windows 10.

### 1.1 Borderless WGC

File: `libmello/src/video/capture_wgc.cpp` (`WgcCapture::start`, after `CreateCaptureSession`).

- Set `session.IsBorderRequired(false)` best-effort, gated by `ApiInformation.IsPropertyPresent` + try/catch around the set. On `E_NOINTERFACE` / missing API, log `border=on (reason)` and continue with border. Never abort capture. (Implemented: `capture_wgc.cpp`.)
- No `RequestAccessAsync` prompt in code yet — consent lives in Settings > Privacy > screenshot border, and the OS ignores the value when denied. A first-run prompt is a UX decision, deferred.
- Document: Windows 10 build 19045 cannot hide the border. Windows 11 needs user consent in Settings. If another app requests border on the same target, border shows anyway.

Acceptance: Win11 windowed stream shows no border. Win10 stream still works with border. No crash on 19045.

### 1.2 FSE detection hardening

File: `libmello/src/video/capture_process.cpp`.

- Keep existing `is_likely_fullscreen` heuristic as primary signal.
- Add `SHQueryUserNotificationState` as secondary signal (`QUNS_RUNNING_D3D_FULL_SCREEN` suggests FSE). Discord notes false positives — so combine, do not replace:
  - Heuristic says fullscreen OR notification state says FSE → prefer DXGI-DDI.
  - Both say windowed → WGC.
- Log both signals on every backend choice and hot-swap (`covers`, `no_chrome`, `quns`). Field diagnosis needs both.
- Keep monitor-thread hot-swap + deferred start as-is. No change to swap protocol, only to the decision input.

Acceptance: FSE game streams via DXGI (no black WGC frame). Borderless game streams via WGC (no full-monitor crop). Transition windowed ↔ FSE triggers one hot-swap + IDR.

### 1.3 Small WGC latency wins (same PR, low risk)

- Frame pool 3 → 2. Reduces one buffered frame. (Done: `capture_wgc.cpp`.)
- `MinUpdateInterval` (24H2+, needs SDK 26100): deferred until the Windows build box SDK version is confirmed. Same try/catch gating pattern as 1.1 when it lands.
- No change to throttle, convert, or encode queue.

Validation: `12-STREAMING.md §15` playbook (720p60 + 1080p60, scroll/resize/DPI, `DComp present failed == 0`).

---

## Part 2 — Geometry ladder (Stage 2)

Goal: below ~1 Mbps, drop resolution before dropping below 30 fps. Stage 1 (fps) already ships. This adds geometry rungs.

### 2.1 Rungs

Ceiling stays the user preset. Walk down, never above. bpp band 0.08–0.10 (same currency as Stage 1):

| Rung | Example from Medium ceiling | Target kbps |
|---|---|---|
| F1 | 1280x720@30 (Stage 1, exists) | 2500 |
| G1 | 960x540@30 | 1600 |
| G2 | 854x480@30 | 1000 |
| G3 | 640x360@30 | 600 |

Thresholds overlap asymmetrically (up-threshold above previous down-threshold). Down in ~2 s, up after 15 s stable, 10 s cooldown, no rung below 30 fps. Same policy as `ladder.rs`.

### 2.2 Controller

File: `mello-core/src/stream/ladder.rs` + `manager.rs`.

- Extend `FramerateLadder` into a `QualityLadder` (or add `GeometryLadder` beside it). Pure function `(target_kbps, host_health, current_rung, elapsed) -> Option<Rung>`. No hardware in unit tests.
- Single writer rule stands: ladder owns `(rung, bitrate)` output. `StreamManager` feeds aggregate GCC/REMB + host health (`eq_drops`, `encode_ms_mean`), ladder emits. No second bitrate writer.
- Host-capability down-switch is sticky (no fast climb back onto a failing GPU).

### 2.3 Host pipeline changes

`libmello`:

- New FFI: `mello_stream_set_resolution(host, w, h)` in `mello.h` + `mello-sys` bindings. Mirrors `set_framerate`.
- `VideoPipeline`: retarget preprocessor (BGRA→NV12 + downscale dimensions) and encoder to new geometry, force IDR with `repeatSPSPPS = 1`.
- Risk: NVENC `nvEncReconfigureEncoder` may reject mid-stream resolution change. Spike first: if reconfigure fails, fall back to full encoder re-init on the same device (keep capture running). Measure on Windows box; do not guess from macOS.
- SFU impact: none on relay (RTP opaque). Late-join IDR cache holds old geometry for ≤1 keyframe interval. Accept and note.

### 2.4 Viewer changes

- Decoder: verify mid-stream SPS geometry change needs no re-init on NVDEC/D3D11VA path; VideoToolbox churn is a known gap (`12-STREAMING.md §14`). Log `SPS geometry changed` at INFO.
- `DCompPresenter`: `present_inner` already `ResizeBuffers` on dim change. Add explicit `rung_change` control-channel message so resize is deterministic, not inferred from first frame. Fall back to inference if message is lost.
- Show rung in UI ("480p") like Discord. Open question from old plan, decided: yes, minimal label on stream card.

### 2.5 Tests

- Unit: controller ramps, cliffs, flap inputs, oscillation penalty. Regression: sustained 1.2 Mbps at Medium ceiling lands on G1 or lower. Must fail without the fix (CLAUDE.md rule).
- No loopback bandwidth lane exists today — loopback never constrains. Add a bandwidth-limited soak lane or the test cannot catch regressions.
- Windows validation per §15 + loss-lane probe (`run-stream-viewer.ps1 -NativeMetrics`, `freeze_n`/`freeze_ms` as primary metric).

---

## Part 3 — Pause UX (streamer tabs out)

Goal: user1 streams a game, ALT-Tabs out. After a short delay all viewers see a pause screen: "user1 paused the stream". Game audio mutes. Voice continues. Tab back resumes video + audio at once.

Decisions (locked 2026-09-09): mute game audio on pause. Enter delay 2–3 s. Host sees a "viewers see paused" hint.

### 3.1 Pause trigger (host)

File: `libmello/src/video/capture_process.cpp`, `video_pipeline.cpp`, `mello-core/src/stream/manager.rs`.

- Scope: process/window capture only. Monitor capture never pauses (desktop is always present).
- Enter pause when one holds for 2.5 s while hosting: target minimized (`SW_SHOWMINIMIZED`), zero-size surface (WGC iconic size), or no captured frames at all.
- Debounce: 2.5 s to enter (hides quick ALT-Tabs), immediate exit on first new frame.
- `VideoPipeline` exposes the stall signal (it already counts `frames_captured` vs `fps_actual`). `StreamManager` owns the 2.5 s debounce and the pause state. One owner, no second writer.
- Reuses existing knowledge: deferred start, `swap_to_wgc` minimized-refusal, DXGI `kStallRecoverAfter`. Pause is the viewer-visible side of the same stall.

### 3.2 Signal + audio

- New control-channel pause/resume message on the reliable channel (same channel as cursor `0x04/0x02` + 2 s ping). New subtype, tiny payload: paused flag + reason (minimized / zero-size / stall).
- Fan out P2P per-viewer and SFU host→viewers. Verify SFU forwards host control to viewers; if not, add forwarding (control-only channel exists both legs).
- Host sends current pause state on every viewer join (like IDR replay), so late joiners land on the pause screen, not a black frame.
- On pause: host stops `send_audio` (mutes game audio now, not after a playout timeout). Voice path untouched. On resume: force IDR, resume audio.

### 3.3 Viewer + host UI

Files: `mello-core/src/events.rs` (new `StreamPaused` / `StreamResumed`), `client/src/handlers/streaming.rs`, `client/ui/panels/stream_view.slint`, `control_bar.slint`, `client/src/dcomp_presenter.rs`.

- Viewer: new `stream-paused` Slint property. Pause card shows pause icon (`icons/pause.svg`, same pattern as `session_preview_card.slint:34,244`) + dimmed card + "{name} paused the stream". WATCH/LEAVE stay available.
- Z-order rule (spec §14 gap): DComp video composites **on top** of Slint. A Slint overlay hides behind frozen video. On pause: detach swap-chain content (`SetContent(None)`) so the Slint card shows; on resume: re-attach. Saves GPU while paused.
- Host: control-bar badge "VIEWERS SEE PAUSED — tab back in" while pause state holds. Pure UI.
- Resume path must be instant: re-attach + IDR + audio resume in the same tick where possible.

### 3.4 Edge cases

- Quick tab-out < 2.5 s: no viewer flicker.
- FSE game exits to desktop (minimizes): same pause path fires. Correct.
- Minimize-before-start: viewers who join during deferred start land on pause, not black.
- Back-to-back tab-outs: exit-then-enter resets the 2.5 s clock. No stuck paused state — resume always wins on new frames.
- Audio-only check: viewer hears nothing from game while paused, hears voice throughout.

---

## Measurements — every part must be visible in live testing

Eye + live testing is the only reliable gate right now. So every part ships its own observable signals. No silent behavior changes.

### Global method

- Log-first. Every state change below logs one INFO line on the side that owns it, greppable by tag. Merge host + viewer + SFU logs with `scripts/coalesce_stream_timeline.py` for post-session review.
- Reuse `stream_client_stats` (10 s cadence, 2048-byte cap, SFU mode) for field visibility. New keys must be abbreviated. Check worst-case payload size in the existing unit test before adding keys.
- Probe tools for every validation: `scripts/run-stream-host.ps1`, `scripts/run-stream-viewer.ps1 -NativeMetrics`. Primary viewer health metric is always `freeze_n` / `freeze_ms` (what the user feels), not packet counters.

### Part 1 signals

| Signal | Where | What to see live |
|---|---|---|
| `border` | host log at WGC start | `border=off` on Win11, `border=on (reason)` on Win10/denied. Eye check: no yellow frame on Win11. |
| `backend` + reasons | host log on choice + hot-swap | `backend=WGC/DXGI covers=.. no_chrome=.. quns=..`. Eye check: FSE game shows game (not black), borderless shows window (not full monitor). |
| `cap_fps` vs `enc_fps` | host diag (exists) | Both ≈ target. Divergence names the culprit (capture vs encoder). |
| swaps | host log | One hot-swap + IDR per windowed↔FSE transition. No swap storms. |

### Part 2 signals

| Signal | Where | What to see live |
|---|---|---|
| `rung` + reason | host log + UI label | `rung=G1 reason=bandwidth/host` + stream card shows "480p". Eye check: picture gets softer, motion stays smooth. |
| `bpp` | host log | Stays in 0.08–0.10 band across rungs. |
| `freeze_n` / `freeze_ms` | viewer stats (exists) | Drops vs same-loss baseline without ladder. This is the win metric. |
| `dec_fps` / `present_fps` | viewer stats (exists) | Holds ≥30 through congestion that previously collapsed to single digits. |

### Part 3 signals

| Signal | Where | What to see live |
|---|---|---|
| `paused=1 reason=..` / `paused=0` | host log + control message | Timestamped enter/exit. Eye check: pause card appears 2–3 s after ALT-Tab, disappears <1 s after tab-back. |
| Resume IDR latency | host + viewer logs | Time from first new captured frame to viewer present. Target: <500 ms. |
| False pauses | host log | Zero pause enters during normal play (no minimize, steady frames). Any hit here is a blocker. |
| Audio mute | viewer log | Game audio stops on pause, returns on resume, no stuck silence. |

### Live test checklist (Windows box, release builds)

1. P1: windowed game → no yellow border (Win11) / border present but stream works (Win10).
2. P1: FSE game → game visible, no black frame. ALT-Tab ↔ game → one hot-swap each way.
3. P2: throttle to ~1 Mbps → rung label drops, motion stays smooth, `freeze_ms` flat. Restore bandwidth → rung climbs after ~15 s.
4. P3: ALT-Tab out → pause card in 2–3 s, game audio silent, voice alive. Tab back → video + audio back <1 s. Quick tab (<2 s) → no card.
5. P3: viewer joins mid-pause → sees pause card immediately.
6. All: `DComp present failed == 0`, idle RAM <100 MB when watching.

---

## Process

- One branch (`feat/stream-borderless-ladder`), conventional commits.
- Land separately: Part 1, then Part 3, then Part 2. Part 3 is independent of Part 2 (control + UI, no encoder work). Part 2 is riskiest (encoder re-init spike) and lands last. Do not mix WGC diffs with encoder re-init diffs.
- Windows-only files cannot compile-check on macOS dev machine — keep diffs minimal/mechanical, validate on Windows box per spec §15 + checklist above.
- Spec updates land with behavior changes (`12-STREAMING.md §3.1`, `§8`, `§11` lifecycle, `§14` gaps).
- No new dependencies. No hooks in this work. No PR without approval.

## Explicit non-goals

- No `DiscordHook64`-style injection.
- No `Present` / swapchain hook.
- No in-game overlay process.
- No kernel driver, no handle hijack, no anti-cheat evasion.
