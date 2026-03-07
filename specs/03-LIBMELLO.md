# LIBMELLO Specification

> **Component:** libmello (Low-Level C++ Library)  
> **Language:** C++17  
> **Status:** Beta Scope  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

libmello is the C++ library that handles all low-level audio/video capture, encoding, decoding, and P2P transport. It exposes a pure C API for FFI compatibility with Rust and future mobile platforms.

**Key Responsibilities:**
- Audio capture (WASAPI), processing (RNNoise, Silero VAD), and encoding (Opus)
- Video capture (DXGI), encoding (NVENC/AMF/QSV), and decoding
- P2P transport (libdatachannel)
- ICE/STUN/TURN connectivity

---

## 2. Project Structure

```
libmello/
├── CMakeLists.txt
├── include/
│   └── mello.h                     # Public C API (single header)
│
├── src/
│   ├── mello.cpp                   # API implementation
│   ├── context.hpp                 # Internal context
│   │
│   ├── audio/
│   │   ├── audio_pipeline.hpp
│   │   ├── audio_pipeline.cpp
│   │   ├── capture_wasapi.hpp      # Windows audio capture
│   │   ├── capture_wasapi.cpp
│   │   ├── playback_wasapi.hpp     # Windows audio playback
│   │   ├── playback_wasapi.cpp
│   │   ├── processing.hpp          # RNNoise + VAD wrapper
│   │   ├── processing.cpp
│   │   ├── opus_encoder.hpp
│   │   ├── opus_encoder.cpp
│   │   ├── opus_decoder.hpp
│   │   ├── opus_decoder.cpp
│   │   └── jitter_buffer.hpp       # Audio jitter buffer
│   │
│   ├── video/
│   │   ├── video_pipeline.hpp
│   │   ├── video_pipeline.cpp
│   │   ├── capture_dxgi.hpp        # Desktop Duplication API
│   │   ├── capture_dxgi.cpp
│   │   ├── encoder.hpp             # Abstract encoder
│   │   ├── encoder_nvenc.hpp       # NVIDIA NVENC
│   │   ├── encoder_nvenc.cpp
│   │   ├── encoder_amf.hpp         # AMD AMF
│   │   ├── encoder_amf.cpp
│   │   ├── encoder_qsv.hpp         # Intel Quick Sync
│   │   ├── encoder_qsv.cpp
│   │   ├── decoder.hpp             # Hardware decoder
│   │   ├── decoder.cpp
│   │   └── color_convert.hpp       # GPU color conversion
│   │
│   ├── transport/
│   │   ├── transport.hpp
│   │   ├── transport.cpp
│   │   ├── peer_connection.hpp     # libdatachannel wrapper
│   │   ├── peer_connection.cpp
│   │   ├── signaling.hpp           # Signal message types
│   │   └── ice_config.hpp          # STUN/TURN configuration
│   │
│   └── util/
│       ├── logger.hpp
│       ├── thread_pool.hpp
│       └── ring_buffer.hpp
│
├── deps/
│   ├── libdatachannel/             # Git submodule
│   ├── opus/                       # Git submodule
│   ├── rnnoise/                    # Git submodule
│   └── silero-vad/                 # ONNX model + runtime
│
└── tests/
    ├── test_audio_pipeline.cpp
    ├── test_video_pipeline.cpp
    └── test_peer_connection.cpp
```

---

## 3. Public C API

### 3.1 Header File

