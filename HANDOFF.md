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
| `feat/stream-twcc-phase1` | ✅ | ✅ | **Phase 1 done** |
| `feat/stream-quality-phase2` | ✅ commits + **UNCOMMITTED ULPFEC work** | branch exists, no unique commits | **Phase 2 in progress** |

`feat/stream-twcc-phase1` was cut from phase0; `feat/stream-quality-phase2` from phase1. Continue phase 2 on `feat/stream-quality-phase2`.

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

### Phase 2 (in progress) — encoder/decode quality
Committed on `feat/stream-quality-phase2` (mello):
- Continuous jitter regulator (replaced one-shot `jitter_primed_` latch) in `video_pipeline.cpp` — `jitter_should_present(depth, now)`: present at ring depth ≥ 2, else at ~90% frame interval.
- Spec §7.7 backlog guard in `mello-core/src/client/stream_ffi.rs`: drop delta AUs while decode input queue > 4; keyframes always feed; `backlog_guard_drops` in cadence log.
- Async decode thread in `video_pipeline`: `feed_packet` is O(copy); decode thread consumes a bounded job queue (cap 8, shed oldest non-keyframe); `decode_queue_depth()` now reports the INPUT backlog; present path uses `decoded_ring_depth()`; thread joined before decoder shutdown; `frames_decoded_`/`decode_errors_` atomic.
- Encoder quality batch (Windows files, needs Windows validation): NVENC High profile + BT.709 limited VUI + temporal AQ + full-res two-pass (`rcParams.multiPass`); AMF High + peak-constrained VBR 1.25×; QSV High + BALANCED + 2 refs; VideoToolbox High 4.2; ladder Medium 4→5 Mbps, Low 2.5→3 Mbps.
- DComp `OpenSharedResource1` caching (`client/src/dcomp_presenter.rs`).
- Test housekeeping: neteq-aware `test_jitter_buffer.cpp` rewrite; `test_video_pipeline.cpp` skips under `CI`.

**UNCOMMITTED in the mello tree right now: the ULPFEC implementation (see §3).**

Roadmap doc: `mello/plans/STREAM-QUALITY-ROADMAP.md` (kept current — update it when you finish items).

---

## 3. IN-FLIGHT: ULPFEC (RFC 5109 XOR parity) — finish this first

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

### Current E2E status: 29/30 — ONE AU incomplete, ONE PLI

Last observed dump:
```
popped=29 dropped=63 | tx fec=69 rtp=820 rtx_req=52 rtx_sent=52 |
rx ingress=706 fec_rec=62 fec_unrec=468 nack_seq=52 core_acc=698
core_missing=57 core_repaired=56 core_complete=29 core_incomplete=1
core_emitted=29 aus_dropped=0 pli=1 restarts=0 inv_rtp=0 inv_h264=0 dup=0
late=0 gate_entries=2 gate_dropped=0
```
Sorted-timestamp analysis shows the missing AU is the LAST one (index 29, seqs ~648–670). Everything else repairs perfectly: 62 FEC recoveries, 56 core repairs, zero output drops, zero gates except one PLI.

