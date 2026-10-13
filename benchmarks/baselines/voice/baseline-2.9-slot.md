# Voice quality baseline 2.9-slot

> Taken 2026-10-13T21:05:05Z on windows/x86_64 from commit `af8c7f0a1c702773c754999cbcb8064e96c903ab` (dirty tree). This file and `baseline-2.9-slot.json` are never edited. Every later run of `scripts/voice-gate.sh` prints its delta against them.

- Baseline 2.9-slot: the 2.9 audio state (commit af8c7f0, the tree of baseline 2.9) measured with shim version 2 and the extra seeds of home-wifi, mobile and bad.
- Shim version 2 draws the impairments (loss, burst, jitter, reorder) per 20 ms send slot, keyed to the packet's media time, not per packet. baseline-2.9.json is the same audio state with shim version 1 and stays as the record.
- Tree: af8c7f0 with its own harness (tools/voice-gate as of af8c7f0) plus benchmarks/baselines/voice/baseline-2.9-slot-harness.patch: the slot shim, extra_seeds and the seed medians, the 2.9-slot output. The harness of fix/voice-gate-slot-shim needs libmello hooks (packet sink, mello_voice_feed_rtp, send feedback) that af8c7f0 does not have, so the af8c7f0 harness takes the baseline, as it took baseline 2.9. profiles.json is the one of fix/voice-gate-slot-shim. git.dirty is true because of the patch.
- This libmello has no packet timestamp: the harness gives each packet the media time of its frame (frame c / 2 - 1 after chunk c, the pre-roll frames before it). The same rule gives every packet's RTP timestamp on the fix/voice-gate-slot-shim tree.
- Platform: Windows x86-64. The gate enforces its numbers only on the platform of the baseline; elsewhere it reports only.

