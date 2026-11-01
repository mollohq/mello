#pragma once
#include "decoder.hpp"

#ifdef _WIN32
#include <d3d11.h>
#include <wrl/client.h>
#include <vector>

namespace mello::video {

namespace wels_abi {
class ISVCDecoder;
}

class OpenH264Decoder : public Decoder {
public:
    static bool is_available();

    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    DecodeFeedResult decode(const uint8_t* data, size_t size, bool is_keyframe, int slot) override;
    bool             picture(int slot, DecodedPicture& out) const override;
    bool             supports_codec(VideoCodec codec) const override { return codec == VideoCodec::H264; }
    const char*      name() const override { return "OpenH264"; }

private:
    wels_abi::ISVCDecoder* decoder_ = nullptr;
    DecoderConfig config_{};

    // One NV12 texture per output slot, at the size of the last picture
    // written into it. A new size makes a new texture; the old one lives on
    // while the present thread still holds it.
    struct Slot {
        Microsoft::WRL::ComPtr<ID3D11Texture2D> texture;
        uint32_t width = 0;
        uint32_t height = 0;
    };
    Slot slots_[kOutputSlots];

    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;
    Microsoft::WRL::ComPtr<ID3D11Device>        device_;

    std::vector<uint8_t> nv12_buf_;
};

} // namespace mello::video
#endif

#if defined(__linux__)
#include <vector>

namespace mello::video {

namespace wels_abi {
class ISVCDecoder;
}

/// OpenH264 on Linux: the picture stays in CPU memory as I420, one copy per
/// output slot (decoder_openh264_linux.cpp).
class OpenH264Decoder : public Decoder {
public:
    static bool is_available();

    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    DecodeFeedResult decode(const uint8_t* data, size_t size, bool is_keyframe, int slot) override;
    bool             picture(int slot, CpuPicture& out) const override;
    bool             supports_codec(VideoCodec codec) const override { return codec == VideoCodec::H264; }
    const char*      name() const override { return "OpenH264"; }

private:
    wels_abi::ISVCDecoder* decoder_ = nullptr;
    DecoderConfig config_{};

    // One contiguous I420 copy per output slot: luma, then U, then V, with
    // the width as the luma stride.
    struct Slot {
        std::vector<uint8_t> i420;
        uint32_t width = 0;
        uint32_t height = 0;
    };
    Slot slots_[kOutputSlots];
};

} // namespace mello::video
#endif
