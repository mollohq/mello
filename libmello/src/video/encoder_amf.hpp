#pragma once
#include "encoder.hpp"

#ifdef _WIN32
#include <wrl/client.h>

#ifdef MELLO_HAS_AMF
#include <AMF/core/Factory.h>
#include <AMF/core/Context.h>
#include <AMF/core/Compute.h>
#include <AMF/components/VideoEncoderVCE.h>
#include <AMF/components/VideoEncoderAV1.h>
#endif

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

    static bool is_available();

private:
    HMODULE dll_ = nullptr;

#ifdef MELLO_HAS_AMF
    amf::AMFFactory*     factory_  = nullptr;
    amf::AMFContextPtr   context_;
    amf::AMFComponentPtr encoder_;
#endif

    VideoCodec    codec_     = VideoCodec::H264;
    bool          force_idr_ = false;
    EncoderConfig config_{};
    EncoderStats  stats_{};
    uint64_t      frame_seq_ = 0;

    Microsoft::WRL::ComPtr<ID3D11Device> device_;
};

} // namespace mello::video
#endif
