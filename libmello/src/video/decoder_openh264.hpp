#pragma once
#include "decoder.hpp"

#ifdef _WIN32
#include <d3d11.h>
#include <wrl/client.h>
#include <vector>

struct ISVCDecoder;

namespace mello::video {

class OpenH264Decoder : public Decoder {
public:
    static bool is_available();

    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    bool             decode(const uint8_t* data, size_t size, bool is_keyframe) override;
    ID3D11Texture2D* get_frame() override;
    bool             supports_codec(VideoCodec codec) const override { return codec == VideoCodec::H264; }
    const char*      name() const override { return "OpenH264"; }

private:
    ISVCDecoder* decoder_ = nullptr;
    DecoderConfig config_{};

    Microsoft::WRL::ComPtr<ID3D11Texture2D>    upload_tex_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;
    Microsoft::WRL::ComPtr<ID3D11Device>        device_;

    std::vector<uint8_t> nv12_buf_;
};

} // namespace mello::video
#endif
