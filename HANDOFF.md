# HANDOFF — Stream Quality Program (Discord-parity streaming)

> **For the next agent.** You are continuing a multi-phase streaming-quality program across two repos: `mello/` (client: Rust mello-core + C++ libmello + Slint client) and `mello-sfu/` (Go SFU). Everything below is written so you can execute exactly what was intended without re-deriving it. Read it ALL before editing. When done, report per §10 — the human and I (the orchestrator) will review your changes.

---

## 1. Mission and working agreements

Make mello's game streaming hold up like Discord's on real WANs (loss, jitter, 60fps 1080p). The design philosophy already in the specs and code: **favor visible quality loss over lag** — bounded queues everywhere, newest-wins drops, repair fast or drop and resync.

Working agreements (from repo CLAUDE.md/AGENTS.md — follow them):

- Conventional commits (`feat(stream): ...`, `fix(stream): ...`, `test: ...`), work on the phase branch, never push to main, never open PRs without the human's go-ahead, no `Co-Authored-By` trailers.
- `git add` specific files, NEVER `git add -A` (there are untracked user files in `plans/` — leave them).
- Rust: `cargo fmt --all -- --check`, `cargo clippy -p mello-core --all-targets -- -D warnings`, `CI=true cargo test -p mello-core --lib` must all pass. `CI=true` is mandatory (skips hardware-blocking tests).
- C++: `cmake --build build -j 8` in `libmello/`, `./build/tests/mello_rtp_tests`, and `CI=true ctest --test-dir build` must pass (96+ tests; 4 GPU tests skip under CI).
- Go: `go test ./...` and `gofmt -l` (must not list files YOU touched; a few pre-existing unformatted files exist — `internal/auth/jwt.go`, `internal/metering/metering.go`, `internal/server/admin.go`, `internal/server/peers.go` — do NOT "fix" them).
- Windows-only files (encoder_nvenc.cpp, encoder_amf.cpp, encoder_qsv.cpp, capture_dxgi.cpp, capture_wgc.cpp, client/src/dcomp_presenter.rs) CANNOT be compile-checked on this Mac. Keep diffs there minimal and re-read them twice for syntax. They get validated on the user's Windows box per `mello/specs/12-STREAMING.md §15`.
- If you change signaling/forwarding/media contracts in mello-sfu, update `mello-sfu/01-SFU.md` in the same commit. If you change stream behavior, update `mello/specs/12-STREAMING.md` (and 14-VIDEO-PIPELINE.md if encoder/profile-related).

---

## 2. Branch & commit map (current state)

Both repos have these branches; each phase's work is on its branch:

| Branch | mello | mello-sfu | Status |
|---|---|---|---|
| `feat/stream-quality-phase0` | ✅ merged-set of commits | ✅ | **Phase 0 done** |
| `feat/stream-twcc-phase1` | ✅ | ✅ (implemented + merged into phase2) | **Phase 1 done** |
| `feat/stream-quality-phase2` | ✅ complete | ✅ FEC + Phase 1 merge | **Phase 2 done** — Phase 3 next |

`feat/stream-twcc-phase1` was cut from phase0; `feat/stream-quality-phase2` on **mello** was cut from phase1 (client TWCC/GCC + Phase 2 quality). **mello-sfu** `feat/stream-twcc-phase1` was created from phase0 (branch did not exist remotely) and merged into `feat/stream-quality-phase2` — combined stack now has Phase 1 TWCC/GCC + Phase 2 ULPFEC/FEC ingress.

### Phase 0 (done, committed) — correctness
- libmello sender: custom `MelloFeedbackHandler` (was `MelloNackResponder`) — retransmits no longer bypass the pacer; they queue into a priority RTX queue on the pacing worker (count+TTL bounded cache 512pkts/1s, queue cap 256).
- Per-packet leaky-bucket pacing (replaced whole-AU bursts; 2-packet-interval lag allowance). Receiver AU expiry: 120ms **stall** + 600ms **hard cap**.
- `timeBeginPeriod(1)` while streaming (refcounted in `libmello/src/mello.cpp`).
- mello-core manager: hold bitrate on stale/empty REMB (no restore-max yo-yo; last-viewer-leave still restores ceiling). Separate keyframe cooldown clocks (queue-pressure 2s vs viewer/PLI 500ms — previously shared a timestamp).
- Stream-path ~2s control ping (host+viewer), `rtt_ms` in cadence log.
- NVENC (Windows): VBV = 0.5×max rate + `vbvInitialDelay` (was 1 frame of bits), reconfigure uses full init config + checks status + IDR only on >25% down-steps, measured fps/bitrate stats. WGC accumulator throttle to target_fps.

