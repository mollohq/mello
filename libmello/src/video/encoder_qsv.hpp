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
    bool        reduce_cost_tier() override;
    int         cost_tier() const override { return cost_tier_; }
    int         max_cost_tier() const override { return kMaxCostTier; }

    static bool is_available();
    /// MELLO_ENABLE_QSV=1 in the process environment. QSV stays out of the
    /// factory without it until it is tested on an Intel GPU (stage 4).
    static bool enabled();
    /// The VBV in KB (vbv_bits_for, rounded up). Pure.
    static mfxU16 vbv_kb_for(uint32_t bitrate_kbps, uint32_t fps);

    /// The oneVPL TargetUsage of a cost tier: the NVENC tiers' equivalent on
    /// Intel. Tier 0 is balanced (4), tier 1 is 6, tier 2 is best speed (7).
    /// Pure, testable without Intel hardware.
    static mfxU16 target_usage_for_tier(int tier);
    static constexpr int kMaxCostTier = 2;

private:
    HMODULE    dll_     = nullptr;
    MfxDispatchFn fn_{};
    mfxLoader  loader_  = nullptr;
    mfxSession session_ = nullptr;
    mfxVideoParam     video_params_{};
    mfxExtCodingOption2 opt2_{};
    mfxExtCodingOption3 opt3_{};
    mfxExtVideoSignalInfo signal_{};
    mfxBitstream      bitstream_{};
    std::vector<uint8_t> bs_buf_;

    bool  force_idr_ = false;
    EncoderConfig config_{};
    EncoderStats  stats_{};
    uint64_t      frame_seq_ = 0;
    int           cost_tier_ = 0;
    bool          cost_tier_unavailable_ = false;

    Microsoft::WRL::ComPtr<ID3D11Device> device_;
};

} // namespace mello::video
#endif
