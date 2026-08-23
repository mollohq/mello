---
name: Echo cancellation improvements
overview: "Fix acoustic echo end-to-end: upgrade the stale WebRTC audio engine (v1.3/M88), use OS-grade AEC where the platform provides it, and add neural residual-echo suppression conditioned on our own playout reference — so speaker (non-headset) users stop transmitting echo from friends' voices, clips, and other apps. Full duplex is never sacrificed."
todos:
  - id: a-upgrade-engine
    content: "Upgrade vendored webrtc-audio-processing v1.3 -> v2.x, rebuild CMake wrapper, keep API surface"
    status: pending
  - id: a-delay-hints
    content: "Feed measured device latency as APM stream-delay hints (CoreAudio + WASAPI)"
    status: pending
  - id: a-regression-harness
    content: "ERLE regression harness: synthetic loopback test asserting cancellation, fails without render feed"
    status: pending
  - id: b-vpio-macos
    content: "macOS VoiceProcessingIO capture backend behind runtime echo-cancellation toggle"
    status: pending
  - id: c-model-pick
    content: "Select + benchmark a small two-input (mic + far-end) suppression model, export ONNX"
    status: pending
  - id: c-integration
    content: "Integrate model post-AEC in audio pipeline behind settings flag (default off)"
    status: pending
  - id: c-validation
    content: "Latency/CPU bench vs budgets, field test matrix, decide default-on"
    status: pending
  - id: docs
    content: "Update specs/03-LIBMELLO.md §4 and specs/10-AUDIO_PIPELINE.md to match landed reality"
    status: pending
isProject: false
---

# Echo Cancellation Improvements

## Problem statement

Users without headsets transmit acoustic echo into VC: their crew hears themselves (remote voices re-entering via the mic) and hear local clip playback. This was reproduced and instrumented in field debugging on 2026-08-23. It is a showstopper-class issue for anyone not on a headset — i.e., the default case for casual users.

**Operator decision (binding):** never degrade VC to fix echo. No half-duplex, no automatic send gating, no muting. Full duplex always; echo must be *suppressed*, not avoided. (Half-duplex was implemented and rejected the same day.)

## Established facts from field debugging (2026-08-23)

These measurements were produced with synthetic loopback unit tests plus `clip gate:` INFO logging in the live client. Trust them; don't re-derive:

- The vendored library is **webrtc-audio-processing v1.3 = WebRTC M88 (2021)** (`libmello/third_party/webrtc-audio-processing`, version string in its `meson.build`). Five years old.
- **AEC3 in our wrapper cancels only ~13 dB** of a perfectly correlated echo path (synthetic loopback, broadband excitation, AGC2 off). Healthy modern implementations reach 25–35 dB. With tonal test signals it collapses to ~1 dB — use broadband excitation in any ERLE test.
- **AGC2 amplifies echo residue**: on intermittent audio (clip speech gaps) its adaptive gain pumps and blasts residue up to **+19 dB above the raw mic level** (field: raw −37.7 dBFS → post −18.5 dBFS).
- The pipeline wiring is correct: `mix_output()` feeds the final playout into `echo_canceller_.process_render()` (audio_pipeline.cpp). The engine, not the plumbing, is the bottleneck.
- Room reverb smears single-lag correlation checks: max normalized correlation across one real clip ranged 0.19–0.71. Correlation-based *gating* is unreliable; correlation-conditioned *suppression* remains valid.
- The existing `EchoCancellerTest` suite never asserted actual cancellation ("verify it doesn't crash") — nothing guarded this area. Fix that first.

## Alternatives considered

### A. Upgrade webrtc-audio-processing v1.3 → v2.x + delay hints ⭐ recommended first

Replace the third_party tree with the current freedesktop v2.x release (M130-era AEC3: substantially better delay estimation/handling of Bluetooth latency, transparent-mode fixes, better double-talk). Feed real device latency into APM (`set_stream_delay_ms`) so the delay estimator starts converged instead of searching.

| Pros | Cons |
|---|---|
| Fixes the engine for **all** echo classes: remote voices, clips, everything | Build integration work (source-list CMake wrapper must be regenerated for the new tree) |
| No new runtime dependencies; same public API shape (`AudioProcessing::Config`) | Still classical DSP: bad rooms + laptop speakers retain a floor of residue |
| Benefits every platform identically | absl dependency bump may ripple into the vcpkg manifest |
| Removes five years of known AEC3 bugs | |