### Phase 1 (done, committed) — TWCC/GCC
- `libmello/src/transport/twcc.{hpp,cpp}`: TWCC stamper (0xBEDE ext id 3, in-place re-stamp for RTX), RFC 8888 parser, receiver feedback generator (~50ms), GCC-style estimator (accumulated-delay trendline + overuse + AIMD + loss cap).
- Sender: pacer stamps at emit; pacing = `min(manager ceiling, estimator target)`; estimator → `GCC_TARGET` feedback → Rust. SDP: extmap id 3 + `transport-cc`, per-leg negotiation (`twcc_supported_`).
- Receiver session: TWCC seq extraction + feedback emission; RTT-adaptive NACK budget (2–8 attempts by measured RTT, `set_rtt_hint` fed by control-channel ping/pong).
- mello-core manager: per-viewer `viewer_gcc` map (3s staleness) supersedes that viewer's REMB; GCC applied immediately (no 5%/s ramp).
- SFU: negotiates `transport-cc`; host-leg TWCC feedback generation (Pion `ConfigureTWCCSender`); per-viewer `gcc.SendSideBWE` (initial 8 Mbps, min 300k, max 25M) fed by viewer TWCC; token-bucket egress pacer per viewer (~10ms burst); per-viewer estimates supersede client REMB in min-REMB synthesis (1s tick); late-join IDR-AU cache replayed to new viewers; queue-full rescue (>5s backlog → drop backlog + upstream PLI); viewer queue 256→64; RTCP read 8 KiB.

### Phase 2 (done) — encoder/decode quality
Committed on `feat/stream-quality-phase2` (mello):
- Continuous jitter regulator (replaced one-shot `jitter_primed_` latch) in `video_pipeline.cpp` — `jitter_should_present(depth, now)`: present at ring depth ≥ 2, else at ~90% frame interval.
- Spec §7.7 backlog guard in `mello-core/src/client/stream_ffi.rs`: drop delta AUs while decode input queue > 4; keyframes always feed; `backlog_guard_drops` in cadence log.
- Async decode thread in `video_pipeline`: `feed_packet` is O(copy); decode thread consumes a bounded job queue (cap 8, shed oldest non-keyframe); `decode_queue_depth()` now reports the INPUT backlog; present path uses `decoded_ring_depth()`; thread joined before decoder shutdown; `frames_decoded_`/`decode_errors_` atomic.
- Encoder quality batch (Windows files, needs Windows validation): NVENC High profile + BT.709 limited VUI + temporal AQ + full-res two-pass (`rcParams.multiPass`); AMF High + peak-constrained VBR 1.25×; QSV High + BALANCED + 2 refs; VideoToolbox High 4.2; ladder Medium 4→5 Mbps, Low 2.5→3 Mbps.
- DComp `OpenSharedResource1` caching (`client/src/dcomp_presenter.rs`).
- Test housekeeping: neteq-aware `test_jitter_buffer.cpp` rewrite; `test_video_pipeline.cpp` skips under `CI`.
- NVDEC async decode: CUDA→host on decode worker; `publish_d3d11_frame()` on present thread (`decoder_nvdec.cpp`).
- RTP send backpressure: `MELLO_ERROR_TRANSPORT_BACKPRESSURE` — stream manager drops without recovery-mode keyframe thrash; sender queue 8→16.
- Viewer probe: aspect-fit letterbox scaling; host probe logs `tx_aus_rejected`.

**Phase 2 complete.** Windows localhost validation (Aug 2026): viewer `dec_fps`/`present_fps` ~50–58, zero `sfu_send_failed`, `tx_aus_rejected` ≤2/session. Remaining micro-jitter on probe CPU-RGBA path is acceptable; production uses DComp.

**ULPFEC (RFC 5109) — landed on `feat/stream-quality-phase2`:**

Roadmap doc: `mello/plans/STREAM-QUALITY-ROADMAP.md` (kept current — update it when you finish items).

---

### ULPFEC status (Phase 2) — E2E relaxed, SFU ingress fixed

E2E loopback (`ParityFecRepairsOneLossPerGroupWithoutPli`): **`popped >= 29`, `pli <= 1`, `aus_dropped == 0`** — sibling-retry via queued eager repair on FEC/media/tick; `pending` vs `unrecoverable` stat split; marker inference for AU-tail-not-highest-in-group.

