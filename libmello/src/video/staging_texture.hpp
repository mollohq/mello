#pragma once
#include "graphics_device.hpp"

#ifdef _WIN32
#include <d3d11.h>
#include <wrl/client.h>
#include <cstdint>

namespace mello::video {

class StagingTexture {
public:
    bool initialize(const GraphicsDevice& device, uint32_t width, uint32_t height);

    /// Async GPU copy: decoded NV12 VRAM surface -> staging texture.
    /// Non-blocking — returns immediately.
    void copy_from(ID3D11Texture2D* nv12_source);

    /// Map staging texture and convert NV12 -> RGBA into `out`.
    /// Blocks until the async copy is complete (typically 0-1ms).
    /// `out` must be pre-allocated: width * height * 4 bytes.
    void read_rgba(uint8_t* out);

    void shutdown();

    uint32_t width()  const { return width_; }
    uint32_t height() const { return height_; }

private:
    Microsoft::WRL::ComPtr<ID3D11Device>        device_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D>     staging_;

    uint32_t width_  = 0;
    uint32_t height_ = 0;
};

} // namespace mello::video
#endif