```c
// include/mello.h

#ifndef MELLO_H
#define MELLO_H

#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

// ============================================================================
// TYPES
// ============================================================================

typedef struct MelloContext MelloContext;
typedef struct MelloVoiceSession MelloVoiceSession;
typedef struct MelloStreamHost MelloStreamHost;
typedef struct MelloStreamView MelloStreamView;
typedef struct MelloPeerConnection MelloPeerConnection;

typedef enum MelloResult {
    MELLO_OK = 0,
    MELLO_ERROR_INVALID_PARAM = -1,
    MELLO_ERROR_NOT_INITIALIZED = -2,
    MELLO_ERROR_ALREADY_STARTED = -3,
    MELLO_ERROR_CAPTURE_FAILED = -4,
    MELLO_ERROR_ENCODE_FAILED = -5,
    MELLO_ERROR_DECODE_FAILED = -6,
    MELLO_ERROR_TRANSPORT_FAILED = -7,
    MELLO_ERROR_NO_HARDWARE = -8,
} MelloResult;

typedef enum MelloEncoderType {
    MELLO_ENCODER_AUTO = 0,     // Auto-detect best available
    MELLO_ENCODER_NVENC = 1,    // NVIDIA NVENC
    MELLO_ENCODER_AMF = 2,      // AMD AMF
    MELLO_ENCODER_QSV = 3,      // Intel Quick Sync
} MelloEncoderType;

typedef struct MelloVideoFrame {
    uint8_t* data;              // RGBA pixel data
    uint32_t width;
    uint32_t height;
    uint32_t stride;            // Bytes per row
    uint64_t timestamp_us;      // Microseconds
} MelloVideoFrame;

typedef struct MelloAudioFrame {
    int16_t* data;              // PCM samples (interleaved stereo)
    uint32_t sample_count;      // Samples per channel
    uint32_t sample_rate;       // Usually 48000
    uint32_t channels;          // Usually 2
    uint64_t timestamp_us;
} MelloAudioFrame;

typedef struct MelloStreamConfig {
    uint32_t width;
    uint32_t height;
    uint32_t fps;
    uint32_t bitrate_kbps;
    MelloEncoderType encoder;
} MelloStreamConfig;

typedef struct MelloIceCandidate {
    const char* candidate;
    const char* sdp_mid;
    int sdp_mline_index;
} MelloIceCandidate;

// Callbacks
typedef void (*MelloVoiceActivityCallback)(void* user_data, bool speaking);
typedef void (*MelloAudioFrameCallback)(void* user_data, const MelloAudioFrame* frame);
typedef void (*MelloVideoFrameCallback)(void* user_data, const MelloVideoFrame* frame);
typedef void (*MelloIceCandidateCallback)(void* user_data, const MelloIceCandidate* candidate);
typedef void (*MelloPeerStateCallback)(void* user_data, int state);

// ============================================================================
// CONTEXT
// ============================================================================

/// Initialize libmello. Call once at startup.
MelloContext* mello_init(void);

/// Shutdown and free resources.
void mello_destroy(MelloContext* ctx);

/// Get last error message.
const char* mello_get_error(MelloContext* ctx);

// ============================================================================
// VOICE
// ============================================================================

/// Start audio capture from default microphone.
MelloResult mello_voice_start_capture(MelloContext* ctx);

/// Stop audio capture.
MelloResult mello_voice_stop_capture(MelloContext* ctx);

/// Set mute state (stops sending audio, still captures for VAD).
void mello_voice_set_mute(MelloContext* ctx, bool muted);

/// Set deafen state (stops receiving audio).
void mello_voice_set_deafen(MelloContext* ctx, bool deafened);

/// Check if local user is currently speaking (VAD).
bool mello_voice_is_speaking(MelloContext* ctx);

/// Set callback for voice activity detection.
void mello_voice_set_vad_callback(
    MelloContext* ctx,
    MelloVoiceActivityCallback callback,
    void* user_data
);

/// Get encoded audio packet to send to peers.
/// Returns packet size, or 0 if no packet available.
int mello_voice_get_packet(MelloContext* ctx, uint8_t* buffer, int buffer_size);

/// Feed received audio packet from a peer.
MelloResult mello_voice_feed_packet(
    MelloContext* ctx,
    const char* peer_id,
    const uint8_t* data,
    int size
);

// ============================================================================
// STREAMING (HOST)
// ============================================================================

/// Start hosting a stream (capturing and encoding).
MelloStreamHost* mello_stream_start_host(
    MelloContext* ctx,
    const MelloStreamConfig* config
);

/// Stop hosting.
void mello_stream_stop_host(MelloStreamHost* host);

/// Get encoded video packet to send to viewers.
/// Returns packet size, or 0 if no packet available.
int mello_stream_get_video_packet(
    MelloStreamHost* host,
    uint8_t* buffer,
    int buffer_size,
    bool* is_keyframe
);

/// Request a keyframe (e.g., when new viewer joins).
void mello_stream_request_keyframe(MelloStreamHost* host);

// ============================================================================
// STREAMING (VIEWER)
// ============================================================================

/// Start viewing a stream.
MelloStreamView* mello_stream_start_view(MelloContext* ctx);

/// Stop viewing.
void mello_stream_stop_view(MelloStreamView* view);

/// Feed received video packet.
MelloResult mello_stream_feed_video_packet(
    MelloStreamView* view,
    const uint8_t* data,
    int size,
    bool is_keyframe
);

/// Get the latest decoded frame.
/// Returns true if a new frame is available.
bool mello_stream_get_frame(MelloStreamView* view, MelloVideoFrame* frame);

/// Free frame data after use.
void mello_stream_free_frame(MelloVideoFrame* frame);

// ============================================================================
// P2P TRANSPORT
// ============================================================================

/// Create a new peer connection.
MelloPeerConnection* mello_peer_create(MelloContext* ctx, const char* peer_id);

/// Destroy a peer connection.
void mello_peer_destroy(MelloPeerConnection* peer);

/// Set ICE servers (STUN/TURN).
void mello_peer_set_ice_servers(
    MelloPeerConnection* peer,
    const char** urls,
    int count
);

/// Create an offer (caller side).
const char* mello_peer_create_offer(MelloPeerConnection* peer);

/// Create an answer (callee side).
const char* mello_peer_create_answer(MelloPeerConnection* peer, const char* offer_sdp);

/// Set remote description (offer or answer).
MelloResult mello_peer_set_remote_description(
    MelloPeerConnection* peer,
    const char* sdp,
    bool is_offer
);

/// Add a remote ICE candidate.
MelloResult mello_peer_add_ice_candidate(
    MelloPeerConnection* peer,
    const MelloIceCandidate* candidate
);

/// Set callback for local ICE candidates.
void mello_peer_set_ice_callback(
    MelloPeerConnection* peer,
    MelloIceCandidateCallback callback,
    void* user_data
);

/// Set callback for connection state changes.
void mello_peer_set_state_callback(
    MelloPeerConnection* peer,
    MelloPeerStateCallback callback,
    void* user_data
);

/// Send data on unreliable channel (video, audio).
MelloResult mello_peer_send_unreliable(
    MelloPeerConnection* peer,
    const uint8_t* data,
    int size
);

/// Send data on reliable channel (control messages).
MelloResult mello_peer_send_reliable(
    MelloPeerConnection* peer,
    const uint8_t* data,
    int size
);

/// Poll for received data. Returns size, or 0 if nothing available.
int mello_peer_recv(
    MelloPeerConnection* peer,
    uint8_t* buffer,
    int buffer_size,
    bool* is_reliable
);

// ============================================================================
// DEVICES
// ============================================================================

typedef struct MelloDevice {
    const char* id;
    const char* name;
    bool is_default;
} MelloDevice;

/// Get available audio input devices.
int mello_get_audio_inputs(MelloContext* ctx, MelloDevice* devices, int max_count);

/// Get available audio output devices.
int mello_get_audio_outputs(MelloContext* ctx, MelloDevice* devices, int max_count);

/// Set audio input device.
MelloResult mello_set_audio_input(MelloContext* ctx, const char* device_id);

/// Set audio output device.
MelloResult mello_set_audio_output(MelloContext* ctx, const char* device_id);

/// Get available video encoders.
int mello_get_encoders(MelloContext* ctx, MelloEncoderType* encoders, int max_count);

#ifdef __cplusplus
}
#endif

#endif // MELLO_H
```

