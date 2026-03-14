#pragma once
#include "graphics_device.hpp"

#ifdef _WIN32
#include <d3d11.h>
#include <wrl/client.h>
#include <cstdint>

namespace mello::video {

class StagingTexture {
public:
    /// @param format  DXGI_FORMAT_NV12 (default) or DXGI_FORMAT_R8_UNORM (NVDEC zero-copy).
    ///                R8 sources get GPU-converted to RGBA via compute shader.
    /// @param uv_y_offset  Row where UV plane starts in R8 layout (coded_height, may differ from video_height).
    bool initialize(const GraphicsDevice& device, uint32_t width, uint32_t video_height,
                    DXGI_FORMAT format = DXGI_FORMAT_NV12, uint32_t uv_y_offset = 0);

    void copy_from(ID3D11Texture2D* source);

    /// Map staging texture and write RGBA into `out`.
    /// `out` must be pre-allocated: width * video_height * 4 bytes.
    void read_rgba(uint8_t* out);

    void shutdown();

    uint32_t width()  const { return width_; }
    uint32_t height() const { return video_height_; }

private:
    bool init_gpu_converter();
    void debug_trace_source(ID3D11Texture2D* source);

    Microsoft::WRL::ComPtr<ID3D11Device>        device_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D>     staging_;

    uint32_t width_        = 0;
    uint32_t video_height_ = 0;
    uint32_t uv_y_offset_  = 0; // UV plane start row in R8 texture (coded_height)
    DXGI_FORMAT format_    = DXGI_FORMAT_NV12;
    uint64_t read_count_   = 0;

    // GPU NV12→RGBA compute shader path (R8 sources only)
    bool gpu_convert_ = false;
    Microsoft::WRL::ComPtr<ID3D11ComputeShader>       cs_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D>           rgba_tex_;
    Microsoft::WRL::ComPtr<ID3D11UnorderedAccessView> rgba_uav_;
    Microsoft::WRL::ComPtr<ID3D11Buffer>              cb_;
    Microsoft::WRL::ComPtr<ID3D11ShaderResourceView>  src_srv_;
    ID3D11Texture2D* src_tex_cached_ = nullptr;
};

} // namespace mello::video
#endif
