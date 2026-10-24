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
    DecodeFeedResult decode(const uint8_t* data, size_t size, bool is_keyframe, int slot) override;
    bool             picture(int slot, DecodedPicture& out) const override;
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
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> d3d_context_;

    // One NV12 texture per output slot: the decoder's surface is copied in,
    // so the AMF surface goes back to its pool at once. A new picture size
    // makes a new texture for the slot.
    struct Slot {
        Microsoft::WRL::ComPtr<ID3D11Texture2D> texture;
        uint32_t width = 0;
        uint32_t height = 0;
    };
    Slot slots_[kOutputSlots];

    bool copy_to_slot(amf::AMFSurface* surface, int slot);
};

} // namespace mello::video
#endif
