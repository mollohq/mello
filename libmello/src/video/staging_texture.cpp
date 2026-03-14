#ifdef _WIN32
#include "staging_texture.hpp"
#include "../util/log.hpp"
#include <algorithm>
#include <chrono>
#include <cstring>

namespace mello::video {

static constexpr const char* TAG = "video/staging";

// HLSL compute shader: reads NV12-layout R8 texture, writes RGBA
static const char* CS_SOURCE = R"(
Texture2D<float> nv12 : register(t0);
RWTexture2D<float4> rgba : register(u0);

cbuffer CB : register(b0) {
    uint vid_w;
    uint vid_h;
    uint uv_y;  // UV plane row offset (coded_height, may differ from vid_h due to macroblock alignment)
};

[numthreads(16, 16, 1)]
void CSMain(uint3 id : SV_DispatchThreadID) {
    if (id.x >= vid_w || id.y >= vid_h) return;

    float y_raw = nv12[uint2(id.x, id.y)].r * 255.0;

    uint uv_row = uv_y + id.y / 2;
    uint uv_col = id.x & ~1u;
    float u_raw = nv12[uint2(uv_col,     uv_row)].r * 255.0;
    float v_raw = nv12[uint2(uv_col + 1, uv_row)].r * 255.0;

    float c = y_raw - 16.0;
    float d = u_raw - 128.0;
    float e = v_raw - 128.0;

    // BT.709 studio-swing
    float r = (298.0 * c + 459.0 * e + 128.0) / 256.0 / 255.0;
    float g = (298.0 * c -  55.0 * d - 136.0 * e + 128.0) / 256.0 / 255.0;
    float b = (298.0 * c + 541.0 * d + 128.0) / 256.0 / 255.0;

    rgba[id.xy] = float4(saturate(r), saturate(g), saturate(b), 1.0);
}
)";

// D3DCompile signature (loaded dynamically to avoid link dependency)
typedef HRESULT(WINAPI* D3DCompileFunc)(
    const void*, SIZE_T, const char*, const void*, void*,
    const char*, const char*, UINT, UINT,
    ID3DBlob**, ID3DBlob**);

bool StagingTexture::init_gpu_converter() {
    HMODULE dll = LoadLibraryA("d3dcompiler_47.dll");
    if (!dll) {
        MELLO_LOG_WARN(TAG, "d3dcompiler_47.dll not found, using CPU fallback");
        return false;
    }

    auto compile_fn = reinterpret_cast<D3DCompileFunc>(GetProcAddress(dll, "D3DCompile"));
    if (!compile_fn) {
        FreeLibrary(dll);
        return false;
    }

    ID3DBlob* blob_raw = nullptr;
    ID3DBlob* errors_raw = nullptr;
    HRESULT hr = compile_fn(
        CS_SOURCE, strlen(CS_SOURCE), "nv12_to_rgba.hlsl", nullptr, nullptr,
        "CSMain", "cs_5_0", 0, 0, &blob_raw, &errors_raw);

    if (FAILED(hr)) {
        if (errors_raw) {
            MELLO_LOG_ERROR(TAG, "Shader compile error: %s", (char*)errors_raw->GetBufferPointer());
            errors_raw->Release();
        }
        if (blob_raw) blob_raw->Release();
        FreeLibrary(dll);
        return false;
    }

    hr = device_->CreateComputeShader(blob_raw->GetBufferPointer(), blob_raw->GetBufferSize(), nullptr, &cs_);

    // Release blobs before unloading the compiler DLL (vtable lives in that DLL)
    blob_raw->Release();
    if (errors_raw) errors_raw->Release();
    FreeLibrary(dll);

    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateComputeShader failed: 0x%08X", hr);
        return false;
    }

    // RGBA output texture (GPU, UAV-bindable)
    D3D11_TEXTURE2D_DESC rgba_desc{};
    rgba_desc.Width  = width_;
    rgba_desc.Height = video_height_;
    rgba_desc.MipLevels = 1;
    rgba_desc.ArraySize = 1;
    rgba_desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    rgba_desc.SampleDesc.Count = 1;
    rgba_desc.Usage = D3D11_USAGE_DEFAULT;
    rgba_desc.BindFlags = D3D11_BIND_UNORDERED_ACCESS;

    hr = device_->CreateTexture2D(&rgba_desc, nullptr, &rgba_tex_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateTexture2D (RGBA) failed: 0x%08X", hr);
        return false;
    }

    hr = device_->CreateUnorderedAccessView(rgba_tex_.Get(), nullptr, &rgba_uav_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateUAV failed: 0x%08X", hr);
        return false;
    }

    // Constant buffer (video_width, video_height, uv_y_offset)
    struct { uint32_t w, h, uv_y, pad; } cb_data = { width_, video_height_, uv_y_offset_, 0 };
    D3D11_BUFFER_DESC cb_desc{};
    cb_desc.ByteWidth = sizeof(cb_data);
    cb_desc.Usage = D3D11_USAGE_IMMUTABLE;
    cb_desc.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
    D3D11_SUBRESOURCE_DATA cb_init{ &cb_data, 0, 0 };

    hr = device_->CreateBuffer(&cb_desc, &cb_init, &cb_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateBuffer (CB) failed: 0x%08X", hr);
        return false;
    }

    return true;
}

