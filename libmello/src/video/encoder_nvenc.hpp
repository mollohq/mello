#pragma once
#include "encoder.hpp"

#ifdef _WIN32
#include <wrl/client.h>

#ifdef MELLO_HAS_NVENC
#include <nvEncodeAPI.h>
#endif

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
    HMODULE dll_ = nullptr;

#ifdef MELLO_HAS_NVENC
    NV_ENCODE_API_FUNCTION_LIST fn_{};
    void*                       encoder_   = nullptr;
    NV_ENC_REGISTERED_PTR       reg_res_   = nullptr;
    NV_ENC_OUTPUT_PTR           out_buf_   = nullptr;
    NV_ENC_INPUT_PTR            mapped_input_ = nullptr;
#endif

    bool  force_idr_  = false;
    EncoderConfig config_{};
    EncoderStats  stats_{};
    uint64_t      frame_seq_ = 0;

    Microsoft::WRL::ComPtr<ID3D11Device> device_;
};

} // namespace mello::video
#endif