---

## 4. Audio Pipeline

### 4.1 Overview

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         AUDIO PIPELINE                                  │
│                                                                         │
│  CAPTURE PATH:                                                          │
│  ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐   │
│  │ WASAPI  │──▶│  Echo   │──▶│ RNNoise │──▶│ Silero  │──▶│  Opus   │   │
│  │ Capture │   │ Cancel  │   │ Denoise │   │   VAD   │   │ Encode  │   │
│  └─────────┘   └─────────┘   └─────────┘   └─────────┘   └─────────┘   │
│                                                │              │         │
│                                                ▼              ▼         │
│                                          VAD Callback    Packets Out   │
│                                                                         │
│  PLAYBACK PATH:                                                         │
│  ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐                 │
│  │ Packets │──▶│  Opus   │──▶│ Jitter  │──▶│ WASAPI  │                 │
│  │   In    │   │ Decode  │   │ Buffer  │   │ Playback│                 │
│  └─────────┘   └─────────┘   └─────────┘   └─────────┘                 │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 4.2 Audio Capture (WASAPI)

```cpp
// src/audio/capture_wasapi.hpp

#pragma once
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <functional>
#include <thread>
#include <atomic>

namespace mello::audio {

class WasapiCapture {
public:
    using Callback = std::function<void(const int16_t* samples, size_t count)>;
    
    WasapiCapture();
    ~WasapiCapture();
    
    bool initialize(const char* device_id = nullptr);  // nullptr = default
    bool start(Callback callback);
    void stop();
    
    uint32_t sample_rate() const { return sample_rate_; }
    uint32_t channels() const { return channels_; }
    
private:
    void capture_thread();
    
    IMMDevice* device_ = nullptr;
    IAudioClient* audio_client_ = nullptr;
    IAudioCaptureClient* capture_client_ = nullptr;
    
    uint32_t sample_rate_ = 48000;
    uint32_t channels_ = 2;
    uint32_t buffer_frames_ = 0;
    
    std::thread thread_;
    std::atomic<bool> running_{false};
    Callback callback_;
};

} // namespace mello::audio
```

