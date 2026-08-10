# Adaptive Quality Ladder — Runtime Rung Switching

> **Status:** Stage 1 (framerate rungs) implemented on `feat/stream-adaptive-quality`.
> Stage 2 (geometry rungs) still design-only — see §2.5.
> **Origin:** 2026-08-09 field test. A host delivered a median 44 fps but spent 17% of
> 10s windows under 20 fps, dipping to 5–7 fps, while Discord stayed fluid on the same
> machine at the same time.
> **Specs touched (on implementation):** 12-STREAMING §8 (new §8.3), 14-VIDEO-PIPELINE §6.1.
> **Roadmap:** not currently listed in STREAM-QUALITY-ROADMAP Phase 3 — this is an addition.

---

## 1. Problem

Bitrate is the only runtime adaptation knob in the stack:

```
mello-core/src/stream/manager.rs:241   mello_stream_set_bitrate(self.host, clamped)
libmello/include/mello.h:635           the only hot-reconfigure entry point
```

`QualityPreset` (`stream/config.rs`) fixes width, height and fps **once, before the
stream starts**. Congestion control then moves bitrate within that frozen geometry.

So when available throughput halves, the host keeps encoding 1280×720 at 60 fps and
starves it. At 1.5 Mbps that is ~0.027 bits per pixel per frame — roughly a third of
what the format needs. The encoder either emits mush or fails to hit the target and
stalls, which is what produces single-digit delivered fps.

This contradicts a commitment the streaming spec already makes:

> **12-STREAMING §1:** *Favor visible quality loss (artifacts, lower bitrate) over lag or stalling.*

The ladder needed to honour that exists. Nothing connects it to the controller.

### 1.1 A second finding: the existing preset ladder is not bits/pixel-consistent

Holding bits-per-pixel-per-frame (bpp) roughly constant is what keeps perceived quality
stable as geometry changes. The current presets do not:

| Preset | Geometry | kbps | Mpx/s | **bpp** |
|---|---|---|---|---|
| Ultra | 1920×1080@60 | 8000 | 124.4 | **0.064** |
| High | 1920×1080@30 | 4500 | 62.2 | **0.072** |
| Medium | 1280×720@60 | 5000 | 55.3 | **0.090** |
| Low | 1280×720@30 | 3000 | 27.6 | **0.109** |
| Potato | 854×480@30 | 1500 | 12.3 | **0.122** |

Ultra is the *least* well provisioned rung in the set — a user selecting "best quality"
gets 30% fewer bits per pixel than Medium. Worth fixing independently of this work.

---

## 2. Design

### 2.1 The ladder

Rungs are **derived from the selected preset, which becomes the ceiling**. Adaptation
walks down from it and never above it — the user's choice stays an upper bound, so this
never overrides an explicit "give me 1080p".

Rungs are constructed to hold bpp in the 0.08–0.10 band. Example, ceiling = Medium:

| Rung | Geometry | Target kbps | bpp | Down-switch below | Up-switch above |
|---|---|---|---|---|---|
| 0 | 1280×720@60 | 5000 | 0.090 | 3200 | — |
| 1 | 1280×720@60 | 3500 | 0.063 | 2400 | 4200 |
| 2 | 1280×720@30 | 2500 | 0.090 | 1600 | 3200 |
| 3 | 960×540@30 | 1600 | 0.103 | 1000 | 2200 |
| 4 | 854×480@30 | 1000 | 0.081 | 600 | 1400 |
| 5 | 640×360@30 | 600 | 0.087 | — | 900 |

Up- and down-thresholds deliberately overlap asymmetrically: the up-threshold for rung
N sits above the down-threshold for rung N−1, so a steady estimate between them cannot
oscillate.

### 2.2 Where the controller sits

Today `StreamManager` writes the congestion estimate straight to `set_bitrate`. The
ladder controller must sit **between** them, and own the encoder target. Two components
writing bitrate independently would fight.

