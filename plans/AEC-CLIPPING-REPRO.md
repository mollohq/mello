---
name: AEC clipping — reproduce then fix
overview: "Alpha users hear heavy 'clipping' with echo cancellation ON that vanishes when they turn EC OFF. Leading hypothesis: AGC2 pumps AEC3 residue up during far-end gaps (field-measured +19 dB). Reproduce it RED first (deterministic ctest), then fix, then confirm GREEN. Follow-up to ECHO-CANCELLATION-IMPROVEMENTS.md."
todos:
  - id: repro-log
    content: "Level 0: capture an `aec` debug log from a real clipping session (capture/render frame skew, pre/post RMS ratio in far-end gaps)"
    status: pending
  - id: repro-red
    content: "Level 1: RED ctest in test_echo_canceller.cpp reproducing AGC2 residue pumping in far-end gaps (EC on vs EC off asymmetry). Must fail on current code."
    status: completed
  - id: repro-realistic
    content: "Level 2: offline real-speech double-talk harness -> before/after WAV for subjective A/B"
    status: pending
  - id: decision-gate
    content: "Only implement a fix once a test goes RED. If none reproduce, pivot using the Level 0 log."
    status: pending
  - id: fix
    content: "Constrain AGC2 pumping (leading) + continuous render feed + live delay hint. Each tied to the test it turns green."
    status: pending
  - id: spec
    content: "On landing, amend specs/10-AUDIO_PIPELINE.md §5.2 (render-feed continuity) + §4 (AGC2 behavior). specs/ is shipped-only."
    status: pending
isProject: false
---

# AEC Clipping — Reproduce Then Fix

Branch: cut a new branch from the current `feat/echo-cancellation-improvements` line when work starts.

**Parent:** [ECHO-CANCELLATION-IMPROVEMENTS.md](./ECHO-CANCELLATION-IMPROVEMENTS.md). That plan
upgraded the engine (v1.3→v2.x/M131), added delay hints, added macOS VPIO, and integrated the
DTLN-AEC neural suppressor (flag-off). This plan handles the **residual clipping** that survived
that work.

## Problem statement

Alpha users report heavy "clipping" (audio breakup / pumping) with echo cancellation **ON**.
Turning EC **OFF** removes the clipping but returns audible echo. User `ostkatt`: incredibly
heavy clipping with EC on; with EC off the operator could hear their own voice through
`ostkatt`'s mic, even though `ostkatt` is on a headset.

The bug predates the DTLN-AEC work. The neural suppressor defaults **off**
([client/src/settings.rs:84](../client/src/settings.rs)) and only runs on the software path
([libmello/src/audio/audio_pipeline.cpp:541](../libmello/src/audio/audio_pipeline.cpp)), so it is
**not** the cause. Model choice is a separate workstream and is on hold.

## Leading hypothesis: AGC2 pumps AEC3 residue during far-end gaps

Field-measured fact from the parent plan
([ECHO-CANCELLATION-IMPROVEMENTS.md:46](./ECHO-CANCELLATION-IMPROVEMENTS.md)): AGC2 adaptive gain
blasts echo residue up to **+19 dB** above raw mic level on intermittent audio
(raw −37.7 dBFS → post −18.5 dBFS).

This explains the operator's hard fact that clipping disappears with EC off:

| Setting | Mic content in far-end gap | AGC2 reaction | Result |
|---|---|---|---|
| EC **on** | quiet AEC3 residue | sees a quiet signal → applies large gain | residue blasted up → **clipping/pumping** |
| EC **off** | loud raw echo | sees a loud signal → applies little gain | raw echo audible, but **no pumping** |

Note: with EC off, AGC2 still runs — `process_capture` returns only when **both** AEC and AGC are
off ([libmello/src/audio/echo_canceller.cpp:116](../libmello/src/audio/echo_canceller.cpp)). So the
fact that EC-off removes the clipping points at the **AEC3+AGC2 interaction on residue**, not AGC2
alone and not the mic gain (the input-volume slider only attenuates, 0.0–1.0).

## Secondary contributors (test each, do not assume)

