# Stream Quality Roadmap — Path to Discord-Class Streaming

> **Status:** Phase 0 ✅ (branch `feat/stream-quality-phase0`, both repos) · Phase 1 ✅ (branch `feat/stream-twcc-phase1`, both repos) · Phase 2 in progress.
> **Origin:** Senior WebRTC review of `mello/` client stack + `mello-sfu/` (July 2026).
> **Specs touched:** 12-STREAMING, 14-VIDEO-PIPELINE, mello-sfu/01-SFU (updated per phase).

## Phase 0 — Correctness ✅

All items landed (see branch commits): paced RTX queue replacing unpaced libdatachannel retransmits, per-packet leaky-bucket pacing, receiver stall(120ms)/hard-cap(600ms) AU deadlines, `timeBeginPeriod(1)` while streaming, host REMB hold-on-empty, stream-path RTT ping, SFU per-viewer PLI + periodic min-REMB recompute, NVENC VBV 0.5s max-rate + full-config reconfigure + measured stats, WGC accumulator throttle, separate keyframe cooldown clocks.

## Phase 1 — TWCC/GCC ✅

- libmello `transport/twcc.{hpp,cpp}`: TWCC stamper (in-place RTX re-stamp), RFC 8888 parser, receiver feedback generator (50ms), GCC-style delay-gradient estimator (accumulated-delay trendline + overuse + AIMD + loss cap).
- Sender: pacer stamps at emit; `min(manager, estimator)` pacing; `GCC_TARGET` feedback to Rust (per-viewer map, GCC > REMB, immediate application).
- Receiver: TWCC seq extraction + feedback emission; RTT-adaptive NACK budget (2–8 by measured RTT).
- SDP: extmap id=3 + `transport-cc`, per-leg negotiation detection.
- SFU: `transport-cc` negotiated; host-leg feedback generation (Pion); per-viewer `gcc.SendSideBWE` from viewer TWCC; token-bucket egress pacer per viewer (~10ms burst bound); per-viewer estimates supersede client REMB in min-REMB synthesis; late-join IDR-AU cache replayed to new viewers; queue-full rescue (>5s backlog → drop + upstream PLI); viewer queue 256→64; RTCP read 8KiB.

Deferred from Phase 1 (follow-ups, not blockers): chronic-queue viewer *ejection* (rescue implemented instead), SFU pacing driven off host-leg measurements.

## Phase 2 — Encoder/decode quality (in progress)

**Exit criteria:** ULPFEC E2E through SFU (host ingress + viewer egress) green in unit tests; libmello/Go/Rust gates pass; Windows visual validation deferred items (D3D11VA, DComp sync) tracked separately.

Done so far (branch `feat/stream-quality-phase2`):
- ✅ Viewer cadence trio: continuous jitter regulator (replaces one-shot `jitter_primed_` latch), spec §7.7 backlog guard (delta-drop > 4 decode-queue depth, keyframes always feed), async decode thread (feed_packet O(copy); `decode_queue_depth()` now reports decode input backlog).
- ✅ Encoder quality batch: NVENC High profile + BT.709 VUI + temporal AQ + full-res two-pass (watch encode_ms on low-end GPUs in certification), AMF High + peak-constrained VBR 1.25×, QSV High + BALANCED + 2 refs, VideoToolbox High; Medium 4→5 Mbps, Low 2.5→3 Mbps.
- ✅ DComp `OpenSharedResource1` caching (stable handle per stream).
- Follow-up for Windows validation: cross-device sync between libmello write device and DComp presenter device (keyed mutex or fence — current code relies on D3D11 queue timing).

Open:
1. H.264 High profile + VUI BT.709 signalling (all encoder backends). ✅
2. `enableTemporalAQ`, multipass evaluation (runtime-gated on GPU headroom). ✅ (headroom gate = validation item)
3. AMF: VBR with 1.25× headroom (currently CBR peak=target). QSV: balanced usage + 2 refs. ✅
4. D3D11VA decoder: proper implementation (pic params + NAL/slice parsing) — un-cripples Intel-iGPU viewers. **Deferred: large Windows-only effort, no local verification; candidates for its own focused PR on the Windows box.**
5. ULPFEC (RFC 5109) at low loss rates, sender + receiver + SFU parallel FEC track (PT 127, SSRC leg+1). ✅ (host FEC-FR SDP + SFU host ingress; mello-sfu Phase 1 TWCC/GCC merged into phase2)
6. Viewer cadence: continuous PID-paced jitter buffer (replaces one-shot `jitter_primed_`), async decode thread, spec §7.7 backlog guard. ✅
7. DComp: cache `OpenSharedResource1`, keyed-mutex/fence sync with libmello device. ✅ (sync → validation follow-up)
8. Bitrate ladder retune (Medium 720p60 uplift) — config only. ✅

## Verification constraints

- Dev machine is macOS. Locally verifiable: transport C++ (`src/transport/*`, platform-neutral), mello-core Rust, mello-sfu Go.
- Windows-only files (`encoder_nvenc.cpp`, `capture_dxgi.cpp`, `capture_wgc.cpp`, `dcomp_presenter.rs`) cannot be compile-checked locally — diffs there are kept minimal/mechanical and validated on the Windows box per `12-STREAMING.md §15`.

## Phase 0 — Correctness ✅ (done)

