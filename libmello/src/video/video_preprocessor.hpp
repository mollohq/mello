#pragma once
#include "graphics_device.hpp"

#ifdef _WIN32
#include <d3d11.h>
#include <d3d11_1.h>
#include <wrl/client.h>
#include <array>

using Microsoft::WRL::ComPtr;

namespace mello::video {

struct ConvertResult {
    ID3D11Texture2D* texture;
    size_t           slot_index;
};

class VideoPreprocessor {
public:
    static constexpr size_t NV12_RING_SLOTS = 3;

    VideoPreprocessor() = default;
    ~VideoPreprocessor();

    bool initialize(const GraphicsDevice& device, uint32_t width, uint32_t height);
    bool initialize(const GraphicsDevice& device,
                    uint32_t in_w, uint32_t in_h,
                    uint32_t out_w, uint32_t out_h);

    /// Convert BGRA source to the next NV12 ring slot. Returns texture + slot index.
    ConvertResult convert(ID3D11Texture2D* bgra_source);

    void shutdown();

private:
    void verify_nv12_output(ID3D11Texture2D* bgra_source, size_t slot);
    bool create_nv12_slot(size_t idx, uint32_t w, uint32_t h);

    ComPtr<ID3D11Device>                      device_;
    ComPtr<ID3D11DeviceContext>               context_;
    ComPtr<ID3D11VideoDevice>                 video_device_;
    ComPtr<ID3D11VideoContext>                video_context_;
    ComPtr<ID3D11VideoProcessorEnumerator>    vp_enum_;
    ComPtr<ID3D11VideoProcessor>              video_processor_;

    struct Nv12Slot {
        ComPtr<ID3D11Texture2D>                   texture;
        ComPtr<ID3D11VideoProcessorOutputView>    output_view;
    };
    std::array<Nv12Slot, NV12_RING_SLOTS> nv12_slots_{};
    size_t nv12_write_idx_ = 0;

    uint32_t in_w_  = 0;
    uint32_t in_h_  = 0;
    uint32_t width_  = 0;
    uint32_t height_ = 0;
    uint64_t frame_count_ = 0;
};

} // namespace mello::video
#endif
