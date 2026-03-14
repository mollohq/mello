#ifdef _WIN32
#include "color_converter.hpp"
#include "../util/log.hpp"
#include <d3dcompiler.h>
#include <cassert>

namespace mello::video {

static constexpr const char* TAG = "video/color";

// BT.601 BGRA -> NV12 compute shader.
// Dispatched as ceil(width/16) x ceil(height/16) thread groups.
// Each thread converts one pixel to Y; every 2x2 block also produces one UV pair.
static const char* BGRA_TO_NV12_HLSL = R"(
Texture2D<float4>       g_input  : register(t0);
RWTexture2D<float>      g_out_y  : register(u0);
RWTexture2D<float2>     g_out_uv : register(u1);

cbuffer Constants : register(b0) {
    uint g_width;
    uint g_height;
};

[numthreads(16, 16, 1)]
void main(uint3 dtid : SV_DispatchThreadID) {
    if (dtid.x >= g_width || dtid.y >= g_height) return;

    float4 bgra = g_input.Load(int3(dtid.xy, 0));
    float r = bgra.z;
    float g = bgra.y;
    float b = bgra.x;

    // BT.601 full range
    float y = 0.299f * r + 0.587f * g + 0.114f * b;
    g_out_y[dtid.xy] = y;

    // Subsample chroma at 2x2 block origins
    if ((dtid.x & 1) == 0 && (dtid.y & 1) == 0) {
        float u = -0.169f * r - 0.331f * g + 0.500f * b + 0.5f;
        float v =  0.500f * r - 0.419f * g - 0.081f * b + 0.5f;
        g_out_uv[uint2(dtid.x >> 1, dtid.y >> 1)] = float2(u, v);
    }
}
)";

ColorConverter::~ColorConverter() {
    shutdown();
}

bool ColorConverter::compile_shader() {
    ComPtr<ID3DBlob> blob;
    ComPtr<ID3DBlob> errors;
    HRESULT hr = D3DCompile(
        BGRA_TO_NV12_HLSL,
        strlen(BGRA_TO_NV12_HLSL),
        "bgra_to_nv12.hlsl",
        nullptr, nullptr,
        "main", "cs_5_0",
        D3DCOMPILE_OPTIMIZATION_LEVEL3,
        0,
        &blob, &errors);

    if (FAILED(hr)) {
        const char* msg = errors ? static_cast<const char*>(errors->GetBufferPointer()) : "unknown";
        MELLO_LOG_ERROR(TAG, "Shader compile failed: %s", msg);
        return false;
    }

    hr = device_->CreateComputeShader(blob->GetBufferPointer(), blob->GetBufferSize(), nullptr, &cs_bgra_to_nv12_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateComputeShader failed: hr=0x%08X", hr);
        return false;
    }
    return true;
}

bool ColorConverter::initialize(const GraphicsDevice& device, uint32_t width, uint32_t height) {
    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);
    width_  = width;
    height_ = height;

    if (!compile_shader()) return false;

    // NV12 texture: Y plane is width x height (R8_UNORM), UV plane is (width/2) x (height/2) (R8G8_UNORM)
    // We use two separate textures for UAV output since NV12 doesn't support UAV directly.

    // Y plane texture
    D3D11_TEXTURE2D_DESC y_desc{};
    y_desc.Width  = width;
    y_desc.Height = height;
    y_desc.MipLevels = 1;
    y_desc.ArraySize = 1;
    y_desc.Format = DXGI_FORMAT_R8_UNORM;
    y_desc.SampleDesc.Count = 1;
    y_desc.Usage = D3D11_USAGE_DEFAULT;
    y_desc.BindFlags = D3D11_BIND_UNORDERED_ACCESS | D3D11_BIND_SHADER_RESOURCE;

    ComPtr<ID3D11Texture2D> y_tex;
    HRESULT hr = device_->CreateTexture2D(&y_desc, nullptr, &y_tex);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Failed to create Y texture: hr=0x%08X", hr);
        return false;
    }

    hr = device_->CreateUnorderedAccessView(y_tex.Get(), nullptr, &uav_output_y_);
    if (FAILED(hr)) return false;

    // UV plane texture (half width, half height, RG8)
    D3D11_TEXTURE2D_DESC uv_desc = y_desc;
    uv_desc.Width  = width / 2;
    uv_desc.Height = height / 2;
    uv_desc.Format = DXGI_FORMAT_R8G8_UNORM;

    ComPtr<ID3D11Texture2D> uv_tex;
    hr = device_->CreateTexture2D(&uv_desc, nullptr, &uv_tex);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Failed to create UV texture: hr=0x%08X", hr);
        return false;
    }

    hr = device_->CreateUnorderedAccessView(uv_tex.Get(), nullptr, &uav_output_uv_);
    if (FAILED(hr)) return false;

    // NV12 output texture for the encoder (single NV12 surface).
    // The encoder reads from this.
    D3D11_TEXTURE2D_DESC nv12_desc{};
    nv12_desc.Width  = width;
    nv12_desc.Height = height;
    nv12_desc.MipLevels = 1;
    nv12_desc.ArraySize = 1;
    nv12_desc.Format = DXGI_FORMAT_NV12;
    nv12_desc.SampleDesc.Count = 1;
    nv12_desc.Usage = D3D11_USAGE_DEFAULT;
    nv12_desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;

    hr = device_->CreateTexture2D(&nv12_desc, nullptr, &nv12_texture_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Failed to create NV12 texture: hr=0x%08X", hr);
        return false;
    }

    MELLO_LOG_INFO(TAG, "Color converter initialized: %ux%u BGRA->NV12 (GPU compute)", width, height);
    return true;
}

