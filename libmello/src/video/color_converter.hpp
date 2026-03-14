#pragma once
#include "graphics_device.hpp"

#ifdef _WIN32
#include <d3d11.h>
#include <wrl/client.h>

using Microsoft::WRL::ComPtr;

namespace mello::video {

class ColorConverter {
public:
    ColorConverter() = default;
    ~ColorConverter();

    bool initialize(const GraphicsDevice& device, uint32_t width, uint32_t height);

    /// Convert BGRA source texture to NV12.
    /// Output texture is owned by ColorConverter and reused across calls.
    ID3D11Texture2D* convert(ID3D11Texture2D* bgra_source);

    void shutdown();

private:
    bool compile_shader();

    ComPtr<ID3D11Device>              device_;
    ComPtr<ID3D11DeviceContext>       context_;
    ComPtr<ID3D11ComputeShader>       cs_bgra_to_nv12_;
    ComPtr<ID3D11ShaderResourceView>  srv_input_;
    ComPtr<ID3D11UnorderedAccessView> uav_output_y_;
    ComPtr<ID3D11UnorderedAccessView> uav_output_uv_;
    ComPtr<ID3D11Texture2D>           nv12_texture_;

    uint32_t width_  = 0;
    uint32_t height_ = 0;
};

} // namespace mello::video
#endif
