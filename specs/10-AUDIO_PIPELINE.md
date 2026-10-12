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
│ Transport    │───▶│ NetEQ per peer: decode,  │───▶│ Audio Output │
│ RTP or P2P   │    │ FEC, PLC, time stretch;  │    │ WASAPI/CA    │
│ frames       │    │ stereo mixer + limiter;  │    │ (stereo)     │
│              │    │ AEC render reference     │    │              │
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

`AudioPipeline` is the voice orchestrator. It owns capture/playback devices, the DSP chain, the receive and playout engine (`VoicePlayout`: one NetEQ per peer and the mixer, section 5), and clip buffering.

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
    void set_ns_mode(NsMode mode);
    void set_transient_suppression(bool enabled);
    void set_high_pass_filter(bool enabled);

    int  get_packet(uint8_t* buffer, int buffer_size);
    void set_packet_sink(PacketSink sink);  // §4.5
    void feed_packet(const char* peer_id, const uint8_t* data, int size);  // voice frame, §4.2
    void feed_rtp(const char* peer_id, const uint8_t* packet, int size, int64_t receive_time_ms);
    void remove_peer(const char* peer_id);
    void set_peer_volume(const char* peer_id, float gain);  // §5.3
    void set_peer_pan(const char* peer_id, float pan);

private:
    void on_captured_audio(const int16_t* samples, size_t count);
    void process_and_encode_frame(int16_t* frame, uint32_t timestamp);
    size_t mix_output(int16_t* out, size_t count, uint32_t channels);  // device thread
    void render_frame();                                               // one 10 ms mix
    void clear_remote_streams();

    std::unique_ptr<VoicePlayout> playout_;  // NetEQ per peer + AudioMixerImpl
};
```

`set_echo_cancellation` is a backend selector on macOS: on selects the VPIO duplex unit (OS AEC/AGC, software APM capture skipped), off selects the plain HAL pair plus software AEC. On other platforms it only flips the APM flag. The switch applies immediately only mid-session; outside voice the desired backend is stored for the next capture start.

---

## 4. Capture and Encode Path

Per 20ms frame, endpoint processing order is adaptive:

1. optional input gain
2. WebRTC APM capture-side processing (AEC3 + AGC2, plus optional WebRTC NS/HPF) — skipped when the macOS VPIO duplex backend is active (the OS unit already ran AEC/AGC; see §6.2)
3. neural residual-echo suppression when `echo_suppression` is on (software path only; post-AEC mic plus far-end reference ring, silence bypass)
4. clip ring tap (when clip buffer is active)
5. cheap RMS/noise-floor gate updates input level and decides whether this is a speech candidate (uses post-stage RMS when the suppressor ran, pre-AEC level otherwise)
6. Silero VAD runs only for candidate speech / hangover windows
7. downward expander attenuates the frame toward a floor gain when the VAD is below threshold (see §4.4)
8. when speech opens, flush pre-roll frames so starts are not clipped
9. while speech or hangover is active, apply the selected enhancement mode and Opus encode
10. give the encoded packet, with a monotonically increasing sequence, to the packet sink (§4.5), or put it in the `get_packet` queue when no sink is set

RNNoise remains the default quality noise suppression path, but it is not run on obvious
silence or non-speech background. This preserves Discord-like voice quality during speech
while avoiding continuous neural DSP cost when no voice is being transmitted. WebRTC NS
levels remain available as runtime test/diagnostic modes.

```cpp
// libmello/src/audio/audio_pipeline.cpp (simplified)
echo_canceller_.process_capture(capture_accum_.data(), FRAME_SIZE);
if (suppressor_enabled) echo_suppressor_.process(capture_accum_.data());
bool candidate = gate_rms >= max(MIN_SPEECH_RMS, noise_floor * NOISE_FLOOR_GATE_MULT);
if (candidate || candidate_hangover || speech_hangover) {
    vad_.feed(capture_accum_.data(), FRAME_SIZE);
}
speech_expander_.process(capture_accum_.data(), FRAME_SIZE,
                         vad_.probability() >= VAD_THRESHOLD);  // §4.4