ID3D11Texture2D* ColorConverter::convert(ID3D11Texture2D* bgra_source) {
    assert(bgra_source);

    // Create SRV for the input texture on-the-fly (source texture may change per frame)
    D3D11_TEXTURE2D_DESC src_desc{};
    bgra_source->GetDesc(&src_desc);

    D3D11_SHADER_RESOURCE_VIEW_DESC srv_desc{};
    srv_desc.Format = src_desc.Format;
    srv_desc.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
    srv_desc.Texture2D.MipLevels = 1;

    ComPtr<ID3D11ShaderResourceView> srv;
    device_->CreateShaderResourceView(bgra_source, &srv_desc, &srv);

    // Set up constant buffer with dimensions
    struct Constants { uint32_t width; uint32_t height; };
    Constants cb{width_, height_};

    D3D11_BUFFER_DESC cb_desc{};
    cb_desc.ByteWidth = sizeof(Constants);
    cb_desc.Usage = D3D11_USAGE_DEFAULT;
    cb_desc.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
    D3D11_SUBRESOURCE_DATA cb_data{};
    cb_data.pSysMem = &cb;

    ComPtr<ID3D11Buffer> cb_buf;
    device_->CreateBuffer(&cb_desc, &cb_data, &cb_buf);

    // Dispatch
    context_->CSSetShader(cs_bgra_to_nv12_.Get(), nullptr, 0);

    ID3D11ShaderResourceView* srvs[] = {srv.Get()};
    context_->CSSetShaderResources(0, 1, srvs);

    ID3D11UnorderedAccessView* uavs[] = {uav_output_y_.Get(), uav_output_uv_.Get()};
    context_->CSSetUnorderedAccessViews(0, 2, uavs, nullptr);

    ID3D11Buffer* cbs[] = {cb_buf.Get()};
    context_->CSSetConstantBuffers(0, 1, cbs);

    context_->Dispatch((width_ + 15) / 16, (height_ + 15) / 16, 1);

    // Unbind
    ID3D11ShaderResourceView* null_srv[] = {nullptr};
    ID3D11UnorderedAccessView* null_uav[] = {nullptr, nullptr};
    context_->CSSetShaderResources(0, 1, null_srv);
    context_->CSSetUnorderedAccessViews(0, 2, null_uav, nullptr);

    // Copy Y and UV planes into the NV12 texture.
    // NV12 layout: Y plane at offset 0 (width*height), UV plane immediately after.
    // We use CopySubresourceRegion to assemble the NV12 surface.
    ComPtr<ID3D11Texture2D> y_tex;
    uav_output_y_->GetResource(reinterpret_cast<ID3D11Resource**>(y_tex.GetAddressOf()));

    D3D11_BOX y_box{0, 0, 0, width_, height_, 1};
    context_->CopySubresourceRegion(nv12_texture_.Get(), 0, 0, 0, 0, y_tex.Get(), 0, &y_box);

    ComPtr<ID3D11Texture2D> uv_tex;
    uav_output_uv_->GetResource(reinterpret_cast<ID3D11Resource**>(uv_tex.GetAddressOf()));

    D3D11_BOX uv_box{0, 0, 0, width_ / 2, height_ / 2, 1};
    context_->CopySubresourceRegion(nv12_texture_.Get(), 0, 0, height_, 0, uv_tex.Get(), 0, &uv_box);

    return nv12_texture_.Get();
}

void ColorConverter::shutdown() {
    cs_bgra_to_nv12_.Reset();
    srv_input_.Reset();
    uav_output_y_.Reset();
    uav_output_uv_.Reset();
    nv12_texture_.Reset();
    context_.Reset();
    device_.Reset();
}

} // namespace mello::video
#endif