1. **Render reference fed only during far-end talkspurts.** `mix_output` calls `process_render`
   only when `any_remote || has_clip_audio`
   ([libmello/src/audio/audio_pipeline.cpp:932](../libmello/src/audio/audio_pipeline.cpp)); the feed
   is also AEC-gated ([echo_canceller.cpp:157](../libmello/src/audio/echo_canceller.cpp)). AEC3
   wants a continuous render stream aligned 1:1 with capture. Bursty feeding forces re-convergence
   on each talkspurt, leaving more residue for AGC2 to pump. **This contradicts spec
   10-AUDIO_PIPELINE.md §5.2** ("feed when far-end audio exists") — the spec is silent on AEC3's
   continuous-render contract. A spec change is required if this fix lands.
2. **Stale, coarse delay hint.** `refresh_stream_delay_hint` runs only on init/device switch
   ([audio_pipeline.cpp:965](../libmello/src/audio/audio_pipeline.cpp)); jitter depth drifts during a
   call → worse alignment → more residue.
3. **Clipped reference.** Multi-peer mixing saturates to int16
   ([audio_pipeline.cpp:819](../libmello/src/audio/audio_pipeline.cpp)) before that buffer becomes
   the AEC reference — a nonlinear reference degrades the linear filter.

## Reproduction ladder — RED first, deterministic first

**Operator directive:** reproduce before implementing. Do not write a fix until a test is RED.

### Level 0 — field evidence (no code)
One `aec`-tagged debug log from a clipping session (from `ostkatt` or a Windows repro). Read:
- `capture_frames` vs `render_frames` skew
  ([echo_canceller.cpp:144](../libmello/src/audio/echo_canceller.cpp), `:195`).
- `capture: pre_rms post_rms ratio` in far-end gaps
  ([echo_canceller.cpp:149](../libmello/src/audio/echo_canceller.cpp)). A `post/pre` ratio well
  **above** 1.0 during a gap while the operator is quiet is AGC2 pumping, measured directly.

### Level 1 — primary RED test (C++ ctest, deterministic, no hardware)
New case(s) in [libmello/tests/test_echo_canceller.cpp](../libmello/tests/test_echo_canceller.cpp).
The existing harness never covers this: it feeds render every frame (`:171`, `:239`) or never
(`:265`), and always with **AGC off**. Production is intermittent render + **AGC on**.

- `Agc2DoesNotPumpResidueInFarEndGaps` — feed a low-level near-end noise (quiet operator) plus a
  delayed far-end echo, with **AGC on, AEC on**. Drive far-end in bursts, then a gap. Measure output
  level **in the gap**. **RED = gap output pumped well above input** (reproduce the +19 dB class).
  GREEN after the AGC2 fix.
- `NearEndPreservedUnderIntermittentRender` — continuous near-end, render fed burst-then-gap, AGC on.
  Measure near-end preservation after gaps vs a continuous-render control. Guards contributor #1.
- Both must **fail on current code** (TESTING.md rule; prove RED before GREEN).

### Level 2 — realistic subjective harness (offline, uses existing dataset)
Use the injection API — `mello_voice_start_capture_inject` / `inject_capture` /
`feed_packet` / `get_packet` ([libmello/include/mello.h:153](../libmello/include/mello.h)) — with the
LibriSpeech clean + a delayed far-end echo mix. Decode `get_packet` output to `before.wav` /
`after.wav` for A/B listening. Confirms "Discord clarity", not just a moved number.

### Decision gate
- **Level 1 RED → hypothesis holds → implement fix → watch RED→GREEN.**
- **Level 1 stays GREEN → hypothesis wrong → do not implement.** Pivot using the Level 0 log to the
  next suspect (delay hint under real device latency, the 48↔16k resampler) and build its RED test.

### Level 1 RESULT (2026-09-11, macOS arm64) — hypothesis CONFIRMED
`EchoCancellerTest.Agc2DoesNotPumpResidueInFarEndGaps` is RED on current code, deterministic
across runs:

```
[AGC-PUMP] gap-floor gain: EC on=11.64 dB, EC off=0.00 dB, excess=11.64 dB
           (gap_on=0.013445 gap_off=0.003519 floor=0.003519)
```

- EC **off**: gap floor untouched (0.00 dB) — AGC saw loud echo in bursts, kept gain low.
- EC **on**: same floor pumped **+11.64 dB** — AEC cancelled the echo, AGC over-gained the residue.
- Same class as the field +19 dB, and reproduces the exact EC-on/EC-off asymmetry ostkatt reported.
- All 14 pre-existing `EchoCancellerTest` cases still pass; ERLE 23.7 dB aligned / 22.7 dB misaligned.

