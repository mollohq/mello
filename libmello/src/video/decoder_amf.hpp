#pragma once
#include "decoder.hpp"

#ifdef _WIN32
#include <wrl/client.h>
#include <AMF/core/Factory.h>
#include <AMF/core/Context.h>
#include <AMF/components/VideoDecoderUVD.h>

namespace mello::video {

class AmfDecoder : public Decoder {
public:
    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    bool             decode(const uint8_t* data, size_t size, bool is_keyframe) override;
    ID3D11Texture2D* get_frame() override;
    bool             supports_codec(VideoCodec codec) const override;
    const char*      name() const override { return "AMF-Decode"; }

    static bool is_available();

private:
    HMODULE dll_ = nullptr;

    amf::AMFFactory*     factory_ = nullptr;
    amf::AMFContextPtr   context_;
    amf::AMFComponentPtr decoder_;

    DecoderConfig config_{};
    Microsoft::WRL::ComPtr<ID3D11Device>    device_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> frame_tex_;
};

} // namespace mello::video
#endif
