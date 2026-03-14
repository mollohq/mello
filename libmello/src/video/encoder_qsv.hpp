#pragma once
#include "encoder.hpp"

#ifdef _WIN32
#include <wrl/client.h>
#include <vpl/mfx.h>

namespace mello::video {

struct MfxDispatchFn {
    decltype(&MFXLoad)                          Load;
    decltype(&MFXUnload)                        Unload;
    decltype(&MFXCreateConfig)                  CreateConfig;
    decltype(&MFXSetConfigFilterProperty)       SetConfigFilterProperty;
    decltype(&MFXCreateSession)                 CreateSession;
    decltype(&MFXClose)                         Close;
    decltype(&MFXVideoCORE_SetHandle)           CoreSetHandle;
    decltype(&MFXVideoCORE_SyncOperation)       CoreSyncOp;
    decltype(&MFXVideoENCODE_Init)              EncInit;
    decltype(&MFXVideoENCODE_Reset)             EncReset;
    decltype(&MFXVideoENCODE_Close)             EncClose;
    decltype(&MFXVideoENCODE_GetVideoParam)     EncGetVideoParam;
    decltype(&MFXVideoENCODE_EncodeFrameAsync)  EncFrameAsync;
};

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
    HMODULE    dll_     = nullptr;
    MfxDispatchFn fn_{};
    mfxLoader  loader_  = nullptr;
    mfxSession session_ = nullptr;
    mfxVideoParam     video_params_{};
    mfxBitstream      bitstream_{};
    std::vector<uint8_t> bs_buf_;

    bool  force_idr_ = false;
    EncoderConfig config_{};
    EncoderStats  stats_{};
    uint64_t      frame_seq_ = 0;

    Microsoft::WRL::ComPtr<ID3D11Device> device_;
};

} // namespace mello::video
#endif