| Item | Value |
|---|---|
| Scorer | pesq-wb (ITU-T P.862.2), pesq 0.0.4 |
| Inputs fingerprint (profiles.json without gates) | `d3bd006f0d527418` |
| Corpus fingerprint | `ac3221b5ba3b8dc4` |
| Runtime | 114.6 s |
| Method: backend | MELLO_AUDIO_BACKEND=test (no device, no device thread) |
| Method: clock | virtual: mello_test_set_clock_ms, 1 ms steps; receiver pulls 10 ms per step of 10 ms |
| Method: delay | windowed cross-correlation at 4 kHz, reference vs output; latency = playout time - capture time |
| Method: path | mello_voice_inject_capture -> mello_voice_get_packet -> shim (SFU header rewrite, impairments) -> mello_voice_feed_packet -> mello_voice_test_pull_output |
| Method: send_tick | packets leave on a 20 ms tick at sender time 20k+10 ms, like VoiceManager::tick |
| Method: sender | one encode per sender setup, 10 ms capture chunks; profiles replay a prefix of the packet trace |
| Method: shim | version 2: impairment draws per 20 ms send slot (media time of the packet's frame), every stream advanced through every slot; profiles with extra_seeds also run with each extra seed |

Definitions:

- Delay is mouth-to-ear: playout time minus capture time, from windowed
  cross-correlation of the output against the input.
- Start and end are the first and the last pass over the corpus. Growth is
  end minus start, so it compares the same speech.
- In-clip growth is the median over clips of the delay change from the first
  to the last third of the clip.
- Concealment per lost frame counts PLC and FEC frames for losses (network
  loss plus receiver drops). The target is 1.
- A dropout is a 20 ms speech frame that comes out 20 dB too quiet, or does
  not match the input (concealment or wrong audio).

## Summary

| profile | MOS | delay p50 | delay p95 | start | end | growth | in-clip growth | in-clip seeds | recover s | conceal/lost | fill PLC | underruns | dec err | fed | rx late | resets | dropout ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| clean | 3.27 | 262 | 321 | 262 | 262 | 0 | -5 | - | - | - | 1500 | 125 | 0 | 3988 | 0 | 0 | 460 |
| home-wifi | 2.47 | 342 | 562 | 331 | 342 | 10.1 | 69.9 | 90.1 | - | 2 | 1318 | 125 | 0 | 3897 | 0 | 0 | 2480 |
| home-wifi@203 | 2.55 | 362 | 538 | 322 | 382 | 60 | 100 | - | - | 2 | 1306 | 125 | 0 | 3891 | 0 | 0 | 2420 |
| home-wifi@204 | 2.55 | 332 | 1081 | 322 | 332 | 10 | 90.1 | - | - | 1.97 | 1337 | 125 | 0 | 3897 | 0 | 0 | 1700 |
| mobile | 1.92 | 452 | 863 | 436 | 452 | 15.5 | 170 | 170 | - | 1.93 | 1093 | 128 | 0 | 3772 | 2 | 0 | 4980 |
| mobile@304 | 1.93 | 462 | 901 | 452 | 492 | 40 | 265 | - | - | 1.96 | 1071 | 130 | 0 | 3768 | 1 | 0 | 4300 |
| mobile@305 | 1.99 | 448 | 1121 | 422 | 532 | 110 | 158 | - | - | 1.82 | 1186 | 129 | 0 | 3755 | 0 | 0 | 5320 |
| bad | 1.42 | 806 | 1189 | 742 | 822 | 80 | 358 | 358 | - | 1.75 | 870 | 131 | 0 | 3429 | 4 | 0 | 10840 |
| bad@405 | 1.44 | 742 | 1191 | 652 | 793 | 142 | 326 | - | - | 1.78 | 884 | 131 | 0 | 3466 | 3 | 0 | 10960 |
| bad@406 | 1.38 | 732 | 1182 | 622 | 851 | 230 | 395 | - | - | 1.80 | 837 | 133 | 0 | 3478 | 5 | 0 | 10940 |
| burst | 3.05 | 262 | 382 | 271 | - | - | 0 | - | 5.50 | 2 | 952 | 125 | 0 | 2566 | 0 | 0 | 700 |
| drift | 4.26 | 141 | 168 | 114 | 165 | 51.1 | 0.67 | - | - | - | 285 | 10 | 0 | 30000 | 0 | 0 | 1480 |
| outage | 2.96 | 262 | 312 | 292 | - | - | 0 | - | 12 | 1.02 | 1291 | 125 | 0 | 2400 | 0 | 0 | 3220 |
| wrap | 3.78 | 262 | 292 | - | - | - | 0 | - | - | - | 604 | 125 | 0 | 1286 | 0 | 1 | 60 |

## All metrics

| metric | clean | home-wifi | home-wifi@203 | home-wifi@204 | mobile | mobile@304 | mobile@305 | bad | bad@405 | bad@406 | burst | drift | outage | wrap |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| MOS-LQO, mean over clips | 3.27 | 2.47 | 2.55 | 2.55 | 1.92 | 1.93 | 1.99 | 1.42 | 1.44 | 1.38 | 3.05 | 4.26 | 2.96 | 3.78 |
| MOS-LQO, worst clip | 2.55 | 1.71 | 2.10 | 1.87 | 1.60 | 1.48 | 1.41 | 1.17 | 1.19 | 1.27 | 1.58 | 3.83 | 1.11 | 3.50 |
| Mouth-to-ear delay p50 (ms) | 261.50 | 341.50 | 361.50 | 331.50 | 451.50 | 461.50 | 448 | 806.38 | 741.50 | 731.50 | 261.50 | 140.95 | 261.50 | 261.50 |
| Mouth-to-ear delay p95 (ms) | 321.25 | 561.50 | 537.70 | 1081.25 | 863.20 | 901.25 | 1121.25 | 1189.40 | 1191.25 | 1181.50 | 381.50 | 167.79 | 311.50 | 291.50 |
| Delay over the first corpus loop (ms) | 261.50 | 331.38 | 321.50 | 321.50 | 436 | 451.50 | 421.50 | 741.50 | 651.50 | 621.50 | 271 | 113.80 | 291.50 | - |
| Delay over the last corpus loop (ms) | 261.50 | 341.50 | 381.50 | 331.50 | 451.50 | 491.50 | 531.50 | 821.50 | 793.38 | 851.25 | - | 164.93 | - | - |
| Delay growth, last - first loop (ms) | 0 | 10.12 | 60 | 10 | 15.50 | 40 | 110 | 80 | 141.88 | 229.75 | - | 51.13 | - | - |
| Delay growth inside a clip, median (ms) | -5 | 69.88 | 100.12 | 90.12 | 170.12 | 264.88 | 158.25 | 358.12 | 326.25 | 395.12 | 0 | 0.67 | 0 | 0 |
| Delay growth inside a clip, median over seeds (ms) | - | 90.12 | - | - | 170.12 | - | - | 358.12 | - | - | - | - | - | - |
| MOS-LQO, median over seeds | - | 2.55 | - | - | 1.93 | - | - | 1.42 | - | - | - | - | - | - |
| Delay max (ms) | 321.50 | 661.50 | 591.50 | 1151.50 | 911.50 | 1369.50 | 1399.50 | 1509.50 | 1509.50 | 1201.25 | 866.50 | 177.59 | 1091.50 | 291.50 |
| Delay before the event (ms) | - | - | - | - | - | - | - | - | - | - | 251.50 | - | 221.50 | - |
| Delay peak from the event on (ms) | - | - | - | - | - | - | - | - | - | - | 866.50 | - | 1091.50 | - |
| Back to the pre-event delay after (s) | - | - | - | - | - | - | - | - | - | - | 5.50 | - | 12 | - |
| Concealment frames per lost frame | - | 2 | 2 | 1.97 | 1.93 | 1.96 | 1.82 | 1.75 | 1.78 | 1.80 | 2 | - | 1.02 | - |
| Lost frames (network + receiver drops) | 0 | 91 | 97 | 92 | 218 | 221 | 235 | 563 | 525 | 515 | 23 | 0 | 189 | 0 |
| Packets lost in the network | 0 | 91 | 97 | 91 | 216 | 220 | 233 | 559 | 522 | 510 | 23 | 0 | 189 | 0 |
| Concealment frames for losses | 0 | 182 | 194 | 181 | 420 | 433 | 427 | 983 | 932 | 926 | 46 | 0 | 192 | 0 |
| PLC on jitter Missing | 0 | 91 | 97 | 92 | 218 | 221 | 235 | 562 | 525 | 515 | 23 | 0 | 189 | 0 |
| FEC on sequence gap | 0 | 86 | 85 | 66 | 134 | 134 | 116 | 226 | 233 | 232 | 21 | 0 | 0 | 0 |
| PLC on sequence gap | 0 | 5 | 12 | 23 | 68 | 78 | 76 | 195 | 174 | 179 | 2 | 0 | 3 | 0 |
| PLC frames filling an empty playout buffer | 1500 | 1318 | 1306 | 1337 | 1093 | 1071 | 1186 | 870 | 884 | 837 | 952 | 285 | 1291 | 604 |
| Mixer underruns (no audio at all) | 125 | 125 | 125 | 125 | 128 | 130 | 129 | 131 | 131 | 133 | 125 | 10 | 125 | 125 |
| Decode errors | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| Packets sent | 3988 | 3988 | 3988 | 3988 | 3988 | 3988 | 3988 | 3988 | 3988 | 3988 | 2589 | 30000 | 2589 | 1286 |
| Packets fed to the receiver | 3988 | 3897 | 3891 | 3897 | 3772 | 3768 | 3755 | 3429 | 3466 | 3478 | 2566 | 30000 | 2400 | 1286 |
| Receiver drops: late | 0 | 0 | 0 | 0 | 2 | 1 | 0 | 4 | 3 | 5 | 0 | 0 | 0 | 0 |
| Receiver drops: jitter buffer full | 0 | 0 | 0 | 1 | 0 | 0 | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| Jitter buffer resets | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| Speech dropout (ms) | 460 | 2480 | 2420 | 1700 | 4980 | 4300 | 5320 | 10840 | 10960 | 10940 | 700 | 1480 | 3220 | 60 |
| Speech dropout around the RTP wrap (ms) | - | - | - | - | - | - | - | - | - | - | - | - | - | 0 |
| Decoded playout buffer p50 (ms) | 150 | 220 | 230 | 210 | 280 | 300 | 270 | 520 | 460 | 500 | 150 | 50 | 140 | 120 |
| Decoded playout buffer max (ms) | 220 | 580 | 510 | 990 | 810 | 910 | 990 | 990 | 990 | 990 | 680 | 90 | 990 | 190 |
| Jitter target delay p50 (ms) | 21 | 32 | 32 | 31 | 46 | 46 | 45 | 77 | 78 | 76 | 23 | 22 | 23 | 31 |

## Gates at the baseline (informational)

| profile | metric | value | rule | result | note |
|---|---|---:|---|---|---|
| clean | decode_errors | 0 | <= 0 | pass |  |
| clean | rx_dropped_late | 0 | <= 0 | pass |  |
| clean | rx_jitter_resets | 0 | <= 0 | pass |  |
| clean | latency_growth_ms | 0 | >= -20, <= 20 | pass | no delay build-up on a clean link |
| clean | latency_clip_growth_ms | -5 | >= -20, <= 20 | pass | no delay build-up on a clean link |
| clean | latency_p50_ms | 262 | <= 194 | FAIL | stage 3b: the shipped pre-roll (5 frames) plus 10 ms; the 100 ms target is not met (183.5 ms). 1 frame gives 101.5 to 104 ms but fails other enforced gates (TESTING.md, talk-spurt gate) |
| clean | onset_loss_db_p95 | - | <= 1.39 | FAIL | stage 3b: talk-spurt onset loss, first 200 ms; the shipped row (0.39 dB) plus 1 dB; stage 3 was 1.70 |
| clean | mos | 3.27 | >= 3.17 | pass | stage 5: MOS drops at most 0.1 against stage 4 (3.27) |
| home-wifi | decode_errors | 0 | <= 0 | pass |  |
| home-wifi | conceal_per_lost | 2 | >= 0.95, <= 1.05 | fail (informational) | stage 1.1: one concealment per lost frame; concealable losses only since stage 3b. Informational since stage 3b: NetEQ merges a short expand into the next frame, so the ratio is under 1 with the same total concealment; delay growth, its harm, is enforced by latency_clip_growth_ms and the drift gate |
| home-wifi | latency_clip_growth_ms_median | 90.1 | <= 20 | FAIL | stage 1 gate: flat delay at 2 % loss; median over seeds 202, 203, 204 since the slot shim: one seed's value spreads over more than the limit, because where a stall lands against a clip boundary decides it |
| home-wifi | latency_clip_growth_ms | 69.9 | <= 20 | fail (informational) | seed 202 alone, informational: the enforced gate is the median over seeds above |
| home-wifi | mos | 2.47 | >= 3.15 | FAIL | stage 5: MOS drops at most 0.1 against stage 4 (3.25) |
| mobile | decode_errors | 0 | <= 0 | pass |  |
| mobile | conceal_per_lost | 1.93 | >= 0.95, <= 1.05 | fail (informational) | stage 1.1; concealable losses only since stage 3b. Informational since stage 3b: NetEQ merges a short expand into the next frame, so the ratio is under 1 with the same total concealment; delay growth, its harm, is enforced by latency_clip_growth_ms and the drift gate |
| mobile | latency_clip_growth_ms_median | 170 | <= 20 | FAIL | stage 1.1: no delay build-up from double concealment; median over seeds 303, 304, 305 since the slot shim: one seed's value spreads over more than the limit, because where a stall lands against a clip boundary decides it |
| mobile | latency_clip_growth_ms | 170 | <= 20 | fail (informational) | seed 303 alone, informational: the enforced gate is the median over seeds above |
| mobile | mos | 1.92 | >= 2.70 | FAIL | stage 5: MOS drops at most 0.1 against stage 4 (2.80) |
| bad | decode_errors | 0 | <= 0 | pass |  |
| bad | conceal_per_lost | 1.75 | >= 0.95, <= 1.05 | fail (informational) | stage 1.1; concealable losses only since stage 3b. Informational since stage 3b: NetEQ merges a short expand into the next frame, so the ratio is under 1 with the same total concealment; delay growth, its harm, is enforced by latency_clip_growth_ms and the drift gate |
| bad | latency_clip_growth_ms_median | 358 | <= 20 | FAIL | stage 1.1: no delay build-up from double concealment; median over seeds 404, 405, 406 since the slot shim: one seed's value spreads over more than the limit, because where a stall lands against a clip boundary decides it |
| bad | latency_clip_growth_ms | 358 | <= 20 | fail (informational) | seed 404 alone, informational: the enforced gate is the median over seeds above |
| bad | mos | 1.42 | >= 2.12 | FAIL | stage 5: MOS drops at most 0.1 against stage 4 (2.22) |
| burst | decode_errors | 0 | <= 0 | pass |  |
| burst | recovery_s | 5.50 | <= 5 | FAIL | stage 3 gate: back to the pre-burst delay within 5 s; since stage 3b on the NetEQ buffer level, which accelerate drains |
| burst | recovery_e2e_s | - | <= 5 | fail (informational) | stage 3 gate before stage 3b: on the cross-correlated mouth-to-ear delay, which has points only in speech, so its value moves in steps of the pause schedule |
| burst | mos | 3.05 | >= 3.19 | FAIL | stage 5: MOS drops at most 0.1 against stage 4 (3.29) |
| drift | decode_errors | 0 | <= 0 | pass |  |
| drift | latency_growth_ms | 51.1 | >= -20, <= 20 | FAIL | stage 3 gate: zero drift over 10 minutes |
| drift | mos | 4.26 | >= 4.15 | pass | stage 5: MOS drops at most 0.1 against stage 4 (4.25) |
| outage | decode_errors | 0 | <= 0 | pass |  |
| outage | recovery_s | 12 | <= 5 | FAIL | stage 1.7: back to the pre-outage delay within 5 s; since stage 3b on the NetEQ buffer level, which has no point while the outage plays concealment |
| outage | recovery_e2e_s | - | <= 5 | fail (informational) | stage 1.7 before stage 3b: on the cross-correlated mouth-to-ear delay |
| outage | mos | 2.96 | >= 2.88 | pass | stage 5: MOS drops at most 0.1 against stage 4 (2.98) |
| wrap | decode_errors | 0 | <= 0 | pass |  |
| wrap | rx_jitter_resets | 1 | <= 0 | FAIL | stage 1.2: the wrap must not reset the jitter buffer |
| wrap | rx_dropped_late | 0 | <= 0 | pass | stage 1.2: no packet dropped at the wrap |
| wrap | wrap_dropout_ms | 0 | <= 0 | pass | stage 1.2: no dropout at the wrap |
| wrap | mos | 3.78 | >= 3.64 | pass | stage 5: MOS drops at most 0.1 against stage 4 (3.74) |