**Gate passed → proceed to the fix (constrain AGC2 pumping).**

## Fix candidates (NOT yet written — each tied to the test it turns green)

Leading, do first:
1. **Constrain AGC2 pumping.** Root-cause fix, not a band-aid (CLAUDE.md). Full duplex must stay
   intact (binding operator decision, parent plan) — no send gating, no auto-mute.

### AGC2 knob surface (vendored M131, read 2026-09-12)
`gain_controller2.adaptive_digital` in
`libmello/third_party/webrtc-audio-processing/webrtc/api/audio/audio_processing.h` (~line 360):

| Field | Default | Note |
|---|---|---|
| `max_gain_db` | 50.0 | max digital gain — the pumping ceiling |
| `max_output_noise_level_dbfs` | −50.0 | noise gate — did NOT stop our +11.64 dB pump (residue read as speech) |
| `max_gain_change_db_per_second` | 6.0 | ramp speed |
| `headroom_db` | 5.0 | target headroom |
| `initial_gain_db` | 15.0 | start gain |

Current `apply_config` sets only `adaptive_digital.enabled = true`; all above stay default
([echo_canceller.cpp:89](../libmello/src/audio/echo_canceller.cpp)). No config knob gates adaptation
on near-end presence — that VAD is internal to AGC2.

### Two constraints on the fix
- A naive `max_gain_db` cap trades pumping for **under-amplified quiet talkers**. Guard it with a
  new test `QuietNearEndSpeechStillNormalized` BEFORE tuning any knob.
- The −50 dBFS noise gate did not prevent the pump, so config tuning may be insufficient. If no
  config point satisfies both tests, the fix is **structural** (near-end-gated AGC adaptation) and
  needs re-planning before implementation.

### Fix sequence
1. Add `QuietNearEndSpeechStillNormalized` (guard test).
2. Tuning matrix vs both tests: `max_gain_db`, `max_output_noise_level_dbfs`,
   `max_gain_change_db_per_second`. Target: pump excess < 6 dB AND quiet near-end still normalized.
3. If (2) succeeds → config-only fix. If not → structural near-end-gated AGC (re-plan first).

### Step 1 RESULT (2026-09-12, macOS arm64)

Guard test finding: a steady quiet broadband floor at −35 dBFS gets only −0.27 dB — AGC2 treats
synthetic broadband as **noise, not speech**, and does not normalize it. So a unit test cannot
measure quiet-*speech* normalization. The guard was repurposed to `SteadyLowLevelInputNotAmplified`
(assert steady floor stays < +3 dB). Quiet-talker normalization moves to Level 2 (real LibriSpeech).

Tuning matrix (pump excess / steady gain):

| Config | pump excess | steady |
|---|---|---|
| stock | +11.64 dB | −0.27 dB |
| `max_gain_change=1` | +5.56 | +9.77 (bad: slows decay, floor amplified) |
| `max_gain_change=0.5` | +1.67 | +12.24 (bad) |
| `max_gain_db=6` | +3.58 | −0.27 (starves real quiet talkers) |
| `max_output_noise=-70` | +3.34 | −0.27 |
| **`initial_gain_db=0`** | **−2.42** | **−0.27** |

**Root cause:** AGC2's `initial_gain_db=15` default lingers across the far-end burst/gap cycle. AEC
cancels the echo, AGC2 sees near-silence, and the leftover start gain blasts the gap floor. On a
*steady* signal that gain decays to ~0, so the pump is an intermittent artifact.

**Fix applied:** `cfg.gain_controller2.adaptive_digital.initial_gain_db = 0.0f` in
[echo_canceller.cpp](../libmello/src/audio/echo_canceller.cpp) `apply_config`. No `max_gain_db` cap
(genuine near-end speech keeps full adaptive range); steady behavior unchanged.

**Verification:** `EchoCancellerTest.Agc2DoesNotPumpResidueInFarEndGaps` RED→GREEN
(+11.64 → −2.42 dB). All 16 `EchoCancellerTest` cases pass; ERLE unchanged (23.7 / 22.7 dB); full
libmello suite 63 pass / 7 hardware-skip / 0 fail.

### Level 2 RESULT (2026-09-12) — the fix is NOT yet validated against a realistic repro

