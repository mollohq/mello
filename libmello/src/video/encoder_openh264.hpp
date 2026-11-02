#pragma once
#include "encoder.hpp"

#ifdef _WIN32
#include <d3d11.h>
#include <wrl/client.h>
#include <atomic>
#include <chrono>
#include <cstdint>
#include <vector>

namespace mello::video {

namespace wels_abi {
class ISVCEncoder;
}

/// Software H.264 encoder: Cisco's prebuilt OpenH264 DLL, the same DLL that
/// `openh264_dynload.cpp` loads for decode (`WelsCreateSVCEncoder`).
///
/// Test lane only. The factory selects it only when `MELLO_SOFT_ENCODER=1`
/// (see `soft_encoder_requested()`), so the streaming quality gate can run
/// the transport and the viewer with no hardware encoder. Production keeps
/// the rule of spec 03 section 5: hardware encode only.
///
/// Low-latency settings: one spatial and one temporal layer, one slice, no
/// B-frames (OpenH264 has none), bitrate rate control without frame skip,
/// an IDR every `keyframe_interval` frames and on request, constant SPS/PPS
/// ids so a decoder can join at any IDR.
///
/// Threads. `initialize`, `encode` and `shutdown` run on the encode thread
/// (`initialize` runs before that thread starts). `request_keyframe`,
/// `set_bitrate` and `set_framerate` may run on any thread: they only store
/// atomics, and `encode` applies them before the next frame. The DLL object
/// is touched by the encode thread only.
class OpenH264Encoder : public Encoder {
public:
    OpenH264Encoder() = default;
    ~OpenH264Encoder() override;

    /// True when the DLL loads and exports the encoder entry points.
    static bool is_available();

    bool        initialize(const GraphicsDevice& device, const EncoderConfig& config) override;
    void        shutdown() override;
    bool        encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) override;
    void        request_keyframe() override;
    void        set_bitrate(uint32_t kbps) override;
    void        get_stats(EncoderStats& out) const override;
    bool        supports_codec(VideoCodec codec) const override { return codec == VideoCodec::H264; }
    const char* name() const override { return "OpenH264"; }
    void        set_framerate(uint32_t fps) override;
    void        get_phase_timing(EncodePhaseTiming& out) const override;

private:
    bool read_back(ID3D11Texture2D* nv12_texture);
    void apply_pending_controls();

    wels_abi::ISVCEncoder* encoder_ = nullptr;
    EncoderConfig          config_{};

    Microsoft::WRL::ComPtr<ID3D11Device>        device_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D>     staging_;

    // I420 planes of the current input frame.
    std::vector<uint8_t> y_, u_, v_;

    std::atomic<bool>     force_idr_{false};
    std::atomic<uint32_t> pending_bitrate_kbps_{0};
    std::atomic<uint32_t> pending_fps_{0};
    uint32_t              applied_fps_ = 0;

    uint64_t     frame_seq_ = 0;
    EncoderStats stats_{};
    std::chrono::steady_clock::time_point stats_window_start_{};
    uint32_t     stats_window_frames_ = 0;
    uint64_t     stats_window_bytes_  = 0;
    EncodePhaseTiming phases_{};
};

} // namespace mello::video
#endif

#if defined(__linux__)
#include <atomic>
#include <chrono>
#include <cstdint>
#include <vector>

namespace mello::video {

namespace wels_abi {
class ISVCEncoder;
}

/// Linux: the software encoder of the Windows test lane, fed from a CPU
/// picture (encoder_openh264_linux.cpp). The same rules: the factory picks it
/// only with `MELLO_SOFT_ENCODER=1`, and the same low-latency settings. The
/// picture is converted and scaled to I420 on the CPU.
///
/// Threads: `initialize`, `encode` and `shutdown` on the capture thread (the
/// caller of start_host for `initialize`); the setters store atomics.
class OpenH264Encoder : public Encoder {
public:
    OpenH264Encoder() = default;
    ~OpenH264Encoder() override;

    static bool is_available();

    bool        initialize(const GraphicsDevice& device, const EncoderConfig& config) override;
    void        shutdown() override;
    bool        encode(const CaptureFrame& frame, uint64_t capture_us, EncodedPacket& out) override;
    void        request_keyframe() override;
    void        set_bitrate(uint32_t kbps) override;
    void        get_stats(EncoderStats& out) const override;
    bool        supports_codec(VideoCodec codec) const override { return codec == VideoCodec::H264; }
    const char* name() const override { return "OpenH264"; }
    void        set_framerate(uint32_t fps) override;
    void        get_phase_timing(EncodePhaseTiming& out) const override;

private:
    void apply_pending_controls();

    wels_abi::ISVCEncoder* encoder_ = nullptr;
    EncoderConfig          config_{};

    // I420 planes of the current input frame, at the encode size.
    std::vector<uint8_t> y_, u_, v_;

    std::atomic<bool>     force_idr_{false};
    std::atomic<uint32_t> pending_bitrate_kbps_{0};
    std::atomic<uint32_t> pending_fps_{0};
    uint32_t              applied_fps_ = 0;

    uint64_t     frame_seq_ = 0;
    EncoderStats stats_{};
    std::chrono::steady_clock::time_point stats_window_start_{};
    uint32_t     stats_window_frames_ = 0;
    uint64_t     stats_window_bytes_  = 0;
    EncodePhaseTiming phases_{};
};

} // namespace mello::video
#endif