**Hypotheses (check in this order):**
1. **Eager-repair give-up bug**: `try_eager_fec_recovery` runs ONLY on FEC-packet arrivals. If `recover()` is attempted before all 9 sibling members of a group have arrived, it fails (`missing > 1` → unrecoverable++) and only retries when the NEXT FEC packet arrives. After the LAST FEC packet (group 67), no more FEC arrivals happen — a seq needing a late retry is stranded until the NACK path tries `recover` (which DOES retry, since NACKs re-fire on the worker tick). But NACK only fires for seqs marked missing — check whether the missing seq was (a) ever registered in `missing_` and (b) whether the NACK-path `recover` succeeded but the injection came after the AU already expired. Likely fix: run `try_eager_fec_recovery` ALSO on the worker tick (e.g., every 1ms tick when `fec_recovery` has uncovered seqs), not just on FEC arrivals. Keep it cheap (only when `uncovered_mask_sequences()` non-empty).
2. **`unrecoverable` counter is misleading** (468): `recover()` increments it whenever a group has >1 missing at ATTEMPT time, including the benign "siblings haven't arrived yet" case. Split into `unrecoverable` (2+ truly lost — permanent) vs `pending` (not all members present — retryable). Don't gate logic on the counter, just fix the stat semantics.
3. If the missing seq is the AU's marker (last fragment) and NOT the group's highest mask index: the marker can't be recovered by level-0 FEC → that AU can never complete via marker boundary (and for the LAST AU there's no next-timestamp boundary). If this is the case, the correct product behavior is: accept the PLI (it's exactly what PLI is for) and relax the test to `popped >= 29 && rx_fec_recovered > 0 && aus_dropped == 0 && pli <= 1` WITH A COMMENT explaining the boundary. Only do this after ruling out (1) and (2) — the intended bar is full repair for ≤1 loss/group.

**Debug path:** print the unrepaired seq (add a getter to `UlpfecRecovery` that lists uncovered seqs at test end, or dump `missing_` contents via a test-only accessor on `RtpH264Receiver`). Confirm which of the 3 hypotheses it is, fix, keep `popped == 30 && pli == 0` if at all achievable.

### CRITICAL merge blocker: SFU-mode FEC is broken without an SFU change

Pion's `TrackLocalStaticRTP.writeRTP` rewrites `Header.SSRC` and `Header.PayloadType` to the binding's values **unconditionally** (`track_local_static.go:203-204`). On the current SFU, FEC packets leave toward viewers as **PT 96, SSRC = leg media SSRC** (not PT 127, not +1) — the viewer's FEC routing check (`ssrc == media+1 && PT == 127`) can never match, and the FEC packet lands in the H.264 core as garbage.

**Required SFU change (mello-sfu):** add a parallel FEC track per viewer.
- On viewer leg creation (`PrepareViewerVideo`/peer stream video wiring): negotiate a second `TrackLocalStaticRTP` with `webrtc.RTPCodecCapability{MimeType: "video/ulpfec", ClockRate: 90000}` PT 127 and SSRC = viewer leg SSRC + 1. MediaEngine must also register the ulpfec codec PT 127 (no RTCP feedback needed on it) so answers include it.
- In the fanout (`fanOutVideoRTP` + `writeViewerVideoRTP`/`SendStreamVideoRTPFor`): route packets with `pkt.PayloadType == 127` to the viewer's FEC track; media to the video track. Per-viewer pacing applies to both (share the pacer budget).
- The viewer's mello-side routing (`ssrc == remote_media_ssrc + 1 && PT == 127`) then works unchanged in SFU mode (host stamps FEC SSRC=media+1; SFU rewrites to legSSRC+1 — the invariant holds per hop).
- Update `mello-sfu/01-SFU.md §6.1` (FEC track contract) and the SDP/negotiation tests. Check the m-line count implications: a second m-line per viewer or same m-line with a second PT — prefer same m-line second PT if Pion's TrackLocalStaticRTP binding allows per-PT writes without a second track (it does NOT: one PT per binding — so it must be a second m-line/track; verify with a Pion-level test in `mello-sfu/internal/server` similar to the existing negotiation tests).
- Add a Go test: viewer leg gets FEC packets with PT preserved 127 and SSRC = legSSRC+1.

P2P mode (max 5 viewers) works without this — but P2P is the fallback; SFU is the primary topology. Do not merge the ULPFEC commit series without the SFU part.

### Telemetry for ULPFEC
`tx_fec_packets_sent`, `rx_fec_recovered`, `rx_fec_unrecoverable` already flow into `MelloRtpVideoStats` + Rust `RtpVideoStats`. Wire the viewer-side fields into the mello-core viewer probe/debug log (`log_viewer_native_stats` in `mello-core/src/client/streaming.rs`) if not already — check and add.

---

## 4. Remaining Phase 2 items (after ULPFEC lands)

- **2.4 D3D11VA decoder** (`libmello/src/video/decoder_d3d11va.cpp`): currently a stub that submits the whole AU as one bitstream buffer with no `DXVA_PicParams_H264`, no IQ matrix, no slice headers — cannot produce correct output; Intel-iGPU viewers fall to OpenH264 CPU decode. Implement properly (NAL/slice parsing → `DXVA_PicParams_H264` + `DXVA_Qmatrix_H264` + slice buffers per frame; reference frame management via the 4-slot decode texture array — currently always copies slice 0). Windows-only, can't verify locally → keep in a separate PR built+tested on the Windows box. Effort: large.
- **Deferred follow-ups already noted**: cross-device sync between libmello write device and DComp presenter (keyed mutex or fence — current code relies on D3D11 queue timing); NVENC two-pass load watch on low-end GPUs (`encode_ms` in certification); chronic-queue viewer ejection (rescue implemented instead — re-evaluate after field data).

---

## 5. Phase 3 — experience features (each gets its own planning round; do NOT start without a plan)

1. **Game audio**: WASAPI loopback capture (eRender + `AUDCLNT_STREAMFLAGS_LOOPBACK` — `libmello/src/audio/capture_wasapi.cpp` is mic-only today) → Opus → audio track on stream sessions (libmello has Opus + voice pipeline to crib from) → SFU stream-audio relay (new track type on stream sessions) → viewer playback. `mello_stream_start_audio`/`mello_stream_feed_audio_packet` are stubs (`mello.cpp:1116,1227`). macOS SCK already captures audio (`capture_screencapturekit.mm:236-241`).
2. **Two-rendition ladder → simulcast**: NVENC dual-session (good/degraded) per host; SFU layer-based forwarding per viewer BWE (the per-viewer GCC from Phase 1 is the input). Check Pion simulcast helpers (`ConfigureSimulcastExtensionHeaders` exists in pion/webrtc v4.2.9).
3. **AV1 activation**: `supports_av1` hardcoded `false` (`mello-core/src/client/streaming.rs:879`), codec hardcoded H264 (`:872`); NVENC AV1 branch + dav1d decoder exist. Wire capability exchange end-to-end.
4. **macOS polish**: VT decoder session reuse on IDR (currently recreates per keyframe — `decoder_videotoolbox.mm:240-245`), native-surface present path (CA layer) instead of CPU RGBA FrameSlot.

---

## 6. Verification gates (run all before reporting done)

```bash
# libmello (from /Users/bob/dev/m3llo-dev/mello/libmello)
cmake --build build -j 8 2>&1 | tail -4
./build/tests/mello_rtp_tests 2>&1 | tail -3          # 70 tests (currently 69 pass + 1 FEC E2E failing — your job §3)
CI=true ctest --test-dir build 2>&1 | grep 'tests passed'   # 100% of 96

# mello-core (from /Users/bob/dev/m3llo-dev/mello)
cargo fmt --all -- --check
cargo clippy -p mello-core --all-targets -- -D warnings
CI=true cargo test -p mello-core --lib                # 228+ tests

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
