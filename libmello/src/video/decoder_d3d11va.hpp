#pragma once
#include "decoder.hpp"

#ifdef _WIN32
#include <d3d11.h>
#include <d3d11_1.h>
#include <wrl/client.h>
#include <vector>

namespace mello::video {

class D3d11vaDecoder : public Decoder {
public:
    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    bool             decode(const uint8_t* data, size_t size, bool is_keyframe) override;
    ID3D11Texture2D* get_frame() override;
    bool             supports_codec(VideoCodec codec) const override;
    const char*      name() const override { return "D3D11VA"; }

    static bool is_available(ID3D11Device* device);

private:
    bool submit_decode(const uint8_t* data, size_t size);

    Microsoft::WRL::ComPtr<ID3D11VideoDevice>    video_device_;
    Microsoft::WRL::ComPtr<ID3D11VideoContext>    video_context_;
    Microsoft::WRL::ComPtr<ID3D11VideoDecoder>    decoder_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D>       decode_tex_;
    Microsoft::WRL::ComPtr<ID3D11VideoDecoderOutputView> output_view_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D>       frame_tex_;
    Microsoft::WRL::ComPtr<ID3D11Device>          device_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext>   context_;

    DecoderConfig config_{};
    std::vector<uint8_t> slice_buf_;
};

} // namespace mello::video
#endif
