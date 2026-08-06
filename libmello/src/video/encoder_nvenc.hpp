#pragma once
#include "encoder.hpp"

#ifdef _WIN32
#include <wrl/client.h>
#include <nvEncodeAPI.h>
#include <chrono>
#include <unordered_map>

namespace mello::video {

class NvencEncoder : public Encoder {
public:
    bool        initialize(const GraphicsDevice& device, const EncoderConfig& config) override;
    void        shutdown() override;
    bool        encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) override;
    void        request_keyframe() override;
    void        set_bitrate(uint32_t kbps) override;
    void        get_stats(EncoderStats& out) const override;
    bool        supports_codec(VideoCodec codec) const override;
    const char* name() const override { return "NVENC"; }

    static bool is_available();

private:
    NV_ENC_REGISTERED_PTR get_or_register(ID3D11Texture2D* tex);

    HMODULE dll_ = nullptr;

    NV_ENCODE_API_FUNCTION_LIST fn_{};
    void*                       encoder_   = nullptr;
    NV_ENC_OUTPUT_PTR           out_buf_   = nullptr;
    NV_ENC_INPUT_PTR            mapped_input_ = nullptr;

    // Async encode: completion event signalled by NVENC when bitstream is ready
    HANDLE completion_event_ = nullptr;
    bool   async_mode_ = false;

    // Per-texture registration cache: avoids re-registering the same NV12 ring slot
    std::unordered_map<ID3D11Texture2D*, NV_ENC_REGISTERED_PTR> reg_cache_;

    bool  force_idr_  = false;
    EncoderConfig config_{};
    EncoderStats  stats_{};
    uint64_t      frame_seq_ = 0;

    // Full init-time NVENC config. Reconfigure reuses it because the driver
    // does not merge sparse configs on re-init (unset fields become zero).
    NV_ENC_CONFIG base_config_{};

    // Measured-stats window (fps/bitrate are computed, not echoed from cfg).
    std::chrono::steady_clock::time_point stats_window_start_{};
    uint32_t stats_window_frames_ = 0;
    uint64_t stats_window_bytes_  = 0;

    Microsoft::WRL::ComPtr<ID3D11Device> device_;
};

} // namespace mello::video
#endif