```
  viewer REMB ─┐
  viewer GCC  ─┼─▶ aggregate target ─┐
               │                     ├─▶ LadderController ─▶ (rung, bitrate) ─▶ encoder
  encode queue ┤                     │
  encode_ms    ┴─▶ host capability ──┘
```

### 2.3 Two independent inputs

**Bandwidth** — the aggregated GCC/REMB target the manager already computes.

**Host capability** — new, and the reason today's failure was invisible. If `eq_drops`
is climbing or `encode_ms` exceeds the frame budget, the host cannot sustain the rung
*regardless of how much bandwidth is available*. The encode queue is `ENCODE_QUEUE_CAP = 2`
with newest-wins eviction, so a struggling encoder silently discards frames and reports
no error anywhere. Dropping the rung fixes that case too.

A host-capability down-switch must be sticky: re-probing upward against a GPU that
cannot keep up just re-enters the stall.

### 2.4 Switching policy

- **Down:** 2 consecutive 1s samples below the rung floor. Fast — stalling is the worst outcome.
- **Up:** 15 consecutive seconds above the next rung's up-threshold. Slow, matching the
  existing "increases are rate-limited" REMB policy in §8.2.
- **Cooldown:** 10 s minimum between switches.
- **Oscillation penalty:** if an up-switch reverses within 30 s, double the dwell
  requirement for that rung, decaying back after 5 minutes of stability.
- **On switch:** reconfigure the encoder, force an IDR (SPS/PPS ride every keyframe
  already — `repeatSPSPPS = 1`).

### 2.5 Staging — framerate first

**Stage 1 — framerate only. ✅ Implemented** (`stream/ladder.rs`, spec 12 §8.1.1).
Rungs that change fps but not geometry (0→1→2 above).
No SPS geometry change, no decoder re-init, no swap-chain resize, **no viewer-side
change at all**. At fixed bitrate, halving fps doubles bits per frame — 720p30 at
1.5 Mbps is watchable where 720p60 is not. This is low-risk and likely recovers most
of the observed loss.

**Stage 2 — geometry.** Adds 540p/480p/360p rungs. Requires:
- viewer decoder tolerating mid-stream SPS geometry change (VideoToolbox already churns
  on SPS change — 12-STREAMING §14 known gap);
- `DCompPresenter` swap chain resize (it is created at stream resolution with a scale
  transform, `client/src/dcomp_presenter.rs`);
- a `rung_change` control-channel message so the presenter resizes deterministically
  rather than inferring from the first decoded frame.

Ship Stage 1 alone if Stage 2 slips.

### 2.6 SFU impact

None on the relay path — RTP is forwarded opaquely, so geometry changes are transparent.

One edge: the late-join IDR cache holds the last complete IDR. Immediately after a rung
switch that cached AU is the old geometry. We force an IDR on switch, so the exposure is
one keyframe interval; a joining viewer may decode one stale-geometry IDR before the new
one lands. Acceptable, but note it if late-join artefacts appear.

---

## 3. Testing

- **Controller is a pure function** — `(target_kbps, host_health, current_rung, elapsed) -> Option<Rung>`.
  Unit-testable with no hardware: ramps, cliffs, flapping inputs, oscillation-penalty decay.
- **Regression test must fail without the fix** (CLAUDE.md): feed a sustained 1.2 Mbps
  estimate at ceiling Medium and assert the controller lands on rung 3 or lower.
- **Soak:** existing `run-stream-certification.ps1` gates `avg dec_fps` and `min dec_fps < 45`
  over loopback. Extend with a bandwidth-limited lane — the current gate cannot fail this
  way because loopback never constrains.

## 4. Open questions

1. Should the rung be visible in the viewer UI ("480p"), or silent? Discord shows it.
2. Should a host-capability down-switch surface to the streamer ("your GPU can't keep up
   at 60fps")? It is actionable, unlike a bandwidth drop.
3. Does the AV1 path need its own bpp table? AV1 preset bitrates are ~half H.264's, so
   the same geometry implies a different band.