### 4.3 Audio Processing

```cpp
// src/audio/processing.hpp

#pragma once
#include <rnnoise.h>
#include <onnxruntime_cxx_api.h>
#include <memory>
#include <vector>

namespace mello::audio {

class AudioProcessor {
public:
    AudioProcessor();
    ~AudioProcessor();
    
    /// Initialize with sample rate (must be 48000 for RNNoise)
    bool initialize(uint32_t sample_rate);
    
    /// Process audio frame: denoise + VAD
    /// Input: interleaved stereo, Output: denoised interleaved stereo
    /// Returns VAD probability (0.0 to 1.0)
    float process(const int16_t* input, int16_t* output, size_t sample_count);
    
    /// Check if currently speaking (based on recent VAD)
    bool is_speaking() const;
    
private:
    // RNNoise (one per channel)
    DenoiseState* rnnoise_left_ = nullptr;
    DenoiseState* rnnoise_right_ = nullptr;
    
    // Silero VAD
    Ort::Env ort_env_;
    std::unique_ptr<Ort::Session> vad_session_;
    std::vector<float> vad_state_;  // LSTM state
    
    // VAD smoothing
    float vad_probability_ = 0.0f;
    int speech_frames_ = 0;
    int silence_frames_ = 0;
    bool speaking_ = false;
    
    // Thresholds
    static constexpr float VAD_THRESHOLD = 0.5f;
    static constexpr int SPEECH_FRAMES_THRESHOLD = 3;
    static constexpr int SILENCE_FRAMES_THRESHOLD = 15;
};

} // namespace mello::audio
```

### 4.4 Opus Encoding

