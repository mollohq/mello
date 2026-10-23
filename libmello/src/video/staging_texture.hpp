#pragma once
#include "graphics_device.hpp"

#ifdef _WIN32
#include "decoder.hpp"
#include <d3d11.h>
#include <wrl/client.h>
#include <cstdint>
#include <vector>

namespace mello::video {

// The viewer's converter (streaming-quality stage 3, spec 14 section 5.4).
//
// The present thread converts the decoded picture that is due (NV12, or R8
// with the NV12 layout from NVDEC) to RGBA with a compute shader, and copies
// the result into one shared RGBA texture. The shared texture has a keyed
// mutex (key 0 on both sides): libmello writes it only while it holds the
// mutex, and the presenter reads it only while it holds the mutex, so the
// presenter never reads a half-written frame. The shared texture follows the
// picture size: a new size gives a new texture and a new handle.
//
// For a CPU frame callback the RGBA also goes to a staging texture and to
// memory (read_rgba).
//
// Thread: every call runs on the present thread. Source views are cached per
// source texture; a cached view keeps its texture alive, so a recycled
// address cannot reach a stale view.
class StagingTexture {
public:
    bool initialize(const GraphicsDevice& device, bool enable_cpu_readback);
    void shutdown();

    /// Convert `picture` into the shared RGBA texture (and the staging
    /// texture when `copy_to_cpu`). False when the conversion cannot run.
    bool convert(const DecodedPicture& picture, bool copy_to_cpu);

    /// Read the last converted frame into `out` (width() * height() * 4
    /// bytes). Waits for the GPU.
    bool read_rgba(uint8_t* out);

    uint32_t width()  const { return width_; }
    uint32_t height() const { return height_; }
    /// NT handle of the shared RGBA texture (keyed mutex, key 0). Changes
    /// when the size changes.
    void* shared_rgba_handle() const { return shared_rgba_handle_; }
    /// How many times the shared texture was made (a new handle each time).
    uint32_t shared_generation() const { return shared_generation_; }

private:
    bool init_shaders();
    bool ensure_targets(uint32_t width, uint32_t height);
    void release_targets();

    struct SourceViews {
        Microsoft::WRL::ComPtr<ID3D11Texture2D> texture;
        Microsoft::WRL::ComPtr<ID3D11ShaderResourceView> first;   // R8 layout, or NV12 luma
        Microsoft::WRL::ComPtr<ID3D11ShaderResourceView> chroma;  // NV12 chroma (R8G8)
        uint64_t last_use = 0;
    };
    SourceViews* views_for(const DecodedPicture& picture);

    Microsoft::WRL::ComPtr<ID3D11Device>        device_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;
    bool cpu_readback_enabled_ = false;

    Microsoft::WRL::ComPtr<ID3D11ComputeShader> cs_r8_;
    Microsoft::WRL::ComPtr<ID3D11ComputeShader> cs_nv12_;
    Microsoft::WRL::ComPtr<ID3D11Buffer>        cb_;

    uint32_t width_  = 0;
    uint32_t height_ = 0;
    Microsoft::WRL::ComPtr<ID3D11Texture2D>           rgba_tex_;   // compute target
    Microsoft::WRL::ComPtr<ID3D11UnorderedAccessView> rgba_uav_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D>           shared_tex_; // keyed mutex
    Microsoft::WRL::ComPtr<IDXGIKeyedMutex>           shared_mutex_;
    void*                                             shared_rgba_handle_ = nullptr;
    uint32_t                                          shared_generation_ = 0;
    Microsoft::WRL::ComPtr<ID3D11Texture2D>           staging_;

    std::vector<SourceViews> views_;
    uint64_t use_counter_ = 0;
};

} // namespace mello::video
#endif
