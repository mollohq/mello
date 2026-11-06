#pragma once
#include "decoder.hpp"
#ifdef _WIN32
#include <wrl/client.h>
#include <cstdint>
#include <vector>

struct IMFTransform;
struct IMFDXGIDeviceManager;
struct IMFSample;
struct IMFMediaType;

namespace mello::video {

/// The Microsoft H.264 decoder (Media Foundation transform
/// CLSID_CMSH264DecoderMFT in msmpeg2vdec.dll). It is part of Windows 10 and
/// 11 except the N and KN editions without the Media Feature Pack. It comes
/// after NVDEC and AMF, so an Intel viewer decodes with it (12-STREAMING.md
/// section 7.4).
///
/// Modes:
/// - GPU (DXVA): the device has video support and the transform takes our
///   DXGI device manager. The transform's output texture (a texture array
///   slice) is copied into the slot texture on the GPU.
/// - CPU: no video support, the device manager is refused, or
///   MELLO_MF_DECODER_CPU=1. The NV12 picture in system memory goes into
///   the slot texture with UpdateSubresource.
///
/// COM: every COM object of this decoder lives in the MTA. initialize keeps
/// the MTA alive (CoIncrementMTAUsage), so the decode thread, which has no
/// COM apartment of its own, runs in the implicit MTA. initialize and
/// shutdown do their COM work on a short helper thread when they are called
/// on an STA thread.
///
/// Threads: initialize on the caller of the viewer's start (or the decode
/// thread for a mid-stream switch), decode and picture on the decode thread,
/// shutdown after the decode thread ended.
class MfDecoder : public Decoder {
public:
    MfDecoder();
    ~MfDecoder() override;

    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    DecodeFeedResult decode(const uint8_t* data, size_t size, bool is_keyframe, int slot) override;
    bool             picture(int slot, DecodedPicture& out) const override;
    bool             supports_codec(VideoCodec codec) const override { return codec == VideoCodec::H264; }
    const char*      name() const override { return "MediaFoundation"; }

    /// True when Media Foundation starts and the transform can be made. The
    /// first call probes; later calls give the same result. An N edition
    /// without the Media Feature Pack gives false (logged once).
    static bool is_available();

    /// True after initialize when the transform decodes on the GPU.
    bool gpu_mode() const { return gpu_mode_; }

private:
    bool setup();
    void release_com();
    bool set_output_type();
    bool make_cpu_output_sample();
    bool drain_output(int slot, bool& frame_ready);
    bool copy_to_slot(IMFSample* sample, int slot);
    bool ensure_slot(int slot, uint32_t w, uint32_t h);

    Microsoft::WRL::ComPtr<ID3D11Device>        device_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;
    Microsoft::WRL::ComPtr<IMFTransform>         mft_;
    Microsoft::WRL::ComPtr<IMFDXGIDeviceManager> manager_;
    Microsoft::WRL::ComPtr<IMFSample>            cpu_out_sample_;
    DecoderConfig config_{};

    bool     mf_started_ = false;
    void*    mta_cookie_ = nullptr;  // CO_MTA_USAGE_COOKIE
    bool     gpu_mode_ = false;
    bool     mft_provides_samples_ = false;
    uint32_t cpu_out_size_ = 0;
    int64_t  sample_time_ = 0;

    // The output picture: the coded size (MF_MT_FRAME_SIZE, 1088 rows for a
    // 1080p stream), the visible area in it (MF_MT_MINIMUM_DISPLAY_APERTURE)
    // and the row pitch of a CPU picture.
    uint32_t coded_w_ = 0;
    uint32_t coded_h_ = 0;
    uint32_t visible_x_ = 0;
    uint32_t visible_y_ = 0;
    uint32_t visible_w_ = 0;
    uint32_t visible_h_ = 0;
    int32_t  cpu_stride_ = 0;

    // One NV12 texture per output slot. A new picture size makes a new
    // texture for the slot; the ring keeps the old one while the present
    // thread holds it.
    struct Slot {
        Microsoft::WRL::ComPtr<ID3D11Texture2D> texture;
        uint32_t width = 0;
        uint32_t height = 0;
    };
    Slot slots_[kOutputSlots];
    std::vector<uint8_t> nv12_buf_;
};

} // namespace mello::video
#endif
