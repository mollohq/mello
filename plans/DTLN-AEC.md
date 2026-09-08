---
name: DTLN-AEC neural residual suppression
overview: "Two-input (post-AEC mic + far-end reference) neural suppressor for echo residue that classical AEC cannot explain. ONNX model beside the Silero VAD session, post-AEC in the capture path, settings flag default off, strict size/latency budgets."
isProject: false
---

# DTLN-AEC Neural Residual Suppression

Branch: `feat/dtln-echo-suppression` (from `feat/echo-cancellation-improvements` post-merge).

## Size baseline (before, macOS arm64, debug profile, 2026-09-05)

| Artifact | Size (KiB) | Notes |
|---|---|---|
| `target/debug/mello` | 289068 | debug client binary, symbols included |
| `libmello/build/libmello.a` | 23648 | static lib, debug |
| `libonnxruntime.1.23.2.dylib` | 34316 | already shipped for Silero VAD |
| `libmello/models/silero_vad.onnx` | 2276 | existing model |

Budgets for the addition: model ≤6 MB installer, steady-state ≤20 MB RAM, inference ≤10 ms per 20 ms frame on min-spec Windows CPU, mouth-to-ear median <50 ms. Deltas are measured same-profile after integration (table below gets an "after" column).

## Size after (macOS arm64, debug profile, same method)

| Artifact | Before (KiB) | After (KiB) | Delta |
|---|---|---|---|
| `target/debug/mello` | 289068 | 289116 | +48 (settings/UI/FFI; model ships beside, not linked in) |
| `libmello/build/libmello.a` | 23648 | 25564 | +1916 (suppressor + ORT loader, debug info included) |
| new model `echo_suppressor.onnx` | — | 5080 | +5080 (5.0 MB, inside the 6 MB installer budget) |
| `libonnxruntime` dylib | 34316 | 34316 | +0 (already shipped for VAD) |

Timing: 0.107 ms pure inference (Python/ORT reference); ~1.6 ms per
20 ms frame all-in (resample + DFT + inference + upsample) on M3 Max.
Windows min-spec numbers still open (handoff task). RAM steady-state
unmeasured — same task.

## Model selection (decided 2026-09-06)

**Pick: Microsoft AEC-Challenge 2022 GRU baseline via the PINTO0309 ONNX
conversion** (`dec-baseline-model-icassp2022.onnx`, Apache-2.0 conversion of
MIT-licensed baseline code; attribute both).

Why not DTLN-aec_128 despite the branch name: 16 kHz with 32 ms frames and
8 ms hops fits our 48 kHz / 20 ms pipeline badly (reframe + resample +
32 ms algorithmic latency breaks the ≤1-frame-lookahead rule), the pair is
7.3 MB fp32 (over budget without quantization), and conversion needs a TF
toolchain absent here. The GRU baseline runs one inference per 20 ms
frame: downsample 960 → 320, sqrt-Hann + rfft-320 front-end, mask apply,
irfft, upsample back. States (2×[1,1,322]) persist across frames.

Measured (Python/ORT, Mac arm64): echo-only ERLE 66.7 dB synthetic and
74.7 dB on real AEC-Challenge far-end singletalk (matches the shipped
dtln_aec_512 reference energy); near-end singletalk −4.1 dB (bounded
over-suppression); doubletalk output-vs-farend correlation 0.001;
0.107 ms per inference against a 10 ms budget. File is 5.0 MB fp32,
inside the 6 MB installer budget with no quantization.

C++ chain status: broadband ERLE 84 dB, real far-end singletalk −36 dB,
real near-end passthrough −0.0 dB, real doubletalk −12 dB, speech-like
synthetic doubletalk −10.5 dB. The Python-parity gap on synthetic tones
(−4.6 vs −10.5 dB) survives analysis-window, warmup, delay-search, and
float64-front-end experiments; it smells like resampling-distribution
shift plus recurrent trajectory sensitivity on pathological steady
inputs. It is tracked, not blocking: the flag ships off, real-speech
behavior is strong, and MOS (not energy parity) gates default-on.

Rejected: TheStageAI `dtln-aec` (encrypted, subscription), `joint_aec_ns`
(ideal shape and MIT, but ships no pretrained weights), DeepFilterNet
family (denoise-first, no echo-conditioned variant evaluated),
DTLN-aec_256/512 (15–41 MB, no chance).
