#pragma once
#include "encoder.hpp"

#ifdef _WIN32
#include <wrl/client.h>
#include <AMF/core/Factory.h>
#include <AMF/core/Context.h>
#include <AMF/core/Compute.h>
#include <AMF/components/VideoEncoderVCE.h>
#include <AMF/components/VideoEncoderAV1.h>

namespace mello::video {

class AmfEncoder : public Encoder {
public:
    bool        initialize(const GraphicsDevice& device, const EncoderConfig& config) override;
    void        shutdown() override;
    bool        encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) override;
    void        request_keyframe() override;
    void        set_bitrate(uint32_t kbps) override;
    void        get_stats(EncoderStats& out) const override;
    bool        supports_codec(VideoCodec codec) const override;
    const char* name() const override { return "AMF"; }
    bool        reduce_cost_tier() override;
    int         cost_tier() const override { return cost_tier_; }
    int         max_cost_tier() const override { return kMaxCostTier; }
    bool        reconfigure_size(uint32_t width, uint32_t height) override;
    void        set_framerate(uint32_t fps) override;

    static bool is_available();

    /// What one cost tier asks of the AMF H.264 encoder. The NVENC tiers'
    /// equivalents: tier 1 takes the quality preset down to speed, tier 2 also
    /// turns off the per-block adaptive quantization (VBAQ), the high motion
    /// quality boost and pre-analysis. Tier 0 is the init configuration
    /// (ultra-low-latency usage, pre-analysis off). Pure, testable without
    /// AMD hardware.
    struct TierSettings {
        int64_t quality_preset = -1;  ///< AMF_VIDEO_ENCODER_QUALITY_PRESET_*, -1 keeps it
        bool    adaptive_off = false; ///< VBAQ, high motion boost and pre-analysis off
    };
    static TierSettings tier_settings(int tier);
    static constexpr int kMaxCostTier = 2;

private:
    HMODULE dll_ = nullptr;

    amf::AMFFactory*     factory_  = nullptr;
    amf::AMFContextPtr   context_;
    amf::AMFComponentPtr encoder_;

    VideoCodec    codec_     = VideoCodec::H264;
    bool          force_idr_ = false;
    EncoderConfig config_{};
    EncoderStats  stats_{};
    uint64_t      frame_seq_ = 0;
    int           cost_tier_ = 0;
    // A tier change the driver refused: no further attempts this session.
    bool          cost_tier_unavailable_ = false;

    Microsoft::WRL::ComPtr<ID3D11Device> device_;
};

} // namespace mello::video
#endif