bool StagingTexture::initialize(const GraphicsDevice& device, uint32_t width, uint32_t video_height,
                                DXGI_FORMAT format, uint32_t uv_y_offset) {
    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);
    width_        = width;
    video_height_ = video_height;
    uv_y_offset_  = uv_y_offset ? uv_y_offset : video_height;
    format_       = format;

    // For R8 sources, try GPU compute shader path
    if (format == DXGI_FORMAT_R8_UNORM) {
        gpu_convert_ = init_gpu_converter();
    }

    DXGI_FORMAT staging_fmt;
    uint32_t    staging_h;

    if (gpu_convert_) {
        // Staging is RGBA — compute shader produces RGBA on GPU
        staging_fmt = DXGI_FORMAT_R8G8B8A8_UNORM;
        staging_h   = video_height;
    } else if (format == DXGI_FORMAT_R8_UNORM) {
        staging_fmt = DXGI_FORMAT_R8_UNORM;
        staging_h   = video_height + video_height / 2;
    } else {
        staging_fmt = DXGI_FORMAT_NV12;
        staging_h   = video_height;
    }

    D3D11_TEXTURE2D_DESC desc{};
    desc.Width  = width;
    desc.Height = staging_h;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = staging_fmt;
    desc.SampleDesc.Count = 1;
    desc.Usage = D3D11_USAGE_STAGING;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;

    HRESULT hr = device_->CreateTexture2D(&desc, nullptr, &staging_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Failed to create staging texture: hr=0x%08X", hr);
        return false;
    }

    const char* path_str = gpu_convert_ ? "GPU compute" : "CPU";
    const char* fmt_str  = (staging_fmt == DXGI_FORMAT_R8G8B8A8_UNORM) ? "RGBA"
                         : (staging_fmt == DXGI_FORMAT_R8_UNORM) ? "R8" : "NV12";
    MELLO_LOG_INFO(TAG, "Staging texture initialized: %ux%u %s (video %ux%u, uv_offset=%u, convert=%s)",
        width, staging_h, fmt_str, width, video_height, uv_y_offset_, path_str);
    return true;
}

