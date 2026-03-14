#pragma once
#include "encoder.hpp"

#ifdef _WIN32
#include <wrl/client.h>

#ifdef MELLO_HAS_QSV
#include <vpl/mfx.h>
#endif

namespace mello::video {

class QsvEncoder : public Encoder {
public:
    bool        initialize(const GraphicsDevice& device, const EncoderConfig& config) override;
    void        shutdown() override;
    bool        encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) override;
    void        request_keyframe() override;
    void        set_bitrate(uint32_t kbps) override;
    void        get_stats(EncoderStats& out) const override;
    bool        supports_codec(VideoCodec codec) const override;
    const char* name() const override { return "QSV-oneVPL"; }

    static bool is_available();

private:
#ifdef MELLO_HAS_QSV
    mfxLoader  loader_  = nullptr;
    mfxSession session_ = nullptr;
    mfxVideoParam     video_params_{};
    mfxBitstream      bitstream_{};
    std::vector<uint8_t> bs_buf_;
#endif

    bool  force_idr_ = false;
    EncoderConfig config_{};
    EncoderStats  stats_{};
    uint64_t      frame_seq_ = 0;

    Microsoft::WRL::ComPtr<ID3D11Device> device_;
};

} // namespace mello::video
#endif