```cpp
// src/audio/opus_encoder.hpp

#pragma once
#include <opus.h>
#include <cstdint>
#include <vector>

namespace mello::audio {

class OpusEncoder {
public:
    OpusEncoder();
    ~OpusEncoder();
    
    /// Initialize encoder
    /// @param sample_rate 48000 recommended
    /// @param channels 1 or 2
    /// @param bitrate Target bitrate in bps (e.g., 64000 for 64kbps)
    bool initialize(uint32_t sample_rate, uint32_t channels, uint32_t bitrate);
    
    /// Encode a frame of audio
    /// @param input PCM samples (frame_size samples per channel)
    /// @param frame_size Samples per channel (must be 2.5, 5, 10, 20, 40, 60 ms)
    /// @param output Output buffer for encoded data
    /// @param max_output_size Maximum output buffer size
    /// @return Encoded size in bytes, or negative on error
    int encode(
        const int16_t* input,
        int frame_size,
        uint8_t* output,
        int max_output_size
    );
    
    /// Set bitrate dynamically
    void set_bitrate(uint32_t bitrate);
    
private:
    ::OpusEncoder* encoder_ = nullptr;
    uint32_t sample_rate_ = 0;
    uint32_t channels_ = 0;
};

class OpusDecoder {
public:
    OpusDecoder();
    ~OpusDecoder();
    
    bool initialize(uint32_t sample_rate, uint32_t channels);
    
    /// Decode a packet
    /// @return Number of samples per channel decoded, or negative on error
    int decode(
        const uint8_t* input,
        int input_size,
        int16_t* output,
        int max_frame_size
    );
    
    /// Decode with packet loss concealment (no input)
    int decode_plc(int16_t* output, int frame_size);
    
private:
    ::OpusDecoder* decoder_ = nullptr;
    uint32_t sample_rate_ = 0;
    uint32_t channels_ = 0;
};

} // namespace mello::audio
```

---

## 5. Video Pipeline

### 5.1 Overview

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         VIDEO PIPELINE (HOST)                           │
│                                                                         │
│  ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐                 │
│  │  DXGI   │──▶│  Color  │──▶│ Hardware│──▶│ Packet  │                 │
│  │ Capture │   │ Convert │   │ Encode  │   │ Queue   │                 │
│  │         │   │ (GPU)   │   │ NVENC/  │   │         │                 │
│  │ D3D11   │   │ BGRA→   │   │ AMF/QSV │   │         │                 │
│  │ Texture │   │ NV12    │   │         │   │         │                 │
│  └─────────┘   └─────────┘   └─────────┘   └─────────┘                 │
│      ▲                                          │                       │
│      │ Zero-copy in VRAM                        ▼                       │
│                                            To Viewers                   │
└─────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────┐
│                         VIDEO PIPELINE (VIEWER)                         │
│                                                                         │
│  ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐                 │
│  │ Packets │──▶│ Hardware│──▶│  Color  │──▶│  Frame  │                 │
│  │   In    │   │ Decode  │   │ Convert │   │ Buffer  │                 │
│  │         │   │ DXVA2/  │   │ (GPU)   │   │         │                 │
│  │         │   │ NVDEC   │   │ NV12→   │   │ RGBA    │                 │
│  │         │   │         │   │ RGBA    │   │ Pixels  │                 │
│  └─────────┘   └─────────┘   └─────────┘   └─────────┘                 │
│                                                 │                       │
│                                                 ▼                       │
│                                            To UI                        │
└─────────────────────────────────────────────────────────────────────────┘
```

### 5.2 DXGI Capture

```cpp
// src/video/capture_dxgi.hpp

#pragma once
#include <d3d11.h>
#include <dxgi1_2.h>
#include <wrl/client.h>
#include <functional>
#include <thread>
#include <atomic>

using Microsoft::WRL::ComPtr;

namespace mello::video {

class DxgiCapture {
public:
    using FrameCallback = std::function<void(ID3D11Texture2D* texture, uint64_t timestamp)>;
    
    DxgiCapture();
    ~DxgiCapture();
    
    /// Initialize capture for primary display
    bool initialize();
    
    /// Start capturing at target FPS
    bool start(uint32_t target_fps, FrameCallback callback);
    
    /// Stop capturing
    void stop();
    
