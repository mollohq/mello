#pragma once
#include "graphics_device.hpp"

#ifdef _WIN32
#include <d3d11.h>
#include <d3d11_1.h>
#include <wrl/client.h>

using Microsoft::WRL::ComPtr;

namespace mello::video {

class VideoPreprocessor {
public:
    VideoPreprocessor() = default;
    ~VideoPreprocessor();

    /// Initialize with same input and output resolution (no scaling).
    bool initialize(const GraphicsDevice& device, uint32_t width, uint32_t height);

    /// Initialize with separate input (capture) and output (encode) resolution.
    /// The video processor handles high-quality bilinear downscaling in the
    /// same pass as the BGRA→NV12 color conversion.
    bool initialize(const GraphicsDevice& device,
                    uint32_t in_w, uint32_t in_h,
                    uint32_t out_w, uint32_t out_h);

    /// Convert BGRA source texture to NV12 (and downscale if resolutions differ).
    /// Output texture is owned by VideoPreprocessor and reused across calls.
    ID3D11Texture2D* convert(ID3D11Texture2D* bgra_source);

    void shutdown();

private:
    void verify_nv12_output(ID3D11Texture2D* bgra_source);

    ComPtr<ID3D11Device>                      device_;
    ComPtr<ID3D11DeviceContext>               context_;
    ComPtr<ID3D11VideoDevice>                 video_device_;
    ComPtr<ID3D11VideoContext>                video_context_;
    ComPtr<ID3D11VideoProcessorEnumerator>    vp_enum_;
    ComPtr<ID3D11VideoProcessor>              video_processor_;
    ComPtr<ID3D11VideoProcessorOutputView>    output_view_;
    ComPtr<ID3D11Texture2D>                   nv12_texture_;

    uint32_t in_w_  = 0;
    uint32_t in_h_  = 0;
    uint32_t width_  = 0;  // output width
    uint32_t height_ = 0;  // output height
    uint64_t frame_count_ = 0;
};

} // namespace mello::video
#endif
