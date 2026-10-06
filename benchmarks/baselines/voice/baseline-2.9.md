# Voice quality baseline 2.9

> Taken 2026-10-06T08:07:01Z on windows/x86_64 from commit `af8c7f0a1c702773c754999cbcb8064e96c903ab`. This file and `baseline-2.9.json` are never edited. Every later run of `scripts/voice-gate.sh` prints its delta against them.

| Item | Value |
|---|---|
| Scorer | pesq-wb (ITU-T P.862.2), pesq 0.0.4 |
| Inputs fingerprint (profiles.json without gates) | `87b9c6fedca45671` |
| Corpus fingerprint | `ac3221b5ba3b8dc4` |
| Runtime | 95.9 s |
| Method: backend | MELLO_AUDIO_BACKEND=test (no device, no device thread) |
| Method: clock | virtual: mello_test_set_clock_ms, 1 ms steps; receiver pulls 10 ms per step of 10 ms |
| Method: delay | windowed cross-correlation at 4 kHz, reference vs output; latency = playout time - capture time |
| Method: path | mello_voice_inject_capture -> mello_voice_get_packet -> shim (SFU header rewrite, impairments) -> mello_voice_feed_packet -> mello_voice_test_pull_output |
| Method: send_tick | packets leave on a 20 ms tick at sender time 20k+10 ms, like VoiceManager::tick |
| Method: sender | one encode per sender setup, 10 ms capture chunks; profiles replay a prefix of the packet trace |

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

| profile | MOS | delay p50 | delay p95 | start | end | growth | in-clip growth | recover s | conceal/lost | fill PLC | underruns | dec err | fed | rx late | resets | dropout ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| clean | 3.28 | 262 | 312 | 262 | 262 | 0 | -5 | - | - | 1500 | 125 | 0 | 3988 | 0 | 0 | 460 |
| home-wifi | 2.58 | 335 | 602 | 338 | 331 | -7.12 | 60.1 | - | 2 | 1328 | 125 | 0 | 3902 | 0 | 0 | 2260 |
| mobile | 1.84 | 462 | 752 | 442 | 472 | 30 | 185 | - | 1.92 | 1070 | 128 | 0 | 3755 | 0 | 0 | 5560 |
| bad | 1.43 | 752 | 1172 | 712 | 752 | 40.1 | 403 | - | 1.77 | 833 | 127 | 0 | 3456 | 5 | 0 | 11980 |
| burst | 3.19 | 262 | 322 | 272 | - | - | 0 | 5.50 | 2 | 958 | 125 | 0 | 2569 | 0 | 0 | 760 |
| drift | 4.26 | 141 | 168 | 114 | 165 | 51.1 | 0.67 | - | - | 285 | 10 | 0 | 30000 | 0 | 0 | 1480 |
| outage | 2.96 | 262 | 322 | 282 | - | - | 0 | 12 | 1.02 | 1289 | 125 | 0 | 2400 | 0 | 0 | 3280 |
| wrap | 3.55 | 252 | 272 | - | - | - | -16.9 | - | 0 | 608 | 125 | 0 | 1286 | 2 | 1 | 140 |

## All metrics