`EchoCancellerTest.RealSpeechNoFarEndGapPump` drives real LibriSpeech near-end + far-end through
near-talk / far-only cycles, with an ever-present mic floor and (final version) a NONLINEAR
(soft-clip) speaker echo. Far-only residue, stock vs fix:

| Harness variant | STOCK (initial_gain=15) | FIX (initial_gain=0) |
|---|---|---|
| echo-only (no floor) | −59.8 dBFS | −59.7 dBFS |
| + ever-present floor | −49.0 dBFS | −49.0 dBFS |
| + nonlinear speaker echo | −49.1 dBFS | −49.0 dBFS |

**No realistic variant reproduces a pump on stock, and the fix makes no difference there.** Why:
AGC2's adaptive-digital runs after AEC; once real near-end speech trains its speech-level estimator,
the noise estimator (`max_output_noise_level_dbfs`) correctly gates the steady floor, so it is not
pumped. The synthetic `Agc2DoesNotPumpResidueInFarEndGaps` reproduces a pump only in a narrow
regime: the near-end **never talks loudly** (only floor + fully-cancelled echo), so AGC2 never sees
a loud signal, keeps the `initial_gain_db=15` start gain, and blasts the gap. That regime models a
**pure listener on speakers**, not an active talker.

**Honest status of the fix:** `initial_gain_db=0` is a safe hardening (no downside across 17 tests,
ERLE unchanged) and it fixes the listener-regime pump. It is **NOT proven** to fix ostkatt's
reported heavy clipping, because no realistic offline harness reproduces that pump. Earlier
confidence was too high.

**The reproduce-first gate did its job:** do not claim ostkatt is fixed on this evidence.

### Next to actually confirm ostkatt (pick one)
1. **ostkatt's `aec` log / targeted instrumentation** (Level 0) — the real arbiter. If his build lacks
   aec logs, add temporary `clip gate:`-style dBFS/gain logging (parent plan used this) behind a debug
   flag and have him reproduce.
2. **Live/device reproduction** — the field +19 dB was measured live during clip-playback gaps, not
   offline. A device-level repro (real speaker + mic, or the voice-test-client through real I/O) may
   be the only way to trigger the live AGC behavior.
3. Re-examine whether the field pump is the **clip-playback** path specifically (parent plan's
   measured case) rather than remote-voice, and whether the reverted "suspend AGC2 during clips"
   quick-win is the real fix there.

The `RealSpeechNoFarEndGapPump` test stays as a passing sanity floor (realistic speech must not pump),
not a bug reproduction.

Contributors, land as separate commits if their RED test reproduces:
2. **Continuous render feed** (zeros when idle) → `NearEndPreservedUnderIntermittentRender`.
   Requires the spec 10 §5.2 amendment in the same change.
3. **Live delay-hint update** during the call → a drifting-delay ERLE variant.
4. **Float / soft-limited reference** → a clipped-reference ERLE case.

## Platform note (macOS vs Windows)

- **Level 0/1 and the fix run fine here on macOS.** `EchoCanceller` is the cross-platform vendored
  WebRTC APM; the unit test drives it directly and bypasses VPIO. Existing ERLE baselines were
  measured on macOS arm64 ([test_echo_canceller.cpp:195](../libmello/tests/test_echo_canceller.cpp)).
- **Windows is required only for final live validation** of the software AEC path, because macOS
  with EC on takes the VPIO backend and skips the software APM
  ([audio_pipeline.cpp:498](../libmello/src/audio/audio_pipeline.cpp)). VPIO is not part of this bug.

## Out of scope

- Neural model swap (DTLN-AEC → GTCRN / DeepFilterNet / DeepVQE). On hold until clipping is fixed
  and field ERLE is re-measured.
- macOS VPIO path (works well per operator).
- Any half-duplex / send gating / auto-mute (binding operator decision, parent plan).

## Verification

- `./scripts/check-full.sh` (adds libmello ctest) green.
- Level 1 tests proven RED before the fix, GREEN after.
- Manual echo matrix from the parent plan
  ([ECHO-CANCELLATION-IMPROVEMENTS.md:150](./ECHO-CANCELLATION-IMPROVEMENTS.md)), re-run on Windows.
- Median mouth-to-ear < 50 ms, no sustained-underrun warnings.