Implementation notes:
- Source lives in `libmello/third_party/webrtc-audio-processing`; the build wrapper is `libmello/cmake/webrtc-audio-processing/CMakeLists.txt` (explicit `.cc` list + NEON/SSE subtargets). Upstream ships meson; either regenerate the file list for v2.x in the existing CMake wrapper style, or add an ExternalProject meson build. Prefer keeping the in-repo CMake approach consistent with how rnnoise is wrapped.
- Public API deltas are small but real: audit `apply_config()` in `libmello/src/audio/echo_canceller.cpp` against the v2.x `audio_processing.h` (config struct fields occasionally move; `residual_echo_detector` may be replaced by the newer echo detector configuration).
- Delay hints: on macOS read `kAudioUnitProperty_Latency` + `kAudioUnitProperty_SafetyOffset` from both the input and output units and call `apm->set_stream_delay_ms(output_latency − input_latency + jitter_buffer_depth)` per frame; on Windows use `IAudioClient::GetStreamLatency()` + `GetDevicePeriod()`. Re-compute on every `set_capture_device()` / `set_playback_device()` switch (see spec 03 §4.2 invariants — the callback rewiring rules apply unchanged).
- Keep the int16 API usage (`ProcessStream` / `ProcessReverseStream`) — it persists across versions.

### B. macOS VoiceProcessingIO capture backend ⭐ cheap big win on Mac

Use `kAudioUnitSubType_VoiceProcessingIO` for the *capture* AudioUnit on macOS instead of the plain input unit. Apple's VPIO runs its own OS-integrated AEC/AGC against the actual output route — including Bluetooth adapters whose latency defeats software-only AEC — and requires no render-reference plumbing from us.

| Pros | Cons |
|---|---|
| Apple-grade AEC tuned per-device, handles AirPods latency correctly | macOS-only (Windows has no good equivalent — this is why Discord bundles Krisp there) |
| Zero model/dataset work; a capture-backend change | VPIO forces its own processing stack — must disable our AGC2/NS on that path to avoid stacking artifacts |
| Directly addresses the worst field case (Bluetooth) | Behavior differences: VPIO manages its own buffers/sample-rate conversion; validate the 48k mono int16 contract and underrun behavior carefully |

Implementation notes:
- `libmello/src/audio/capture_coreaudio.cpp`: add a VPIO variant (component subtype swap) selected by a constructor flag; `AudioPipeline` picks the backend from the existing `mello_voice_set_echo_cancellation(bool)` runtime control — enabled ⇒ VPIO, disabled ⇒ plain unit + software AEC (keeps the "all exposed controls are runtime-effective" exit criterion, spec 10 §11).
- When VPIO is active, skip our `process_capture` APM pass (avoid double AEC); keep RNNoise + VAD downstream unchanged.
- Test matrix must include AirPods connect/disconnect mid-session and output-device switches (VPIO re-routes its reference automatically, but verify no stalls — see the April 2026 device-switch incident pattern in spec 03 §4.2).

### C. Neural residual-echo suppression (two-input model) ⭐ the durable fix

A small neural network takes **(post-AEC mic, far-end reference)** and outputs near-end speech with echo suppressed — trained to generalize across rooms/devices, including echo from audio Mello doesn't own (game sounds, Spotify) where no exact reference-based method can help. This is the category Discord licenses (Krisp) and NVIDIA ships (Maxine AEC); open small models now cover it.

| Pros | Cons |
|---|---|
| Generalizes beyond exact references — covers other-app audio too | Model selection/export/tuning effort is real; quality varies widely between candidates |
| Preserves full duplex (continuous suppression, passes genuine near-end speech) | Adds CPU + latency on the capture path; must be measured against budgets below |
| Fits existing infra: **ONNX Runtime already ships for Silero VAD** — session management, model loading, Windows delay-load pattern all exist | Needs a far-end reference ring with correct frame alignment (pitfall discovered in field work: CoreAudio delivers ~512-sample callbacks, not our 960-sample frames — accumulate chunks before framing; see git history `render_pending_` accumulator) |
| Small models exist with permissive licenses | One more binary asset in the installer (budget below) |

Candidate shortlist (benchmark ≥2 before committing):

| Candidate | Notes |
|---|---|
| **DTLN-AEC / Microsoft AEC-Challenge baselines** | Explicitly two-input (mic + far-end); small; the challenge datasets are public for fine-tuning; well-proven lineage |
| **GTCRN-class tiny CRNN** | ~24k params, real-time on weak CPUs; needs a two-input/echo variant or fine-tune on AEC-challenge data |
| **DeepFilterNet family** | Official Rust crate (`deep-filter`) fits the stack; primarily denoise — evaluate its residual-echo robustness honestly before adopting |