| # | Fix | Files | Evidence |
|---|-----|-------|----------|
| 0.1 | NACK retransmit path: replace libdatachannel `RtcpNackResponder` with custom caching responder that pushes retransmits into a priority queue drained by the pacer worker (budget-accounted). Fixes retransmit-dropped + fresh-AU-dropped collision in single-slot batch. | `libmello/src/transport/rtp_video_sender.{hpp,cpp}` | `capture_batch` throws at :357; worker `discard_batch` at :515 |
| 0.5 | Packet-level pacing: drain batches with inter-packet spacing instead of whole-AU bursts; credit-based wait anchored at send start. | same | `pace_batch` :417-449 sends all fragments back-to-back |
| 0.4 | High-res timing while streaming: `timeBeginPeriod(1)` scoped to active stream host/viewer (paired with `timeEndPeriod`). | `libmello/src/mello.cpp`, mello-core viewer start | no `timeBeginPeriod` in repo; pacer sleeps quantize to ~15.6 ms |
| 0.7a | Host REMB: on empty fresh-map, hold last target instead of restoring max bitrate. | `mello-core/src/stream/manager.rs` | :259-263 restore-max on empty map (yo-yo) |
| 0.9 | Stream-path RTT: periodic `send_ping` on stream control channel (host + viewer). | `mello-core/src/client/streaming.rs`, `stream/manager.rs` | `rtt_ms` always 0 on stream path |
| 0.7b | SFU: recompute min-REMB periodically (not only on fresh arrival); per-viewer PLI coalescing instead of one global `lastPLI`. | `mello-sfu/internal/server/stream_session.go` | :779-801 |
| 0.2 | NVENC VBV: `vbv = max(avg/2, 3 frames of max)`, set `vbvInitialDelay = vbv/2`. | `libmello/src/video/encoder_nvenc.cpp` | :155 `vbv = avg/fps` (~17 KB at 8 Mbps) |
| 0.3 | NVENC reconfigure: preserve full preset-derived config, check return status, IDR only on down-steps > 25%. | same | :407-442 zeroed config, ignored return, IDR every change |
| 0.6 | WGC throttle: accept at most `target_fps` (same deadline logic as DXGI). | `libmello/src/video/capture_wgc.cpp` | :73 `(void)target_fps` |
| 0.8 | Encoder stats: measured fps/bitrate instead of echoing config. | `encoder_nvenc.cpp` | :390-391 |

Exit gate: libmello ctest green (incl. new NACK/pacer unit tests), `CI=true cargo test --workspace` green, `go test ./...` green in mello-sfu, specs updated.

## Phase 1 — Congestion-control modernization ✅ (done)

1. **Spike:** verify libdatachannel 0.24 extmap/SDP handling for custom RTP header extensions (decides TWCC stamping point).
2. **Host TWCC:** transport-wide sequence header extension stamped on egress (custom MediaHandler) + TWCC RTCP feedback parser.
3. **Host GCC estimator:** trendline delay-gradient + AIMD rate controller; REMB kept as cap/fallback. Replaces `5 %/s` ramp as primary signal.
4. **Host true packet pacer:** budget + burst allowance + probing (replaces batch pacer).
5. **SFU host-leg:** negotiate `transport-cc`, Pion TWCC feedback generator on host-facing receiver.
6. **Viewer TWCC:** feedback generator in `rtp_video_receiver_session`.
7. **SFU per-viewer:** TWCC consumption per viewer leg, per-viewer egress pacer, queue shrink 256→~64, synthesized min-REMB to host from per-viewer estimates (keeps existing host plumbing).
8. **Loss recovery tuning:** RTT-adaptive NACK retry, NACK cache TTL, per-viewer PLI (done in 0.7b), IDR/GOP cache for late-join fast-start, RTCP read buffer 1500→8 KiB, chronic-queue viewer ejection.

## Phase 2 — Encoder/decode quality

1. H.264 High profile + VUI BT.709 signalling (all encoder backends).
2. `enableTemporalAQ`, multipass evaluation (runtime-gated on GPU headroom).
3. AMF: VBR with 1.25× headroom (currently CBR peak=target). QSV: balanced usage + 2 refs.
4. D3D11VA decoder: proper implementation (pic params + NAL/slice parsing) — un-cripples Intel-iGPU viewers.
5. ULPFEC (RFC 5109) at low loss rates, sender + receiver + SFU parallel FEC track. ✅
6. Viewer cadence: continuous PID-paced jitter buffer (replaces one-shot `jitter_primed_`), async decode thread, spec §7.7 backlog guard.
7. DComp: cache `OpenSharedResource1`, keyed-mutex/fence sync with libmello device.
8. Bitrate ladder retune (Medium 720p60 uplift) — config only.

## Phase 3 — Experience features (each gets its own planning round)

1. Game audio: WASAPI loopback capture → Opus → stream audio track → SFU stream-audio relay → viewer playback.
2. Two-rendition ladder (NVENC dual session) → SFU simulcast forwarding.
3. AV1 activation (`supports_av1` hardcoded false at `streaming.rs:879`; NVENC AV1 + dav1d exist).
4. macOS: VT session reuse on IDR, native-surface present path.

## Process

- One branch per phase per repo; conventional commits; `cargo fmt`, `clippy -D warnings`, `CI=true cargo test --workspace`, libmello ctest, `go test ./...` green before PR.
- Spec updates land in the same commit as behavior changes (12-STREAMING §3.4/§14, 14 §6.1, mello-sfu 01-SFU §6.1).
- No new dependencies without asking. No PR opened without approval.