    uint32_t width() const { return width_; }
    uint32_t height() const { return height_; }
    
    ID3D11Device* device() const { return device_.Get(); }
    ID3D11DeviceContext* context() const { return context_.Get(); }
    
private:
    void capture_thread();
    
    ComPtr<ID3D11Device> device_;
    ComPtr<ID3D11DeviceContext> context_;
    ComPtr<IDXGIOutputDuplication> duplication_;
    
    uint32_t width_ = 0;
    uint32_t height_ = 0;
    uint32_t target_fps_ = 60;
    
    std::thread thread_;
    std::atomic<bool> running_{false};
    FrameCallback callback_;
};

} // namespace mello::video
```

### 5.3 Hardware Encoder (Abstract)

```cpp
// src/video/encoder.hpp

#pragma once
#include <d3d11.h>
#include <cstdint>
#include <vector>

namespace mello::video {

struct EncoderConfig {
    uint32_t width;
    uint32_t height;
    uint32_t fps;
    uint32_t bitrate_kbps;
    uint32_t keyframe_interval = 60;  // Frames between keyframes
};

struct EncodedPacket {
    std::vector<uint8_t> data;
    uint64_t timestamp_us;
    bool is_keyframe;
};

class Encoder {
public:
    virtual ~Encoder() = default;
    
    virtual bool initialize(ID3D11Device* device, const EncoderConfig& config) = 0;
    virtual void shutdown() = 0;
    
    /// Encode a frame (texture must be in NV12 format)
    virtual bool encode(ID3D11Texture2D* texture, EncodedPacket& out) = 0;
    
    /// Request next frame to be a keyframe
    virtual void request_keyframe() = 0;
    
    /// Update bitrate dynamically
    virtual void set_bitrate(uint32_t kbps) = 0;
    
    /// Get encoder type
    virtual const char* name() const = 0;
};

/// Detect available encoders and create the best one
std::unique_ptr<Encoder> create_best_encoder();

/// Create a specific encoder
std::unique_ptr<Encoder> create_encoder(int type);  // MELLO_ENCODER_*

} // namespace mello::video
```

### 5.4 NVENC Encoder

```cpp
// src/video/encoder_nvenc.hpp

#pragma once
#include "encoder.hpp"
#include <nvEncodeAPI.h>

namespace mello::video {

class NvencEncoder : public Encoder {
public:
    NvencEncoder();
    ~NvencEncoder() override;
    
    bool initialize(ID3D11Device* device, const EncoderConfig& config) override;
    void shutdown() override;
    bool encode(ID3D11Texture2D* texture, EncodedPacket& out) override;
    void request_keyframe() override;
    void set_bitrate(uint32_t kbps) override;
    const char* name() const override { return "NVENC"; }
    
    static bool is_available();
    
private:
    void* encoder_ = nullptr;  // NV_ENCODE_API_FUNCTION_LIST
    NV_ENC_INITIALIZE_PARAMS init_params_{};
    NV_ENC_CONFIG encode_config_{};
    
    // Input/output buffers
    NV_ENC_REGISTERED_PTR registered_resource_ = nullptr;
    NV_ENC_INPUT_PTR input_buffer_ = nullptr;
    NV_ENC_OUTPUT_PTR output_buffer_ = nullptr;
    
    bool force_keyframe_ = false;
};

} // namespace mello::video
```

---

## 6. Transport Layer

### 6.1 libdatachannel Wrapper

```cpp
// src/transport/peer_connection.hpp

#pragma once
#include <rtc/rtc.hpp>
#include <string>
#include <functional>
#include <memory>
#include <mutex>
#include <queue>

namespace mello::transport {

struct IceConfig {
    std::vector<std::string> stun_servers;
    std::vector<std::string> turn_servers;
    std::string turn_username;
    std::string turn_password;
};

class PeerConnection {
public:
    using IceCallback = std::function<void(const std::string& candidate, 
                                           const std::string& mid, 
                                           int mline_index)>;
    using StateCallback = std::function<void(rtc::PeerConnection::State)>;
    using DataCallback = std::function<void(const uint8_t* data, size_t size, bool reliable)>;
    