if (vad_.is_speaking() || speech_hangover) {
    flush_pre_roll_if_gate_just_opened();
    process_and_encode_frame(capture_accum_.data()); // RNNoise/WebRTC mode + Opus
} else {
    keep_pre_roll_and_update_noise_floor();
}
```

### 4.1 Adaptive Speech Gate

The capture path keeps a short pre-roll buffer (currently about `100ms`) and two hangovers:

- **Candidate hangover:** keeps Silero active through brief RMS dips once a candidate speech window opens.
- **Speech encode hangover:** keeps RNNoise/Opus active briefly after VAD drops to avoid chopped endings.

The gate closes on sustained non-speech. Closing the gate forces the VAD callback to emit `speaking=false`
and prevents stale speaking indicators when Silero is no longer being fed.

Input sensitivity sets the RMS gate threshold (the "Activation Sensitivity" control in Settings):

| Mode | Gate threshold | Set by |
|---|---|---|
| Auto (default) | `max(MIN_SPEECH_RMS, 2.5 x noise floor)`: at least -54 dBFS, tracks the ambient floor | `mello_voice_set_input_sensitivity_auto(ctx, true)` |
| Manual | The slider level in dBFS (-100..0) of raw frame RMS, the scale of the input level meter | `mello_voice_set_input_sensitivity(ctx, db)` and auto off |

- Silero VAD confirms speech at `VAD_THRESHOLD` (0.35) in both modes.
- The noise floor is tracked in both modes, so a change back to auto applies at once.
- Push-to-talk bypasses the gate, so the setting has no effect there.
- `MelloDebugStats` reports `input_sensitivity_auto`, `input_sensitivity_db` and `input_gate_dbfs` (the threshold of the last frame).
- The client sends `Command::SetInputSensitivity { auto, db }` at startup, on each change of the slider or the toggle, and on reset. A settings file without `vad_auto` loads with auto on.

This design was validated with:

- continuous speech: RNNoise runs and quality is preserved
- music-only/non-speech: RNNoise/Opus stay closed except for brief false positives
- idle voice channel: baseline CPU remains low
- no crackles observed in the validated client run

### 4.2 Packet Format

`mello_voice_get_packet` prefixes the Opus payload with a 4-byte little-endian sequence number:

```
[seq0 seq1 seq2 seq3][opus_payload...]
```

The voice frame of the P2P data channel and of the mic test (`mello_voice_feed_packet`) adds the RTP timestamp, so NetEQ gets real media time on every path:

```
[seq0 seq1 seq2 seq3][ts0 ts1 ts2 ts3][opus_payload...]     (both little-endian)
```

mello-core builds it from `mello_voice_get_packet_with_timestamp` (`voice/frame.rs`). A peer that runs a build before stage 3 sends the 4-byte form, which this receiver reads as a wrong timestamp; P2P is the backup path and both ends update together.

RTP timestamp (media time):

- The capture thread keeps a 48 kHz sample counter. It counts every captured 20 ms frame, encoded or not. A frame that the speech gate or mute holds back still advances it, so a gap looks like DTX and not like a stall.
- Each encoded packet carries the counter value of its first sample. Pre-roll frames keep the value of the time they were captured.
- `mello_voice_get_packet_with_timestamp()` returns the packet and this value. Consecutive frames differ by 960. A 1 s gate gap adds 48000.
- In SFU RTP mode, the packet sink (§4.5) calls `mello_peer_send_audio_frame(peer, opus, size, timestamp)` with the Opus payload, without the 4-byte sequence. libmello sends the frame with a libdatachannel `FrameInfo`: RTP timestamp = the packetizer's random start timestamp + media time, modulo 2^32. The RTP sequence comes from the packetizer.
- `mello_peer_send_audio()` does not advance the RTP timestamp (libdatachannel takes it only from a `FrameInfo`). No caller in the repo uses it.
- Stream game audio uses the same packetizer. Its capture clock counts every 20 ms frame (`ts_us`); mello-core converts it to 48 kHz samples for `mello_peer_send_audio_frame`.
- The P2P data-channel path carries the voice frame above: sequence and media time.

Receive side (section 5.1):

- The SFU path gives the whole RTP packet to `mello_voice_feed_rtp`: NetEQ sees the real 16-bit sequence, the timestamp, the payload type and the SSRC.
- The P2P and mic test paths give the voice frame to `mello_voice_feed_packet`. libmello takes the low 16 bits of the sequence and the timestamp as the RTP fields.
- NetEQ handles the 16-bit sequence wrap and the 32-bit timestamp wrap. The wrap is one ordinary step: no discard, no reset (voice gate `wrap`).

### 4.3 Push-to-Talk Mode

When the client enables push-to-talk (`mello_voice_set_push_to_talk(true)`), Silero VAD and the adaptive RMS/pre-roll speech gate are bypassed while the mic is unmuted. The client hotkey and mute state control when packets are sent; AEC3, AGC2, RNNoise, and Opus run on every captured frame during an unmuted PTT hold. Speaking indicators and `voice_speaking` presence remain hotkey-driven in the client, not Silero-driven.

### 4.4 Downward Expander (AGC pump control)

`SpeechExpander` (`libmello/src/audio/speech_expander.hpp`) runs after AGC2 and after the gate decision, before encode. It solves the quiet-mic AGC pump.

Problem: a quiet mic (field case: −39 dBFS speech) forces AGC2 to a large gain (~+23 dB) to normalize the voice. AGC2 applies one gain to speech and floor alike, so it holds that gain across inter-word gaps and blasts the noise floor (measured +14..23 dB). The far side hears the floor swelling between words as crackle. No AGC2 config separates the two gains (verified by a knob sweep).

Fix: the expander passes speech at unity gain and attenuates the frame toward a floor gain (default −20 dB) when the VAD reports no speech. Attack is fast (~1 frame); release is smoothed (~150 ms) so word onsets and tails are not chopped.

Key detail: the expander is keyed off the **raw** `vad_.probability() >= VAD_THRESHOLD`, not `is_speaking()`. `is_speaking()` latches through an 8-frame holdover and stays true through the gaps that pump. The raw probability drops in those gaps and closes the expander there.

The gate decision above is unchanged: it reads the frame before the expander runs. The expander state resets with the gate state on session boundaries. AGC2 also starts at `initial_gain_db = 0` (not the M131 default +15) to remove a separate start-gain pump on the listener path (`echo_canceller.cpp`).

### 4.5 Send Path: Packet Sink

Rule: an encoded frame never waits for the core command loop (plans/voice-quality.md section 3).

`mello_voice_set_packet_sink(ctx, cb, user_data)` sets a sink. While a sink is set:

- The capture thread calls the sink right after Opus encode. Arguments: the Opus payload and its size, the media time (§4.2), the 32-bit sequence.
- The frame does not go to the `get_packet` queue. A frame goes to one of the two, never to both.
- Setting a sink drops the frames in the queue. They would leave late, or reach a later `get_packet` caller as stale audio.
- Mute and push-to-talk gate frames before encode, as for the queue. A frame that is not encoded does not reach the sink.

Lifetime: set and clear (`cb = NULL`) wait for a sink call that runs now. After a clear returns, no call uses `user_data`. `mello_destroy` clears the sink.

Who uses which path:

| Mode | Send path | Thread |
|---|---|---|
| SFU voice | Packet sink -> `mello_peer_send_audio_frame` | Audio capture thread |
| P2P mesh | `get_packet` on the 20 ms voice tick -> data channel | Command loop |
| Mic test (loopback), no call | `get_packet` on the voice tick -> voice frame -> `mello_voice_feed_packet` | Command loop |
| Mic test in an SFU call | The packet sink also feeds the voice frame as the `loopback` peer | Audio capture thread |
| Voice quality gate | Packet sink collects the trace (§9) | Caller thread |

mello-core details are in §7.3. The thread rules for the sink are in spec 03 §7.

---

## 5. Receive, NetEQ, and Playout

Received voice plays through WebRTC NetEQ (M131), one instance per remote peer, and the WebRTC `AudioMixerImpl` over them (plans/voice-quality.md stage 3). NetEQ owns decode, in-band FEC, concealment (expand), comfort noise, and time stretching (accelerate, preemptive expand). The custom jitter buffer, the per-peer Opus decoders and the fill PLC are gone.

```
SFU RTP packet ──▶ mello_voice_feed_rtp ──┐
P2P / mic test frame ──▶ mello_voice_feed_packet ──┤  (network, command loop, capture threads)
                                          ▼
                    VoicePlayout: peer map ──▶ NetEq per peer (InsertPacket)
                                                   │
