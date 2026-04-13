# Audio Pipeline Specification

> **Component:** Voice Endpoint + SFU Voice Transport  
> **Status:** v0.3 Target  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)  
> **Related:** [02-MELLO-CORE.md](./02-MELLO-CORE.md), [03-LIBMELLO.md](./03-LIBMELLO.md), [13-VOICE-CHANNELS.md](./13-VOICE-CHANNELS.md), [15-DEBUG-TELEMETRY.md](./15-DEBUG-TELEMETRY.md), [EXTERNAL-SFU.md](./EXTERNAL-SFU.md)

---

## 1. Overview

This spec defines Mello's end-to-end realtime voice pipeline across:

- endpoint audio I/O and DSP (`libmello`)
- signaling/transport orchestration (`mello-core`)
- multi-party forwarding lifecycle (`mello-sfu`)

The target is Discord-grade continuity and intelligibility under realistic jitter/loss, not just clean-network demos.

**Capture and send path:**
```
┌──────────────┐    ┌──────────────────────────┐    ┌──────────────┐
│ Audio Input  │───▶│ DSP + VAD + Opus Encode │───▶│ Transport     │
│ WASAPI/CA    │    │ AEC/AGC/NS + seq header │    │ P2P or SFU    │
└──────────────┘    └──────────────────────────┘    └──────────────┘
```

**Receive and playout path:**
```
┌──────────────┐    ┌──────────────────────────┐    ┌──────────────┐
│ Transport    │───▶│ Jitter + Decode + PLC/FEC│───▶│ Audio Output │
│ P2P or SFU   │    │ per-peer mix + AEC render│    │ WASAPI/CA    │
└──────────────┘    └──────────────────────────┘    └──────────────┘
```

---

## 2. Audio Contract

All internal voice processing uses one hard contract:

- sample rate: `48000 Hz`
- channels: `mono (1)`
- sample format: `int16 PCM`
- frame size: `960 samples` (`20ms`)
- encoder: `Opus` with in-band FEC enabled

This contract is enforced at platform boundaries. Device-native formats are converted at capture/playback edges so DSP, VAD, Opus, and jitter logic operate on deterministic timing.

```cpp
// libmello/src/audio/opus_codec.hpp
static constexpr int FRAME_SIZE = 960;
static constexpr int SAMPLE_RATE = 48000;
static constexpr int CHANNELS = 1;
```

---

## 3. AudioPipeline (libmello)

`AudioPipeline` is the voice orchestrator. It owns capture/playback devices, DSP chain, per-peer decode state, jitter buffers, and clip buffering.

```cpp
// libmello/src/audio/audio_pipeline.hpp
class AudioPipeline {
public:
    bool initialize();
    void shutdown();

    bool start_capture();
    void stop_capture();

    void set_mute(bool muted);
    void set_deafen(bool deafened);
    void set_echo_cancellation(bool enabled);
    void set_agc(bool enabled);
    void set_noise_suppression(bool enabled);

    int  get_packet(uint8_t* buffer, int buffer_size);
    void feed_packet(const char* peer_id, const uint8_t* data, int size);

private:
    void on_captured_audio(const int16_t* samples, size_t count);
    size_t mix_output(int16_t* out, size_t count);
    void clear_remote_streams();
};
```

---

## 4. Capture and Encode Path

Per 20ms frame, endpoint processing order is:

1. optional input gain
2. AEC/AGC capture-side processing
3. clip ring tap (when clip buffer is active)
4. noise suppression (RNNoise)
5. VAD decision (Silero)
6. Opus encode only when speaking
7. enqueue encoded packet with monotonically increasing sequence

```cpp
// libmello/src/audio/audio_pipeline.cpp (simplified)
echo_canceller_.process_capture(capture_accum_.data(), FRAME_SIZE);
noise_suppressor_.process(capture_accum_.data(), FRAME_SIZE);
vad_.feed(capture_accum_.data(), FRAME_SIZE);
if (vad_.is_speaking()) {
    int encoded = encoder_.encode(...);
    pkt.sequence = sequence_++;
    outgoing_.push(std::move(pkt));
}
```

### 4.1 Packet Format

For endpoint packet API (`mello_voice_get_packet` / `mello_voice_feed_packet`), payload is prefixed by a 4-byte little-endian sequence number:

```
[seq0 seq1 seq2 seq3][opus_payload...]
```

In SFU RTP mode, `mello-core` strips this 4-byte sequence before `mello_peer_send_audio()` because RTP sequence/timestamp are handled by transport.

---

## 5. Receive, Jitter, and Playout

Each remote sender gets isolated decode state:

- `JitterBuffer`
- `OpusDec`
- decode priming + last sequence tracking
- per-peer PCM ring buffer

`JitterBuffer::pop()` returns timeline state, not only payload:

```cpp
enum class JitterPopResult {
    None,    // not ready yet
    Packet,  // packet available
    Missing, // packet considered lost; conceal
};
```

Current adaptive bounds:

- target delay defaults around `60ms`
- min/max delay guardrails `20ms..200ms`
- bounded per-callback drain to avoid lock monopolization

### 5.1 Concealment Policy

When timeline indicates loss:

- single-frame gap: attempt Opus in-band `decode_fec` from the next packet
- otherwise: `decode_plc` for bounded concealment frames
- explicit `Missing` events also trigger PLC when decoder is primed

This keeps continuity under jitter/loss while preventing unbounded latency growth.

### 5.2 Mix and Render

Mixed output applies:

- per-peer summing with saturation
- output gain
- optional clip playback overlay
- AEC render reference feed (`process_render`) when far-end audio exists

On voice leave, `stop_capture()` must clear all remote decode/jitter/ring state immediately to avoid stale PLC artifacts.

---

## 6. Platform Boundary Enforcement

### 6.1 Windows (WASAPI)

- capture/playback initialize from device mix format
- supports float/int16 device formats
- performs explicit downmix/resample to internal 48k mono int16 contract
- performs reverse conversion for playout to device-native format/channels

### 6.2 macOS (CoreAudio)

- capture/playback set 48k mono int16 stream format
- post-set validation rejects mismatch
- fails fast if actual device unit format violates contract

---

## 7. Transport and SFU Lifecycle

Voice transport has two modes:

- P2P mesh (legacy/backup)
- SFU voice (`mello-sfu`) with RTP audio tracks and websocket signaling

### 7.1 SFU Join/Negotiation

`mello-core` flow:

1. websocket connect (`welcome`)
2. `join_voice`
3. client offer -> server answer
4. ICE candidates exchanged
5. RTP audio send/receive + control channel

Server flow is serialized via per-peer renegotiation mutex to avoid offer/answer races.

### 7.2 Leave/Close Semantics

Leave must use a proper websocket close handshake:

1. client sends `leave`
2. server sends `left` ack and close control frame (`NormalClosure`)
3. both sides close websocket and release peer/session resources idempotently

This prevents stale legs and lingering half-closed sockets under repeated join/leave churn.

---

## 8. Public C API (Voice Surface)

Voice controls and packet ingress/egress are exposed through `mello.h`:

```c
MelloResult mello_voice_start_capture(MelloContext* ctx);
MelloResult mello_voice_stop_capture(MelloContext* ctx);
void mello_voice_set_mute(MelloContext* ctx, bool muted);
void mello_voice_set_deafen(MelloContext* ctx, bool deafened);
void mello_voice_set_echo_cancellation(MelloContext* ctx, bool enabled);
void mello_voice_set_agc(MelloContext* ctx, bool enabled);
void mello_voice_set_noise_suppression(MelloContext* ctx, bool enabled);
int  mello_voice_get_packet(MelloContext* ctx, uint8_t* buffer, int buffer_size);
MelloResult mello_voice_feed_packet(MelloContext* ctx, const char* peer_id, const uint8_t* data, int size);
```

All user-facing voice toggles in `client` and `mello-core` must map to these runtime controls (no dead settings).

---

## 9. Quality Gates

Every release candidate must pass a fixed impairment matrix and soak run set.

Hard gates:

- median mouth-to-ear latency remains within architecture target (`<50ms`)
- effective packet-loss impact remains bounded (`<1%` in defined profiles)
- join/rejoin success rate (`>90%` across churn loops)
- no repeated disconnect/renegotiation loops in steady-state rooms
- structured listening gate shows no severe intelligibility regressions

Operational tooling source of truth:

- `mello-sfu/tools/voice-soak/main.go`
- SFU admin troubleshoot endpoints and dashboard views

---

## 10. File Structure

```
libmello/src/audio/
├── audio_pipeline.hpp / .cpp
├── audio_capture.hpp / playback.hpp
├── capture_wasapi.hpp / .cpp
├── playback_wasapi.hpp / .cpp
├── capture_coreaudio.cpp
├── playback_coreaudio.cpp
├── jitter_buffer.hpp / .cpp
├── opus_codec.hpp / .cpp
├── noise_suppressor.hpp / .cpp
├── echo_canceller.hpp / .cpp
└── vad.hpp / .cpp

mello-core/src/
├── voice/mod.rs
└── transport/sfu_connection.rs

mello-sfu/internal/server/
├── signaling.go
├── peer.go
├── voice_session.go
└── admin.go
```

---

## 11. Exit Criteria

Audio pipeline work for this milestone is complete when:

- endpoint contract is enforced across supported platforms
- all exposed controls are runtime-effective
- jitter/loss concealment is stable and intelligible in impairment tests
- SFU leave/rejoin/renegotiation behaves deterministically under churn
- per-leg telemetry and admin troubleshooting views are sufficient for rapid diagnosis