    PeerConnection(const std::string& peer_id, const IceConfig& config);
    ~PeerConnection();
    
    const std::string& peer_id() const { return peer_id_; }
    
    // Signaling
    std::string create_offer();
    std::string create_answer(const std::string& offer_sdp);
    bool set_remote_description(const std::string& sdp, bool is_offer);
    bool add_ice_candidate(const std::string& candidate, 
                           const std::string& mid, 
                           int mline_index);
    
    // Callbacks
    void set_ice_callback(IceCallback cb);
    void set_state_callback(StateCallback cb);
    void set_data_callback(DataCallback cb);
    
    // Data channels
    bool send_reliable(const uint8_t* data, size_t size);
    bool send_unreliable(const uint8_t* data, size_t size);
    
    // State
    bool is_connected() const;
    
private:
    std::string peer_id_;
    std::shared_ptr<rtc::PeerConnection> pc_;
    std::shared_ptr<rtc::DataChannel> reliable_channel_;
    std::shared_ptr<rtc::DataChannel> unreliable_channel_;
    
    IceCallback ice_callback_;
    StateCallback state_callback_;
    DataCallback data_callback_;
    
    std::mutex mutex_;
};

} // namespace mello::transport
```

---

## 7. Context Implementation

```cpp
// src/context.hpp

#pragma once
#include "audio/audio_pipeline.hpp"
#include "video/video_pipeline.hpp"
#include "transport/peer_connection.hpp"
#include <unordered_map>
#include <mutex>

namespace mello {

class Context {
public:
    Context();
    ~Context();
    
    bool initialize();
    void shutdown();
    
    // Audio
    audio::AudioPipeline& audio() { return audio_; }
    
    // Video
    video::VideoPipeline& video() { return video_; }
    
    // Peers
    transport::PeerConnection* create_peer(const std::string& peer_id);
    transport::PeerConnection* get_peer(const std::string& peer_id);
    void destroy_peer(const std::string& peer_id);
    
    // Error handling
    void set_error(const std::string& error);
    const char* get_error() const;
    
private:
    audio::AudioPipeline audio_;
    video::VideoPipeline video_;
    
    std::unordered_map<std::string, std::unique_ptr<transport::PeerConnection>> peers_;
    std::mutex peers_mutex_;
    
    transport::IceConfig ice_config_;
    std::string last_error_;
};

} // namespace mello
```

---

## 8. CMake Build

```cmake
# CMakeLists.txt