device thread: mix_output ──▶ render_frame ──▶ AudioMixerImpl::Mix (GetAudio per peer, 10 ms)
                                   │              volume, pan, output gain, limiter (stereo)
                                   ├──▶ deafen, clip tap, clip playback, AEC render reference
                                   ▼
                              output ring (10 ms) ──▶ device callback (any size, 1 or 2 channels)
```

Code: `libmello/src/audio/voice_playout.{hpp,cpp}` (NetEQ, controller, mixer), `audio_pipeline.cpp` (`feed_packet`, `feed_rtp`, `render_frame`, `mix_output`).

### 5.1 Receive Edges

| Path | Input | Call | Thread |
|---|---|---|---|
| SFU voice | The whole RTP packet from the track (`mello_peer_set_audio_rtp_callback`). The sender is the member that the SFU's slot map names for the track's mid (section 7.1), not an SDP attribute | `mello_voice_feed_rtp(ctx, sender, rtp, size, -1)` | libdatachannel track thread |
| SFU voice, fallback | `SfuEvent::AudioRtp` from the event queue, only while the direct sink is not set | `mello_voice_feed_rtp` | Command loop |
| P2P mesh | The voice frame from the data channel (section 4.2) | `mello_voice_feed_packet` | Command loop |
| Mic test | The voice frame of the local encoder | `mello_voice_feed_packet` as peer `loopback` | Command loop, or the capture thread in an SFU call |

- `feed_rtp` parses the RTP header (RFC 3550: CSRC list, header extension, padding). Payload types other than 111 (Opus) and 13 (CN) count as rejected and do not reach NetEQ.
- `feed_packet` builds the RTP fields from the 8-byte voice frame header: payload type 111, the low 16 bits of the sequence, the timestamp, SSRC 0. Both paths enter the same NetEQ code.
- Receive time: `receive_time_ms < 0` stamps the packet with `util::steady_now_ms()`: the steady clock in production, the test clock under the voice gate. NetEQ times arrivals with its own tick (one per `GetAudio`) and uses the receive time only for its statistics.
- NetEQ's clock is a `webrtc::Clock` subclass on `util::steady_now_ms()`, never `Clock::GetRealTimeClock()`, so the test clock drives it.
- The legacy audio track callback (`mello_peer_set_audio_track_callback`) still delivers the Opus payload behind a 4-byte sequence header. Stream audio (P2P) and tools use it. An SFU stream viewer strips the RTP header in mello-core (`feed_viewer_audio_rtp`).

### 5.2 NetEQ per Peer

- A peer's NetEQ is created on its first packet: 48 kHz, `enable_muted_state` on, payload 111 = `opus/48000/2` with `stereo=0` (mono decode), payload 13 = `CN/48000`. The decoder factory has Opus only.
- One `webrtc::Environment` per pipeline carries the clock and a mello `FieldTrialsView`. It answers one key, `WebRTC-Audio-NetEqDelayManagerConfig`, with `VoicePlayout::default_delay_manager_config()` (the tuning in section 5.5).
- A new SSRC for the same peer rebuilds its NetEQ (a member that moves to another receive slot or m-line). A timestamp more than 2 s behind the newest one also rebuilds it (a sender restart; P2P keeps SSRC 0). `rx_stream_resets` counts both.
- A receive slot keeps its SSRC when the SFU gives it to another member. NetEQ is keyed by member, so the new member gets a new NetEQ; the old member's NetEQ is destroyed when the slot message releases its last mid (section 7.1). No state carries over between members on one SSRC.
- Insert runs on the inserting thread. `NetEqImpl` locks itself, so an insert and the device thread's `GetAudio` may overlap. The peer map has its own mutex, which the device thread never takes. Thread and lock rules: spec 03 section 7.
- Lifetime: `clear_remote_streams` (voice leave, `mello_voice_stop_capture`) destroys every NetEQ. `mello_voice_remove_peer` destroys one (mello-core calls it when a member leaves). Removal takes the source out of the mixer first and waits for a running mix. Lifetime counters of destroyed instances stay in the totals.

**Talk-spurt controller.** NetEQ runs its own `DecisionLogic` inside a mello `NetEqController` (`TalkSpurtController`, through NetEQ's public controller factory API, no upstream edit). It changes two decisions. Both come from voice gate measurements: there, NetEQ's time stretching of speech cost more PESQ MOS than the latency it saved. With time stretching off, NetEQ matched the stage 2 MOS on every profile; with upstream decisions it lost 0.15 to 0.3.

The first change handles one property of the mello sender: the speech gate sends nothing while it is closed, and when it opens it sends the pre-roll (section 4.1, up to 100 ms) and the current frame at once. So the first packets of a talk spurt arrive up to about 100 ms late against their media time, by design.

| Without the controller (measured, voice gate) | With it |
|---|---|
| NetEQ learns the pre-roll lateness as network jitter: target delay 100 to 180 ms on every profile with a speech gate, for seconds after each spurt start. | The packets that arrive in the same 10 ms tick as a spurt start stay out of NetEQ's arrival statistics. |
| NetEQ time-compresses the extra 100 ms during the following speech (accelerate). PESQ: about -0.3 MOS on `clean`. | The controller holds a minimum delay equal to the spurt's pre-roll lateness, so the spurt plays at its natural delay. The next gate gap removes it at no cost: the next spurt measures its own lateness. |

- Spurt start: the first packet, or a packet whose RTP timestamp moved more than 2 frames further than its sequence number (the sender skipped frames).
- Lateness: how late the spurt's first packet arrived against the schedule of the earliest-scheduled packet in the first 200 ms of the spurt. The hold follows it from 0 as the pre-roll arrives; a spurt never inherits the previous spurt's hold. Below 40 ms it is network jitter, and no delay is held. At most 300 ms.
- A stream with no gate gap (push-to-talk) is one spurt.

The second change: no preemptive expand for 5 s after a target jump. A stall, a jitter spike or reordered packets make the delay manager raise its target in one step (20 ms or more). Upstream NetEQ then stretches the speech to fill the buffer up to the new target, and soon after compresses it again when the target decays. The controller does not stretch the speech in those 5 s; real lateness still fills the buffer, through expand. The fill at stream start and slow target changes are NetEQ's own.

Network jitter, loss, outages and clock drift are otherwise NetEQ's own decisions.

### 5.3 Mixer and Output

- One `AudioMixer::Source` per peer. `GetAudioFrameWithInfo` calls `NetEq::GetAudio` for 10 ms and writes a stereo frame. It returns muted when NetEQ is in muted state.
- Per-peer volume (`mello_voice_set_peer_volume`, 0..2, default 1) times the output volume (`mello_voice_set_output_volume`) applies in the source, before the mixer's limiter. Above 1.0 a loud peer can clip; the limiter acts only on a sum of two or more peers.
- Pan (`mello_voice_set_peer_pan`, -1..+1, default 0) is a balance law: the far channel fades, the near channel stays at unity. Centre is unity on both channels, so a mono downmix is the decoded signal.
- `AudioMixerImpl::Mix(2, ...)` sums the peers with the AGC2 limiter of its `FrameCombiner` (one peer: no limiter). The mix is 10 ms stereo at 48 kHz.
- `render_frame` (device thread) then: deafen zeroes the mix (NetEQ keeps running, so the voice is back at once); the clip buffer records the mono downmix plus the local mic; clip playback adds to both channels; the AEC render reference and the echo suppressor's far end get the mono downmix of what plays. A mix with no peer audio and no clip feeds nothing to the AEC.
- The output ring holds less than one 10 ms frame. `mix_output` fills the device callback from it, with the backend's channel count: 2 on WASAPI and plain CoreAudio, 1 on VPIO, RemoteIO and the test backend (the downmix).
- Stream-delay hint (spec 03 section 4): device output latency + device input latency + the output ring. NetEQ's buffer is before the render reference, so it is not in the echo path.

### 5.4 Telemetry

`MelloDebugStats` (append-only). Delay fields are means over the current peers; counters are totals since the context started, over every peer.

| Field | Source |
|---|---|
| `pipeline_delay_ms` | NetEQ `FilteredCurrentDelayMs` + the output ring. 0 with no peer. Not `GetPlayoutTimestamp`: NetEQ reports none during comfort noise. |
| `rx_target_delay_ms`, `rx_filtered_delay_ms`, `rx_buffer_ms`, `rx_output_ring_ms` | NetEQ `TargetDelayMs` (includes the spurt hold), `FilteredCurrentDelayMs`, packet + sync buffer, the output ring |
| `rx_total_samples_received` .. `rx_interruption_duration_ms` | `NetEqLifetimeStatistics` |
| `rx_buffer_flushes` | NetEQ packet buffer flushes |
| `rx_packets_inserted`, `rx_insert_errors`, `rx_packets_rejected`, `rx_stream_resets` | Engine counters |
| `rx_talk_spurts`, `rx_spurt_hold_ms` | Talk-spurt controller |
| `rx_late_underruns` | 10 ms frames in which a peer played voice expand while at least one 20 ms packet waited in its buffer (late audio). A talk pause leaves the buffer empty and does not count. |
| `underrun_count` | 10 ms mixes in which no peer produced audio (also the muted state of a silent peer) |
| `rx_decode_errors` | NetEQ `GetAudio` failures |
| `rx_jitter_dropped_late`, `rx_jitter_resets`, `rx_jitter_target_delay_ms`, `rx_playout_buffer_ms` | Kept for the ABI: `rx_packets_discarded`, `rx_stream_resets + rx_buffer_flushes`, `rx_target_delay_ms`, `rx_buffer_ms` |
| `rx_frames_decoded`, `rx_conceal_*`, `rx_jitter_missing`, `rx_jitter_dropped_overflow`, `rx_jitter_buffered_packets` | 0: no NetEQ equivalent |

### 5.5 Concealment and Delay Policy

- One timeline: NetEQ produces exactly 10 ms per `GetAudio`. A lost frame is concealed (expand) or repaired from the next packet's in-band FEC once, on the timeline. A loss cannot add delay.
- Delay follows the network both ways: the delay manager's target rises under jitter, and accelerate brings the buffer back down when the target falls. Preemptive expand fills a buffer that runs low.
- Delay manager tuning (`WebRTC-Audio-NetEqDelayManagerConfig`): `forget_factor:0.6,reorder_forget_factor:0.98` (`VoicePlayout::default_delay_manager_config()`). Upstream defaults take about 26 s to forget a 3 s jitter burst; the gate requires 5 s. The measured sweep is in `TESTING.md`.
- Clock drift: NetEQ times playout by the device's `GetAudio` calls and removes the sender's excess with accelerate (voice gate `drift`: growth within ±20 ms over 10 minutes).
- Long outage: NetEQ expands, mutes, and restarts from the first new packet. No backlog plays.

---

## 6. Platform Boundary Enforcement

### 6.1 Windows (WASAPI)

- capture/playback initialize from device mix format
- supports float/int16 device formats
- capture: downmix to mono, then `StreamResampler` (WebRTC `PushSincResampler`, 10 ms blocks) to the internal 48 kHz mono int16 contract
- playback: the voice mix is stereo; `StreamResampler` converts 48 kHz to the device rate per channel. Mono goes to every device channel, stereo to the first two (a surround device gets silence on the rest), and a mono device gets the downmix
- reports `GetStreamLatency` plus device period as the APM delay-hint source, cached at init; query failure degrades to 0 and never fails init

### 6.2 macOS (CoreAudio)

- capture/playback set 48k mono int16 stream format
- post-set validation rejects mismatch
- fails fast if actual device unit format violates contract

Voice path: one VoiceProcessingIO duplex unit carries capture input and render output together when the echo toggle is on. Apple's AEC reference is the audio rendered through that unit's own output bus, so the pair cannot split: input-only VPIO never initializes, and device switches rebuild both halves together with plain-HAL fallback. The duplex unit is voice-session scoped: it activates on capture start and drops back to the plain pair on stop, so no mic-capable unit runs (and no mic indicator shows) outside a voice session. Capture buffers are sized defensively (observed slices exceed the unit's reported max). See spec 03 §4.

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

**Receive slots** (mello-sfu `01-SFU.md` sections 5.4, 5.5, 6.2). The SFU voice offer carries the sendrecv mic m-line (mid `audio`, with `a=ssrc`) and `MELLO_VOICE_RECV_SLOTS` (16) recvonly audio m-lines with mids `s0` to `s15` (`mello_peer_set_voice_recv_slots`, set by `SfuConnection::create_peer`; P2P peers offer none). The SFU forwards one member on each slot and changes members with `ReplaceTrack`: a join or a leave within 16 members sends no `offer`. Past 16, the SFU adds an own m-line per extra member and renegotiates, as before.

The SFU names the member on each mid in JSON text on the control DataChannel:

| Message | Client action (thread: libdatachannel) |
|---|---|
| `{"type":"slot","mid":"s3","user":"<id>"}` | `mello_peer_set_audio_mid_sender(peer, "s3", id)`: RTP on `s3` is from `id` from now on |
| `{"type":"slot","mid":"s3","user":null}` | The mid is freed: its packets drop. A member that holds no mid any more loses its NetEQ (`mello_voice_remove_peer`) |
| `{"type":"speaking","user":"<id>","on":true}` | `Event::VoiceActivity { member_id: id, speaking: true }` (section 7.4) |

- The SFU sends the whole map (and who speaks now) when the control channel opens.
- libmello keeps the map in `VoiceSlotMap` and resolves the sender per packet; a packet on a mid with no member drops. RTP and the control channel are separate transports, so the first packets of a new member can drop before its slot message arrives (soak: under 0.1 %).
- mello-core parses the messages (`handle_control_text` in `transport/sfu_connection.rs`) on the libdatachannel thread, so no slot change and no speaking ring waits on the command loop. Without the direct voice sink, a release goes to the voice tick as `SfuEvent::SlotReleased`.
- The SDP attribute `msid` names nothing for voice any more. A stream viewer still checks the stream audio track's msid (`is_stream_audio_sender`).
- Each mid map belongs to one PeerConnection: a new offer starts empty.

A busy session sends other signaling while a join waits for `joined` (step 2) or `answer` (step 3): `member_joined`, `member_left`, `ice_candidate`, a server `offer`. The client reads until the wanted type or `error` (`read_signal_until`). It keeps every other message in arrival order (at most 256) and does not fail the join on it. The signaling listener handles the kept messages first, then reads the socket. So no membership event and no server ICE candidate is lost.

### 7.2 Leave/Close Semantics

Leave must use a proper websocket close handshake:

1. client sends `leave`
2. server sends `left` ack and close control frame (`NormalClosure`)
3. both sides close websocket and release peer/session resources idempotently

This prevents stale legs and lingering half-closed sockets under repeated join/leave churn.

### 7.3 Voice Send and Receive off the Command Loop

The command loop runs every command and the 20 ms voice tick on one task. A command that waits (a Nakama RPC, a hung native call) stops the loop. No voice audio passes through the loop in SFU mode:

| Direction | Path | Thread |
|---|---|---|
| Send | libmello packet sink -> `SfuPacketSink` -> `AudioSendTarget::send` -> `mello_peer_send_audio_frame` | Audio capture thread |
| Receive | Native RTP callback (`mello_peer_set_audio_rtp_callback`) -> `mello_voice_feed_rtp` (`set_direct_voice_sink`) | libdatachannel thread |

Send path objects (`mello-core/src/voice/send_sink.rs`, `transport/sfu_connection.rs`):

- `SfuConnection` owns an `Arc<AudioSendTarget>`. It holds the peer pointer under an `RwLock`, set after the join. A send takes the read lock for one call.
- `VoiceManager::join_voice_sfu` creates an `SfuPacketSink` on that target and sets it as the libmello sink. `VoiceManager` keeps the `Arc` while libmello holds the pointer.
- `leave_voice`, `mark_disconnected_with_reason` and `Drop` clear the libmello sink before the connection goes. The clear waits for a running sink call.
- `SfuConnection::drop` removes the peer from the target under the write lock, before the teardown thread destroys the peer. A sink that still holds the target then skips; it never reaches a destroyed peer.
- The sink logs the first sent frame and the first skipped frame at info level, then every 500th skip. A skip is a frame the peer did not take (track not open yet, or no peer).

Lock order on the capture thread: libmello capture lock, libmello sink lock, the target's read lock, the peer `mutex_`. The command loop takes the sink lock (clear) and the target's write lock (drop) one at a time, never one inside the other. `send_audio_frame` holds the peer `mutex_` only to copy the track pointer. It never takes `negotiation_mutex_`. A renegotiation (`handle_remote_offer` on the signaling task) holds `negotiation_mutex_` and takes `mutex_` only for short copies, so it cannot wait on the capture thread, and the capture thread cannot wait on it.

What stays on the voice tick in SFU mode: the liveness ping and check (every 2 s), debug and capture stats, the SFU event poll (membership, disconnect, and received audio only when the direct voice sink is not set), and a speaking change that waited for its 100 ms interval (section 7.4). When the packet sink cannot be set, the tick sends the queued frames as before and logs an error.

Watchdog: a voice tick in SFU mode longer than 5 ms (`VOICE_TICK_BUDGET`) is logged at warn level with its slowest step (`mic_level`, `sfu_liveness`, `debug_stats`, `packet_send`, `sfu_events`, `sfu_reconnect`, `signal_send`). At most one line per 10 s, with the count of long ticks in between.

### 7.4 Speaking State

In an SFU call the speaking ring travels in-band, one SFU hop:

| Step | Path | Thread |
|---|---|---|
| Local VAD opens or closes | `vad_cb` -> `InbandSpeaking::on_vad` -> `AudioSendTarget::send_control_text` -> `{"type":"speaking","on":true}` | Capture thread |
| SFU | Records the state per peer (for `rtp_blackout`) and sends `{"type":"speaking","user":"<id>","on":true}` to every other member on a change | SFU |
| Remote client | `handle_control_text` -> `Event::VoiceActivity` on the client event channel | libdatachannel thread |

- Rate limit: at most one change per 100 ms (`INBAND_MIN_CHANGE_INTERVAL`). A change inside the interval waits; the voice tick sends it when the interval ends. A flap back to the state already sent cancels the wait. The last state always goes out.
- Push-to-talk: the key sets the state (`Command::VoiceSpeaking` -> `VoiceManager::on_push_to_talk_speaking`), not the VAD.
- Mute sends `on:false`: a muted microphone stops the VAD where it is.
- A member that leaves gets `VoiceActivity { speaking: false }` from the `member_left` handler; the SFU sends no `on:false` for it.
- The SFU sends the members that speak now when the control channel opens. `SfuConnection` keeps them until voice attaches (`set_activity_sink`), so a ring that started before the join completed is not lost.
- The `voice_speaking` Nakama RPC stays for crew members outside the channel (spec 13 section 4.4). In an SFU call it goes at most once per second (`PRESENCE_MIN_CHANGE_INTERVAL`); in P2P at once. Only the local member's own state goes to it.
- The UI keeps the call's speaking states (`AppContext::call_speaking`). In an SFU call they win over the backend's `voice_update` for the members of the joined channel, which lags and can carry a state that already changed.

Measured 2026-10-07 on localhost (control-channel RTT 0 to 1.6 ms), a member joining a channel of 7 talking members on a real SFU (`slots_live_join_and_speaking_latency`, 10 runs):

| Measurement | Result | Plan gate (30 ms RTT) |
|---|---|---|
| SFU `joined` to the first packet the joiner hears, per member | 104 to 134 ms (one run 23 to 35 ms) | under 300 ms |
| SFU `joined` to the first packet a member hears from the joiner | 147 to 170 ms (one run 85 ms) | under 300 ms |
| VAD open to the remote `VoiceActivity` | 0.2 to 2 ms | under 100 ms |

Most of the join time is ICE, DTLS and the DataChannel open. The joiner's microphone starts only after `wait_for_datachannel_open`, which polls every 50 ms.

---

## 8. Public C API (Voice Surface)

Voice controls and packet ingress/egress are exposed through `mello.h`:

```c
MelloResult mello_voice_start_capture(MelloContext* ctx);
MelloResult mello_voice_stop_capture(MelloContext* ctx);
void mello_voice_set_mute(MelloContext* ctx, bool muted);
void mello_voice_set_deafen(MelloContext* ctx, bool deafened);
void mello_voice_set_echo_cancellation(MelloContext* ctx, bool enabled);
void mello_voice_set_echo_suppression(MelloContext* ctx, bool enabled);
void mello_voice_set_agc(MelloContext* ctx, bool enabled);
void mello_voice_set_noise_suppression(MelloContext* ctx, bool enabled);
void mello_voice_set_ns_mode(MelloContext* ctx, MelloNsMode mode);
void mello_voice_set_transient_suppression(MelloContext* ctx, bool enabled);
void mello_voice_set_high_pass_filter(MelloContext* ctx, bool enabled);
void mello_voice_set_input_sensitivity(MelloContext* ctx, float db);
void mello_voice_set_input_sensitivity_auto(MelloContext* ctx, bool enabled);
int  mello_voice_get_packet(MelloContext* ctx, uint8_t* buffer, int buffer_size);
int  mello_voice_get_packet_with_timestamp(MelloContext* ctx, uint8_t* buffer, int buffer_size, uint32_t* timestamp);
MelloResult mello_voice_set_packet_sink(MelloContext* ctx, MelloPacketSinkCallback callback, void* user_data);
MelloResult mello_voice_feed_packet(MelloContext* ctx, const char* peer_id, const uint8_t* data, int size);
MelloResult mello_voice_feed_rtp(MelloContext* ctx, const char* peer_id, const uint8_t* rtp_packet, int size, int64_t receive_time_ms);
void  mello_voice_remove_peer(MelloContext* ctx, const char* peer_id);
void  mello_voice_set_peer_volume(MelloContext* ctx, const char* peer_id, float gain);
void  mello_voice_set_peer_pan(MelloContext* ctx, const char* peer_id, float pan);
float mello_voice_get_peer_volume(MelloContext* ctx, const char* peer_id);
float mello_voice_get_peer_pan(MelloContext* ctx, const char* peer_id);
void  mello_peer_set_audio_rtp_callback(MelloPeerConnection* peer, MelloAudioTrackCallback callback, void* user_data);
// SFU receive slots and the voice control channel (section 7.1)
#define MELLO_VOICE_RECV_SLOTS 16
void  mello_peer_set_voice_recv_slots(MelloPeerConnection* peer, int count);
MelloResult mello_peer_set_audio_mid_sender(MelloPeerConnection* peer, const char* mid, const char* sender_id);
void  mello_peer_set_control_text_callback(MelloPeerConnection* peer, MelloControlTextCallback callback, void* user_data);
MelloResult mello_peer_send_control_text(MelloPeerConnection* peer, const char* text);
```

Controls added with the NetEQ playout (section 5.3):

| Control | C API | mello-core | Client |
|---|---|---|---|
| Per-member volume | `mello_voice_set_peer_volume` | `Command::SetPeerVolume { peer_id, volume }` -> `VoiceManager::set_peer_volume` | Not exposed yet (member context menu, follow-up) |
| Per-member pan | `mello_voice_set_peer_pan` | none (centre) | none |
| Peer left | `mello_voice_remove_peer` | `VoiceManager::on_member_left`, SFU `MemberLeft`, SFU slot release (`handle_control_text`) | - |

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

Endpoint voice quality gate (`scripts/voice-gate.sh`, `tools/voice-gate`):

- drives the real libmello voice path device-free through the test hooks in
  `mello.h` (`MELLO_AUDIO_BACKEND=test`, `mello_voice_test_pull_output`,
  `mello_test_set_clock_ms`, the NetEQ `rx_*` fields of `MelloDebugStats`)
- the sender collects its packets with a packet sink (§4.5) and sends each
  one at its encode time, as the SFU path does. `--send-tick` replays the
  20 ms voice tick of the command loop instead, for comparison
- an impairment shim between sender and receiver builds the RTP packet as
  the SFU path forwards it (16-bit sequence, timestamp, SSRC) and applies
  loss, jitter, stalls, reorder, outage, clock drift and the RTP sequence
  wrap. The receiver takes it with `mello_voice_feed_rtp`
- reports MOS-LQO (PESQ wideband), playout delay over time, concealment
  frames per lost frame, receiver drops and dropouts, with the delta against
  `benchmarks/baselines/voice/baseline-2.9.json`. See `TESTING.md`.

---

## 10. File Structure

```
libmello/src/audio/
├── audio_pipeline.hpp / .cpp
├── voice_playout.hpp / .cpp    # NetEQ per peer, talk-spurt controller, mixer (§5), C++20
├── stream_resampler.hpp / .cpp # PushSincResampler for the WASAPI edges (§6.1), C++20
├── audio_capture.hpp / playback.hpp
├── capture_wasapi.hpp / .cpp
├── playback_wasapi.hpp / .cpp
├── capture_coreaudio.cpp
├── playback_coreaudio.cpp
├── opus_codec.hpp / .cpp        # Opus encoder (voice), decoder (stream audio)
├── noise_suppressor.hpp / .cpp
├── echo_canceller.hpp / .cpp
├── speech_expander.hpp        # post-AGC downward expander (§4.4)
└── vad.hpp / .cpp