Integration sketch:
- Location: in `AudioPipeline::on_captured_audio` immediately after `echo_canceller_.process_capture(...)` and before the RMS gate/VAD — the model cleans residue, the gate then sees genuinely clean audio (gate ordering matters; see spec 10 §4 step order).
- Reference conditioning: retain recent playout frames (chunk-accumulated to FRAME_SIZE) with a coarse delay search at stream start (±160 ms in 20 ms steps, pick best correlation) plus slow re-tracking. The A-delay-hints work shrinks the search space.
- Runtime: second ORT session beside `vad_`; int8/fp16 quantized model; **budgets**: ≤10 ms inference per 20 ms frame on min-spec Windows CPU, ≤20 MB RAM steady-state, ≤6 MB installer size. Measure with `tools/perf-harness` patterns and the windowed-audio-stats lane (spec 10 §12).
- Latency guard: models must be causal or ≤1-frame lookahead; verify median mouth-to-ear stays under the 50 ms target (spec 10 §9 hard gate) before enabling anywhere.
- Rollout: `settings.echo_suppression` flag, **default off**, field-validate via the manual matrix below, then flip default in a separate commit once validated. Load failure ⇒ log warn and continue with pure AEC3 (soft dependency, never block audio on model availability).

### D. Reference-based soft suppression (classic DSP) — documented, not scheduled

Coherence/Wiener-style spectral suppression scaled by live mic↔reference correlation (continuous attenuation instead of my failed binary gating).

Pros: deterministic, no model, no dataset. Cons: weak against nonlinear speaker paths (the dominant real-world failure), heavy hand-tuning per room class, strictly dominated by C once a model works. Kept here so an implementer knows it was considered and why it lost.

### Rejected by operator

- **Half-duplex / send gating / auto-mute during local media** — implemented 2026-08-23, reverted same day. Never reintroduce anything that suspends or degrades the VC send path automatically. PTT remains a user choice, not a system behavior.

## Independent quick win (needs operator sign-off, not bundled)

- **Suspend AGC2 while local clips play** (restore afterwards). Field-proven: AGC2 pumps +19 dB on clip-gap residue; suspending it during *media playback only* does not alter normal conversation. It was entangled with the rejected half-duplex work and reverted with it; it is safe standalone but deserves an explicit yes because it does modify the user's transmitted level during clips.

## Build order

```
0. Branch: cut from main AFTER the clip-card PR lands (operator will PR it separately).
1. a-regression-harness  — land the ERLE test FIRST so every later step is measurable.
   Baseline expectation on current v1.3: ~13 dB (broadband synthetic loopback).
2. a-upgrade-engine + a-delay-hints
   Gate: harness ERLE ≥ 25 dB on the same harness; full check-full.sh green;
   manual echo matrix (below) noticeably improved.
3. b-vpio-macos (parallel-friendly, isolated to capture_coreaudio.cpp)
   Gate: Mac matrix clean including AirPods; Windows untouched.
4. c-model-pick → c-integration → c-validation (flag-off rollout)
5. docs: rewrite specs/03-LIBMELLO.md §4 and specs/10-AUDIO_PIPELINE.md §4/§5.2
   to describe the landed architecture (per CLAUDE.md golden rule).
```

Steps 1–2 deliver most of the value and are low-risk; step 4 is the long pole and the piece worth prototyping before committing to a model.

## Verification (all steps)

Synthetic (CI, deterministic):
- ERLE harness: `EchoCancellerTest` gains a broadband correlated-loopback test (far-end tone-noise mix → 20 ms-delayed attenuated copy into capture, AGC2 disabled for isolation, measure pre/post RMS over a steady-state window). Assert ≥ threshold set at step 1 baseline ×improvement. Also assert a blind run (no `process_render` calls) stays passthrough — catches render-feed regressions like the chunk-size class of bugs.
- Prove the test fails without the thing it guards (TESTING.md rule): e.g., stub out `process_render` and watch it go red.

Manual matrix (each milestone, logged):
| Output device | Scenario | Pass criterion |
|---|---|---|
| Laptop speakers | Remote peer talks; you stay silent | Far side hears no self-echo |
| Laptop speakers | Clip plays in VC | Far side hears no clip |
| Bluetooth (AirPods-class) | Both scenarios | Same, incl. mid-session connect/disconnect |
| Headset | Double-talk over loud clip | Your speech transmits naturally (full duplex intact) |
Any device | Voice latency | Median mouth-to-ear < 50 ms (perf harness), no sustained-underrun warnings |

Instrumentation during field validation: temporary `clip gate:`-style INFO logs (raw/post dBFS, corr, cand, vad_prob) are the proven debugging tool here — re-add behind a debug flag while validating, remove before merge unless promoted to permanent diagnostics deliberately.

## Explicitly out of scope

OS loopback capture of other applications' audio (Windows WASAPI loopback idea — superseded by model generalization in C), mic-array beamforming/doa localization, enforcing PTT, Windows Voice Capture DSP (evaluated: weak), any send-path gating or half-duplex (operator decision above), iOS (separate port track).