void StagingTexture::debug_trace_source(ID3D11Texture2D* source) {
    // Read back raw R8 source to verify NV12 values and trace the conversion
    D3D11_TEXTURE2D_DESC src_desc{};
    source->GetDesc(&src_desc);

    D3D11_TEXTURE2D_DESC stg_desc = src_desc;
    stg_desc.Usage = D3D11_USAGE_STAGING;
    stg_desc.BindFlags = 0;
    stg_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;

    Microsoft::WRL::ComPtr<ID3D11Texture2D> dbg_staging;
    if (FAILED(device_->CreateTexture2D(&stg_desc, nullptr, &dbg_staging))) return;

    context_->CopyResource(dbg_staging.Get(), source);

    D3D11_MAPPED_SUBRESOURCE m{};
    if (FAILED(context_->Map(dbg_staging.Get(), 0, D3D11_MAP_READ, 0, &m))) return;

    const uint8_t* data = static_cast<const uint8_t*>(m.pData);
    uint32_t cx = width_ / 2;
    uint32_t cy = video_height_ / 2;

    // Sample a few pixels from Y and UV planes
    uint8_t y_tl = data[0];
    uint8_t y_c  = data[cy * m.RowPitch + cx];
    uint8_t y_br = data[(video_height_ - 1) * m.RowPitch + (width_ - 1)];

    const uint8_t* uv_base = data + m.RowPitch * uv_y_offset_;
    uint8_t u_c = uv_base[(cy / 2) * m.RowPitch + (cx & ~1u)];
    uint8_t v_c = uv_base[(cy / 2) * m.RowPitch + (cx & ~1u) + 1];

    // Manual BT.709 conversion
    int c = y_c - 16;
    int d = u_c - 128;
    int e = v_c - 128;
    int exp_r = std::clamp((298 * c + 459 * e + 128) >> 8, 0, 255);
    int exp_g = std::clamp((298 * c -  55 * d - 136 * e + 128) >> 8, 0, 255);
    int exp_b = std::clamp((298 * c + 541 * d + 128) >> 8, 0, 255);

    MELLO_LOG_DEBUG(TAG, "TRACE src R8: pitch=%u Y[0,0]=%u Y[center]=%u Y[br]=%u UV[center]=(%u,%u) "
        "-> expected RGBA=(%d,%d,%d,255)",
        m.RowPitch, y_tl, y_c, y_br, u_c, v_c, exp_r, exp_g, exp_b);

    context_->Unmap(dbg_staging.Get(), 0);
}

void StagingTexture::copy_from(ID3D11Texture2D* source) {
    if (gpu_convert_) {
        if (read_count_ < 3) {
            debug_trace_source(source);
        }

        // Create/cache SRV for the source R8 texture
        if (source != src_tex_cached_) {
            src_srv_.Reset();
            D3D11_SHADER_RESOURCE_VIEW_DESC srv_desc{};
            srv_desc.Format = DXGI_FORMAT_R8_UNORM;
            srv_desc.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
            srv_desc.Texture2D.MipLevels = 1;
            device_->CreateShaderResourceView(source, &srv_desc, &src_srv_);
            src_tex_cached_ = source;
        }

        // Dispatch compute shader: R8 (NV12 layout) → RGBA
        ID3D11ShaderResourceView* srvs[] = { src_srv_.Get() };
        ID3D11UnorderedAccessView* uavs[] = { rgba_uav_.Get() };
        ID3D11Buffer* cbs[] = { cb_.Get() };

        context_->CSSetShader(cs_.Get(), nullptr, 0);
        context_->CSSetShaderResources(0, 1, srvs);
        context_->CSSetUnorderedAccessViews(0, 1, uavs, nullptr);
        context_->CSSetConstantBuffers(0, 1, cbs);

        uint32_t gx = (width_ + 15) / 16;
        uint32_t gy = (video_height_ + 15) / 16;
        context_->Dispatch(gx, gy, 1);

        // Unbind
        ID3D11ShaderResourceView* null_srv[] = { nullptr };
        ID3D11UnorderedAccessView* null_uav[] = { nullptr };
        context_->CSSetShaderResources(0, 1, null_srv);
        context_->CSSetUnorderedAccessViews(0, 1, null_uav, nullptr);

        // Copy RGBA result to staging
        context_->CopyResource(staging_.Get(), rgba_tex_.Get());
    } else {
        context_->CopyResource(staging_.Get(), source);
    }
}