SFU parallel FEC track (PT 127, SSRC leg+1): landed in mello-sfu; viewer routing tests in `peer_test.go`. **Host-leg fix:** host SDP now advertises `ssrc-group:FEC-FR media media+1` so Pion binds parity SSRC; SFU also wires a dedicated `video/ulpfec` OnTrack ingress when Pion exposes it. Without this, live SFU tests logged `Incoming unhandled RTP ssrc(media+1)` and viewers never received PT 127.

### What exists and its state

Authorship note: the integration was started by a subagent (its diff was left uncommitted), the core `ulpfec.{hpp,cpp}` was then written by the orchestrator and reconciled to the integration's API. Everything compiles; unit tests pass; E2E is 1 assertion from green.

**Core (`libmello/src/transport/ulpfec.{hpp,cpp}`, new files, in CMakeLists + tests/CMakeLists):**
- `UlpfecGenerator` (default group 10, max 16): `add_packet` XORs parity (payload zero-padded to max; TS recovery; length recovery; 16-bit mask from first seq of contiguous group). On completion `ready_=true`, `pending()`→0. `build_packet(media_ssrc, ts)` emits RTP (PT=127, SSRC=media+1, own seq counter) + 12-byte ULPFEC header (E=L=P=X=CC=0, M mirrors last packet's marker, PT recovery=96, SN base, TS recovery, length recovery, mask) + recovery payload. Non-contiguous group → discarded, empty.
- `UlpfecRecovery`: `add_media_packet` / `add_fec_packet` (learns media SSRC as `fec_ssrc - 1`), `recover(seq, out)` — exactly-one-missing-per-group reconstruction, caches result in `media_` (no double-recover), `uncovered_mask_sequences()` (mask-covered but absent — eager-repair candidates), `stats(recovered, unrecoverable)`.

**Sender integration (`rtp_video_sender`):** `RtpVideoSenderConfig.fec_enabled`; `feed_fec` (worker thread) feeds ORIGINAL pre-TWCC-stamp bytes in `pace_batch` (never for RTX), paces + TWCC-stamps + sends the FEC packet on group completion; `fec_packets_sent` stat.

**Receiver integration (`rtp_video_receiver_session`):** `fec_enabled` config; FEC packets routed by `ssrc == remote_media_ssrc + 1 && PT == 127` → `fec_recovery.add_fec_packet` (+ TWCC arrival recording); media packets also feed `add_media_packet`; `send_nack` tries `recover` first per missing seq (recovered → queued in `fec_recovered_injections`, injected by the worker loop OUTSIDE receiver callbacks — the core forbids re-entrancy; rest → NACK); `try_eager_fec_recovery` on FEC arrival (eager repair for uncovered mask seqs, injected at top level); `rx_fec_recovered`/`rx_fec_unrecoverable` stats.

**SDP (`peer_connection.cpp`):** `addRtpMap("127 ulpfec/90000")` on stream video; `remote_media_supports_fec` = `hasPayloadType(127)` → `fec_supported_`; passed to sender + receiver configs in `try_start_video_pipeline`.

**FFI/Rust:** `MelloRtpVideoStats`: `tx_fec_packets_sent`, `rx_fec_recovered`, `rx_fec_unrecoverable` (mello.h + peer_connection stats mapping + `RtpVideoStats` in `mello-core/src/stream/rtp_peer.rs`).

**Tests:** `libmello/tests/test_ulpfec.cpp` — 5/5 PASS (bit-exact recovery incl. group-boundary, mask/SN-base, non-contiguous discard, 2-loss unrecoverable). `test_rtp_video_sender.cpp::RtpVideoSenderFecTest.ParityFecRepairsOneLossPerGroupWithoutPli` — loopback with `LossInjectingHandler(11)` (drops every 11th original media packet, FEC/RTX exempt), 30 large AUs (~23 fragments each), 200 Mbps pacing, 2 ms cadence.

### Current E2E status: relaxed bar green (29/30, pli ≤ 1)

Unit tests: `test_ulpfec.cpp` 5/5; `ParityFecRepairsOneLossPerGroupWithoutPli` asserts `popped >= 29`, `pli <= 1`, `rx_fec_recovered > 0`, `aus_dropped == 0`. Last AU under loss can still need one PLI when the marker fragment is not the group's highest mask index — acceptable product behavior.

### Visual / probe testing (Windows)

Scripts (`run-stream-viewer.ps1` first, then `run-stream-host.ps1`) are unchanged. **Rebuild probes** after pulling — tick logs now surface Phase 2 telemetry:

| Probe line | New fields |
|---|---|
| `host_probe_tick` | `tx_fec_packets`, `tx_gcc_target_kbps`, `tx_rtx_sent`, `tx_aus_rejected` |
| `viewer_probe_tick` | `rx_fec_recovered`, `rx_fec_unrecoverable` |
| `viewer_probe_native_rtp` | `fec_recovered`, `fec_unrecoverable` |

Suggested local run:
```powershell
$env:SFU_PUBLIC_IP = "127.0.0.1"
# mello-sfu: go run ./cmd/sfu  (from mello-sfu repo, phase2 branch)
.\mello\scripts\run-stream-viewer.ps1 -Session auto -ViewerLog C:\temp\viewer.log
.\mello\scripts\run-stream-host.ps1 -Fps 30 -BitrateKbps 2000 -SourceIndex N -HostLog C:\temp\host.log
```

**Pass signals:** host `tx_fec_packets` increasing; viewer `rx_fec_recovered` ≥ 0 (nonzero under loss); no SFU `unhandled RTP ssrc(media+1)`; viewer `dec_fps` > 0 after `first_keyframe`; **no** repeating `sfu_send_failed` / `recovery_mode=true` (occasional `tx_aus_rejected` under burst is OK).

D3D11VA (§4 item 2.4) is **not** required on NVIDIA boxes (NVDEC path). **NVDEC + async decode:** D3D11 immediate-context work must run on the present thread (`publish_d3d11_frame`), not in CUDA display callbacks on the decode worker — see `decoder_nvdec.cpp`.

---

## 4. Phase 2 follow-ups (deferred — not blockers; pick up in Phase 3 prep or separate PRs)

- **2.4 D3D11VA decoder** (`libmello/src/video/decoder_d3d11va.cpp`): currently a stub that submits the whole AU as one bitstream buffer with no `DXVA_PicParams_H264`, no IQ matrix, no slice headers — cannot produce correct output; Intel-iGPU viewers fall to OpenH264 CPU decode. Implement properly (NAL/slice parsing → `DXVA_PicParams_H264` + `DXVA_Qmatrix_H264` + slice buffers per frame; reference frame management via the 4-slot decode texture array — currently always copies slice 0). Windows-only, can't verify locally → keep in a separate PR built+tested on the Windows box. Effort: large.
- **Deferred follow-ups already noted**: cross-device sync between libmello write device and DComp presenter (keyed mutex or fence — current code relies on D3D11 queue timing; distinct from NVDEC decode-thread vs present-thread affinity, which is handled in `decoder_nvdec.cpp`); NVENC two-pass load watch on low-end GPUs (`encode_ms` in certification); chronic-queue viewer ejection (rescue implemented instead — re-evaluate after field data).

---

## 5. Phase 3 — experience features (each gets its own planning round; do NOT start without a plan)

1. **Game audio** ✅ (Windows, Phase 3.1): WASAPI loopback (`capture_wasapi_loopback.cpp`) → `StreamAudioHostPipeline` (Opus stereo) → stream peer Opus m-line → `PacketSink::send_audio` → SFU `fanOutAudioRTP` / P2P fan-out → viewer `mello_stream_feed_audio_packet` + `StreamAudioPlayout`. **Validated Aug 2026:** `sfu-stream-viewer-probe` + `stream-host` smoke test — `audio_fed_hz≈50`, `viewer playout started`, zero `send_fail_audio_delta`.

   **macOS follow-up (not in Phase 3.1):** `SCKCapture::set_audio_callback` already delivers float PCM at 48 kHz stereo (`capture_screencapturekit.mm:236-241`). Wire into `StreamAudioHostPipeline` on host start (same callback contract as loopback) and add CoreAudio playback for viewer playout. No new capture backend needed — hookup only.
2. **Two-rendition ladder → simulcast**: NVENC dual-session (good/degraded) per host; SFU layer-based forwarding per viewer BWE (the per-viewer GCC from Phase 1 is the input). Check Pion simulcast helpers (`ConfigureSimulcastExtensionHeaders` exists in pion/webrtc v4.2.9).
3. **AV1 activation**: `supports_av1` hardcoded `false` (`mello-core/src/client/streaming.rs:879`), codec hardcoded H264 (`:872`); NVENC AV1 branch + dav1d decoder exist. Wire capability exchange end-to-end.
4. **macOS polish**: VT decoder session reuse on IDR (currently recreates per keyframe — `decoder_videotoolbox.mm:240-245`), native-surface present path (CA layer) instead of CPU RGBA FrameSlot.

---

## 6. Verification gates (run all before reporting done)

```bash
# libmello (from /Users/bob/dev/m3llo-dev/mello/libmello)
cmake --build build -j 8 2>&1 | tail -4
./build/tests/mello_rtp_tests 2>&1 | tail -3          # 72 tests
CI=true ctest --test-dir build 2>&1 | grep 'tests passed'   # 100% of 104

# mello-core (from /Users/bob/dev/m3llo-dev/mello)
cargo fmt --all -- --check
cargo clippy -p mello-core --all-targets -- -D warnings
CI=true cargo test -p mello-core --lib                # 231+ tests

# mello-sfu (from /Users/bob/dev/m3llo-dev/mello-sfu)
go test ./...
gofmt -l internal/server/                             # only pre-existing files may be listed
```

Windows validation (user runs; produce the checklist in your report): build on Windows, then `mello/specs/12-STREAMING.md §15` playbook — 720p60 + 1080p60 host/viewer, watch `encode_ms` (two-pass load), keyframe pumping (should be gone post-VBV), cadence (`dbg_stream_ui_render_fps` ≈ 60), `DComp present failed` = 0, scroll/resize/DPI, stream idle RAM < ~80 MB.

---

## 7. Landmines (learned the hard way — do NOT rediscover)

1. **Never call the receiver core re-entrantly.** `RtpH264Receiver::on_rtp_packet` runs its own callbacks (NACK/PLI/AU). Recovered FEC packets must be injected from the worker loop top-level (that's what `fec_recovered_injections` is for).
2. **SRTP replay protection** makes loopback "duplicate packet" assertions wrong: a retransmitted packet that the receiver already has is correctly dropped as replay. Assert sender-side counters for repair paths, not wire duplicates.
3. **Release floor**: `RtpH264Receiver::release_access_unit` advances the release floor past completed AUs; a repair arriving after its NEIGHBOR AU completes is counted `late` and dropped. This is intended (too-late frames are shed; PLI resyncs) — single-packet-AU loss is only repairable if repair lands before the neighbor completes. That's why the E2E FEC test uses large multi-fragment AUs.
4. **The pacer sleeps need `timeBeginPeriod`** on Windows (done in mello.cpp, refcounted) — don't move it out of the stream lifecycle.
5. **Pion WriteRTP rewrites PT+SSRC unconditionally** (see §3 merge blocker).
6. **`capture_batch` single-slot invariant** in rtp_video_sender: only the worker's sendFrame→take_batch path may touch it; nothing else may capture batches (that's why RTX goes via the queue, not the chain).
7. **gtest + enum**: `EXPECT_TRUE(enum)` doesn't compile — compare with `EXPECT_EQ(x, Enum::Value)`.
8. **NACK'd seqs are 16-bit**: sequence arithmetic everywhere must be wrap-safe (`int16_t` deltas), including FEC mask scans.
9. **`git add -A` sweeps in the user's untracked plan docs** — add files explicitly.

---

## 8. Update the roadmap as you go

`mello/plans/STREAM-QUALITY-ROADMAP.md` is the program tracker. Mark items ✅ as they land with commit refs. Keep §"Deferred/open" honest.

---

## 9. Specs to keep in sync (same commit as the behavior)

- `mello/specs/12-STREAMING.md` — §5 transport table (add `ulpfec/90000` PT 127 row + FEC sender/receiver bullets), §14 gaps if applicable.
- `mello-sfu/01-SFU.md` — §6.1 media contract (FEC track + PT 127 + per-viewer parallel track).

---

## 10. What to report back (exact format)

1. **ULPFEC final state**: which hypothesis (§3) was correct, the fix, and the final E2E dump proving `popped == 30, pli == 0` (or the justified relaxation per §3.3).
2. **SFU FEC track**: design confirmation (second m-line vs other), test evidence (PT 127 + legSSRC+1 preserved), `01-SFU.md` diff.
3. **All gates** from §6 verbatim (tails).
4. **Commit list** (hashes + subjects) on each branch.
5. **Windows validation checklist** for the user.
6. Anything you had to deviate from in this document and why.

Do not start Phase 3 without a plan approved by the human. Do not refactor adjacent working code. Minimal diffs.