| metric | clean | home-wifi | mobile | bad | burst | drift | outage | wrap |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| MOS-LQO, mean over clips | 3.28 | 2.58 | 1.84 | 1.43 | 3.19 | 4.26 | 2.96 | 3.55 |
| MOS-LQO, worst clip | 2.55 | 2.06 | 1.44 | 1.22 | 2.26 | 3.83 | 1.11 | 3.50 |
| Mouth-to-ear delay p50 (ms) | 261.50 | 335.25 | 461.50 | 751.50 | 261.50 | 140.95 | 261.50 | 251.50 |
| Mouth-to-ear delay p95 (ms) | 311.50 | 601.50 | 751.50 | 1171.50 | 321.50 | 167.79 | 321.50 | 271.50 |
| Delay over the first corpus loop (ms) | 261.50 | 338.38 | 441.50 | 711.50 | 271.50 | 113.80 | 281.50 | - |
| Delay over the last corpus loop (ms) | 261.50 | 331.25 | 471.50 | 751.62 | - | 164.93 | - | - |
| Delay growth, last - first loop (ms) | 0 | -7.12 | 30 | 40.12 | - | 51.13 | - | - |
| Delay growth inside a clip, median (ms) | -5 | 60.12 | 184.88 | 402.62 | 0 | 0.67 | 0 | -16.88 |
| Delay max (ms) | 311.50 | 1349.50 | 1379.50 | 1419.50 | 781.50 | 177.59 | 1091.50 | 271.50 |
| Delay before the event (ms) | - | - | - | - | 251.50 | - | 221.50 | - |
| Delay peak from the event on (ms) | - | - | - | - | 781.50 | - | 1091.50 | - |
| Back to the pre-event delay after (s) | - | - | - | - | 5.50 | - | 12 | - |
| Concealment frames per lost frame | - | 2 | 1.92 | 1.77 | 2 | - | 1.02 | 0 |
| Lost frames (network + receiver drops) | 0 | 86 | 233 | 537 | 20 | 0 | 189 | 2 |
| Packets lost in the network | 0 | 86 | 233 | 532 | 20 | 0 | 189 | 0 |
| Concealment frames for losses | 0 | 172 | 447 | 949 | 40 | 0 | 192 | 0 |
| PLC on jitter Missing | 0 | 86 | 233 | 537 | 20 | 0 | 189 | 0 |
| FEC on sequence gap | 0 | 81 | 136 | 237 | 20 | 0 | 0 | 0 |
| PLC on sequence gap | 0 | 5 | 78 | 175 | 0 | 0 | 3 | 0 |
| PLC frames filling an empty playout buffer | 1500 | 1328 | 1070 | 833 | 958 | 285 | 1289 | 608 |
| Mixer underruns (no audio at all) | 125 | 125 | 128 | 127 | 125 | 10 | 125 | 125 |
| Decode errors | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| Packets sent | 3988 | 3988 | 3988 | 3988 | 2589 | 30000 | 2589 | 1286 |
| Packets fed to the receiver | 3988 | 3902 | 3755 | 3456 | 2569 | 30000 | 2400 | 1286 |
| Receiver drops: late | 0 | 0 | 0 | 5 | 0 | 0 | 0 | 2 |
| Receiver drops: jitter buffer full | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| Jitter buffer resets | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| Speech dropout (ms) | 460 | 2260 | 5560 | 11980 | 760 | 1480 | 3280 | 140 |
| Speech dropout around the RTP wrap (ms) | - | - | - | - | - | - | - | 80 |
| Decoded playout buffer p50 (ms) | 150 | 210 | 280 | 470 | 150 | 50 | 140 | 120 |
| Decoded playout buffer max (ms) | 210 | 540 | 980 | 990 | 660 | 90 | 990 | 170 |
| Jitter target delay p50 (ms) | 21 | 32 | 46 | 75 | 23 | 22 | 23 | 31 |

## Gates at the baseline (informational)

| profile | metric | value | rule | result | note |
|---|---|---:|---|---|---|
| clean | decode_errors | 0 | <= 0 | pass |  |
| clean | rx_dropped_late | 0 | <= 0 | pass |  |
| clean | rx_jitter_resets | 0 | <= 0 | pass |  |
| clean | latency_growth_ms | 0 | >= -20, <= 20 | pass | no delay build-up on a clean link |
| clean | latency_clip_growth_ms | -5 | >= -20, <= 20 | pass | no delay build-up on a clean link |
| clean | latency_p50_ms | 262 | <= 100 | fail (informational) | stages 2 and 3: mouth-to-ear delay on a 20 ms link |
| home-wifi | decode_errors | 0 | <= 0 | pass |  |
| home-wifi | conceal_per_lost | 2 | >= 0.95, <= 1.05 | fail (informational) | stage 1.1: one concealment per lost frame |
| home-wifi | latency_clip_growth_ms | 60.1 | <= 20 | fail (informational) | stage 1 gate: flat delay at 2 % loss |
| mobile | decode_errors | 0 | <= 0 | pass |  |
| mobile | conceal_per_lost | 1.92 | >= 0.95, <= 1.05 | fail (informational) | stage 1.1 |
| mobile | latency_clip_growth_ms | 185 | <= 20 | fail (informational) | stage 3: delay goes down as well as up |
| bad | decode_errors | 0 | <= 0 | pass |  |
| bad | conceal_per_lost | 1.77 | >= 0.95, <= 1.05 | fail (informational) | stage 1.1 |
| bad | latency_clip_growth_ms | 403 | <= 20 | fail (informational) | stage 3: delay goes down as well as up |
| burst | decode_errors | 0 | <= 0 | pass |  |
| burst | recovery_s | 5.50 | <= 5 | fail (informational) | stage 3 gate: back to the pre-burst delay within 5 s |
| drift | decode_errors | 0 | <= 0 | pass |  |
| drift | latency_growth_ms | 51.1 | >= -20, <= 20 | fail (informational) | stage 3 gate: zero drift over 10 minutes |
| outage | decode_errors | 0 | <= 0 | pass |  |
| outage | recovery_s | 12 | <= 5 | fail (informational) | stage 3: back to the pre-outage delay within 5 s |
| wrap | decode_errors | 0 | <= 0 | pass |  |
| wrap | rx_jitter_resets | 1 | <= 0 | fail (informational) | stage 1.2: the wrap must not reset the jitter buffer |
| wrap | rx_dropped_late | 2 | <= 0 | fail (informational) | stage 1.2: no packet dropped at the wrap |
| wrap | wrap_dropout_ms | 80 | <= 0 | fail (informational) | stage 1.2: no dropout at the wrap |