void StagingTexture::read_rgba(uint8_t* out) {
    auto t0 = std::chrono::steady_clock::now();

    D3D11_MAPPED_SUBRESOURCE mapped{};
    HRESULT hr = context_->Map(staging_.Get(), 0, D3D11_MAP_READ, 0, &mapped);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Map failed: hr=0x%08X", hr);
        return;
    }

    auto t1 = std::chrono::steady_clock::now();
    auto stall_ms = std::chrono::duration<float, std::milli>(t1 - t0).count();
    if (stall_ms > 2.0f) {
        MELLO_LOG_WARN(TAG, "Map() stall %.1fms -- possible GPU pipeline pressure", stall_ms);
    }

    if (gpu_convert_) {
        // Staging is RGBA — just memcpy rows (account for pitch != width*4)
        const uint8_t* src = static_cast<const uint8_t*>(mapped.pData);
        uint32_t row_bytes = width_ * 4;

        if (read_count_ < 3) {
            uint32_t cx = width_ / 2;
            uint32_t cy = video_height_ / 2;
            const uint8_t* center = src + cy * mapped.RowPitch + cx * 4;
            MELLO_LOG_DEBUG(TAG, "read_rgba[%llu]: pitch=%u RGBA[center]=(%u,%u,%u,%u)",
                read_count_, mapped.RowPitch, center[0], center[1], center[2], center[3]);
        }

        if (mapped.RowPitch == row_bytes) {
            memcpy(out, src, row_bytes * video_height_);
        } else {
            for (uint32_t row = 0; row < video_height_; ++row) {
                memcpy(out + row * row_bytes, src + row * mapped.RowPitch, row_bytes);
            }
        }
    } else {
        // CPU NV12→RGBA fallback
        const uint8_t* y_plane  = static_cast<const uint8_t*>(mapped.pData);
        const uint8_t* uv_plane = y_plane + mapped.RowPitch * uv_y_offset_;

        if (read_count_ < 3) {
            uint32_t cx = width_ / 2;
            uint32_t cy = video_height_ / 2;
            uint8_t y_tl = y_plane[0];
            uint8_t y_c  = y_plane[cy * mapped.RowPitch + cx];
            uint8_t u_c  = uv_plane[(cy / 2) * mapped.RowPitch + (cx & ~1u)];
            uint8_t v_c  = uv_plane[(cy / 2) * mapped.RowPitch + (cx & ~1u) + 1];
            MELLO_LOG_DEBUG(TAG, "read_rgba[%llu]: pitch=%u Y[0,0]=%u Y[center]=%u UV[center]=(%u,%u)",
                read_count_, mapped.RowPitch, y_tl, y_c, u_c, v_c);
        }

        for (uint32_t row = 0; row < video_height_; ++row) {
            const uint8_t* y_row  = y_plane + row * mapped.RowPitch;
            const uint8_t* uv_row = uv_plane + (row / 2) * mapped.RowPitch;
            uint8_t* dst = out + row * width_ * 4;

            for (uint32_t col = 0; col < width_; ++col) {
                uint8_t y = y_row[col];
                uint8_t u = uv_row[(col & ~1u)];
                uint8_t v = uv_row[(col & ~1u) + 1];

                // BT.709 YUV -> RGB (studio-swing Y 16-235, UV 16-240)
                int c = y - 16;
                int d = u - 128;
                int e = v - 128;

                int r = (298 * c + 459 * e + 128) >> 8;
                int g = (298 * c -  55 * d - 136 * e + 128) >> 8;
                int b = (298 * c + 541 * d + 128) >> 8;

                dst[col * 4 + 0] = static_cast<uint8_t>(std::clamp(r, 0, 255));
                dst[col * 4 + 1] = static_cast<uint8_t>(std::clamp(g, 0, 255));
                dst[col * 4 + 2] = static_cast<uint8_t>(std::clamp(b, 0, 255));
                dst[col * 4 + 3] = 255;
            }
        }
    }

    read_count_++;
    context_->Unmap(staging_.Get(), 0);
}

void StagingTexture::shutdown() {
    src_srv_.Reset();
    cb_.Reset();
    rgba_uav_.Reset();
    rgba_tex_.Reset();
    cs_.Reset();
    staging_.Reset();
    context_.Reset();
    device_.Reset();
}

} // namespace mello::video
#endif