cmake_minimum_required(VERSION 3.20)
project(mello VERSION 0.1.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Options
option(MELLO_BUILD_TESTS "Build tests" ON)
option(MELLO_ENABLE_NVENC "Enable NVIDIA NVENC" ON)
option(MELLO_ENABLE_AMF "Enable AMD AMF" ON)
option(MELLO_ENABLE_QSV "Enable Intel Quick Sync" ON)

# Dependencies
add_subdirectory(deps/libdatachannel)
add_subdirectory(deps/opus)
add_subdirectory(deps/rnnoise)

# Find ONNX Runtime for Silero VAD
find_package(onnxruntime REQUIRED)

# Windows SDK
if(WIN32)
    find_package(DirectX REQUIRED)
endif()

# Sources
set(MELLO_SOURCES
    src/mello.cpp
    src/context.cpp
    
    src/audio/audio_pipeline.cpp
    src/audio/capture_wasapi.cpp
    src/audio/playback_wasapi.cpp
    src/audio/processing.cpp
    src/audio/opus_encoder.cpp
    src/audio/opus_decoder.cpp
    src/audio/jitter_buffer.cpp
    
    src/video/video_pipeline.cpp
    src/video/capture_dxgi.cpp
    src/video/decoder.cpp
    src/video/color_convert.cpp
    
    src/transport/peer_connection.cpp
)

# Encoder sources (conditional)
if(MELLO_ENABLE_NVENC)
    list(APPEND MELLO_SOURCES src/video/encoder_nvenc.cpp)
endif()
if(MELLO_ENABLE_AMF)
    list(APPEND MELLO_SOURCES src/video/encoder_amf.cpp)
endif()
if(MELLO_ENABLE_QSV)
    list(APPEND MELLO_SOURCES src/video/encoder_qsv.cpp)
endif()

# Library
add_library(mello SHARED ${MELLO_SOURCES})

target_include_directories(mello
    PUBLIC include
    PRIVATE src
)

target_link_libraries(mello
    PRIVATE
        datachannel
        opus
        rnnoise
        onnxruntime::onnxruntime
        d3d11
        dxgi
        mfplat
        mfuuid
)

# Install
install(TARGETS mello RUNTIME DESTINATION bin LIBRARY DESTINATION lib)
install(FILES include/mello.h DESTINATION include)

# Tests
if(MELLO_BUILD_TESTS)
    enable_testing()
    add_subdirectory(tests)
endif()
```

---

## 9. Performance Targets

| Metric | Target |
|--------|--------|
| Audio capture latency | <10ms |
| RNNoise + VAD processing | <5ms per 20ms frame |
| Opus encode | <2ms per 20ms frame |
| Video capture (DXGI) | <2ms |
| Video encode (NVENC) | <5ms |
| Video decode (DXVA2) | <5ms |
| P2P round-trip | <5ms (local) |
| Library size | <5MB |

---

## 10. Thread Model

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          THREAD MODEL                                   │
│                                                                         │
│  ┌─────────────────┐                                                    │
│  │   Main Thread   │  ← API calls, state management                    │
│  └─────────────────┘                                                    │
│                                                                         │
│  ┌─────────────────┐                                                    │
│  │  Audio Capture  │  ← WASAPI event-driven, ~20ms wakeup              │
│  │     Thread      │                                                    │
│  └─────────────────┘                                                    │
│                                                                         │
│  ┌─────────────────┐                                                    │
│  │ Audio Playback  │  ← WASAPI event-driven                            │
│  │     Thread      │                                                    │
│  └─────────────────┘                                                    │
│                                                                         │
│  ┌─────────────────┐                                                    │
│  │  Video Capture  │  ← ~16ms wakeup for 60fps                         │
│  │     Thread      │                                                    │
│  └─────────────────┘                                                    │
│                                                                         │
│  ┌─────────────────┐                                                    │
│  │  Video Encode   │  ← Processes captured frames                      │
│  │     Thread      │                                                    │
│  └─────────────────┘                                                    │
│                                                                         │
│  ┌─────────────────┐                                                    │
│  │  Network I/O    │  ← libdatachannel internal threads                │
│  │   (internal)    │                                                    │
│  └─────────────────┘                                                    │
│                                                                         │
│  Thread-safe queues connect threads. Minimal locking.                  │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 11. Testing

```cpp
// tests/test_audio_pipeline.cpp

#include <gtest/gtest.h>
#include "audio/processing.hpp"

TEST(AudioProcessing, DenoiseReducesNoise) {
    mello::audio::AudioProcessor processor;
    ASSERT_TRUE(processor.initialize(48000));
    
    // Generate noisy signal
    std::vector<int16_t> noisy(960 * 2);  // 20ms stereo
    // ... fill with noise + speech
    
    std::vector<int16_t> clean(960 * 2);
    float vad = processor.process(noisy.data(), clean.data(), 960);
    
    // Verify noise reduction
    // ...
}

TEST(AudioProcessing, VadDetectsSpeech) {
    mello::audio::AudioProcessor processor;
    ASSERT_TRUE(processor.initialize(48000));
    
    // Generate speech signal
    // ...
    
    float vad = processor.process(speech.data(), output.data(), 960);
    EXPECT_GT(vad, 0.5f);
}
```

---

*This spec defines libmello. For backend infrastructure, see [04-BACKEND.md](./04-BACKEND.md).*