libmello/third_party/webrtc-neteq/  # vendored NetEQ, Opus decoder, CNG, mixer (M131)

mello-core/src/
├── voice/mod.rs
├── voice/frame.rs              # P2P and mic test voice frame (§4.2)
├── voice/send_sink.rs          # SFU packet sink on the capture thread (§7.3)
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

---

## 12. Long-Session Health Metrics (v0.3)

Two changes keep audio health decisions accurate over long-running sessions:

- **Windowed underrun rate.** The pipeline keeps the raw lifetime `underrun_count` (monotonic, shown in the debug panel for reference) **and** a rolling ~5s windowed underrun count used for health decisions and the `audio_stats` log line. The lifetime counter climbs forever and is useless for "is audio bad *right now*"; the windowed value reflects current playback health.
- **Late playout is the health signal.** `underrun_count` counts 10 ms mixes in which no peer produced audio (before the first packet, and while a silent peer's NetEQ is in muted state). `rx_late_underruns` counts 10 ms frames in which a peer played voice concealment (NetEQ expand) while at least one packet waited in its NetEQ buffer: a stall, or a loss with later packets present. A talk pause leaves the buffer empty and does not count. The mello-core windowed health and the libmello sustained-late-playout warning read `rx_late_underruns`. With receive timestamps in NetEQ, the talk-start hold no longer counts (0 in the clean gate profile).
- **Renegotiation-tolerant track reads (SFU side).** The SFU's consecutive-EOF track cutoff is raised/paused during active renegotiation so transient read errors don't tear down a healthy inbound audio track. See [SFU-INTEGRATION.md](./features/SFU-INTEGRATION.md).

