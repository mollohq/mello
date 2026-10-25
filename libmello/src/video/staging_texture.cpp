#ifdef _WIN32
#include "staging_texture.hpp"
#include "../util/log.hpp"
#include <algorithm>
#include <chrono>
#include <cstring>
#include <dxgi.h>
#include <dxgi1_2.h>

namespace mello::video {

static constexpr const char* TAG = "video/staging";

// How long the converter waits for the presenter to give the shared
// texture back. The presenter holds it only for one CopyResource.
static constexpr DWORD kKeyedMutexTimeoutMs = 100;
// Source textures with cached views. The ring has 3 slots; a size change
// adds 3 more for a while.
static constexpr size_t kMaxCachedSources = 8;

// HLSL compute shader, two entry points with one conversion:
// - CSMain reads an NV12-layout R8 texture (NVDEC): Y rows, then UV rows from
//   row uv_y.
// - CSMainNv12 reads a DXGI_FORMAT_NV12 texture (AMF, OpenH264) through two
//   views: R8 on the luma plane, R8G8 on the chroma plane.
static const char* CS_SOURCE = R"(
Texture2D<float>  nv12   : register(t0);
Texture2D<float>  luma   : register(t1);
Texture2D<float2> chroma : register(t2);
RWTexture2D<float4> rgba : register(u0);

cbuffer CB : register(b0) {
    uint vid_w;
    uint vid_h;
    uint uv_y;  // UV plane row offset of the R8 layout
};

float4 yuv_to_rgba(float y_raw, float u_raw, float v_raw) {
    float c = y_raw - 16.0;
    float d = u_raw - 128.0;
    float e = v_raw - 128.0;

    // BT.709 studio-swing
    float r = (298.0 * c + 459.0 * e + 128.0) / 256.0 / 255.0;
    float g = (298.0 * c -  55.0 * d - 136.0 * e + 128.0) / 256.0 / 255.0;
    float b = (298.0 * c + 541.0 * d + 128.0) / 256.0 / 255.0;
    return float4(saturate(r), saturate(g), saturate(b), 1.0);
}

[numthreads(16, 16, 1)]
void CSMain(uint3 id : SV_DispatchThreadID) {
    if (id.x >= vid_w || id.y >= vid_h) return;

    float y_raw = nv12[uint2(id.x, id.y)].r * 255.0;

    uint uv_row = uv_y + id.y / 2;
    uint uv_col = id.x & ~1u;
    float u_raw = nv12[uint2(uv_col,     uv_row)].r * 255.0;
    float v_raw = nv12[uint2(uv_col + 1, uv_row)].r * 255.0;

    rgba[id.xy] = yuv_to_rgba(y_raw, u_raw, v_raw);
}

[numthreads(16, 16, 1)]
void CSMainNv12(uint3 id : SV_DispatchThreadID) {
    if (id.x >= vid_w || id.y >= vid_h) return;

    float  y_raw = luma[id.xy] * 255.0;
    float2 uv    = chroma[id.xy / 2] * 255.0;

    rgba[id.xy] = yuv_to_rgba(y_raw, uv.x, uv.y);
}
)";

// D3DCompile signature (loaded dynamically to avoid link dependency)
typedef HRESULT(WINAPI* D3DCompileFunc)(
    const void*, SIZE_T, const char*, const void*, void*,
    const char*, const char*, UINT, UINT,
    ID3DBlob**, ID3DBlob**);

struct ConvertConstants {
    uint32_t w, h, uv_y, pad;
};

bool StagingTexture::init_shaders() {
    HMODULE dll = LoadLibraryA("d3dcompiler_47.dll");
    if (!dll) {
        MELLO_LOG_ERROR(TAG, "d3dcompiler_47.dll not found: the viewer cannot convert frames");
        return false;
    }
    auto compile_fn = reinterpret_cast<D3DCompileFunc>(GetProcAddress(dll, "D3DCompile"));
    if (!compile_fn) {
        FreeLibrary(dll);
        return false;
    }

    auto build = [&](const char* entry, Microsoft::WRL::ComPtr<ID3D11ComputeShader>& out) {
        ID3DBlob* blob = nullptr;
        ID3DBlob* errors = nullptr;
        HRESULT hr = compile_fn(CS_SOURCE, strlen(CS_SOURCE), "nv12_to_rgba.hlsl", nullptr, nullptr,
                                entry, "cs_5_0", 0, 0, &blob, &errors);
        if (FAILED(hr)) {
            if (errors) {
                MELLO_LOG_ERROR(TAG, "Shader compile error: %s", (char*)errors->GetBufferPointer());
                errors->Release();
            }
            if (blob) blob->Release();
            return false;
        }
        hr = device_->CreateComputeShader(blob->GetBufferPointer(), blob->GetBufferSize(), nullptr, &out);
        // Release the blobs before the compiler DLL goes (their vtable lives there).
        blob->Release();
        if (errors) errors->Release();
        if (FAILED(hr)) {
            MELLO_LOG_ERROR(TAG, "CreateComputeShader failed: 0x%08X", hr);
            return false;
        }
        return true;
    };
    const bool ok = build("CSMain", cs_r8_) && build("CSMainNv12", cs_nv12_);
    FreeLibrary(dll);
    if (!ok) return false;

    D3D11_BUFFER_DESC cb_desc{};
    cb_desc.ByteWidth = sizeof(ConvertConstants);
    cb_desc.Usage = D3D11_USAGE_DEFAULT;
    cb_desc.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
    HRESULT hr = device_->CreateBuffer(&cb_desc, nullptr, &cb_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateBuffer (CB) failed: 0x%08X", hr);
        return false;
    }
    return true;
}

bool StagingTexture::initialize(const GraphicsDevice& device, bool enable_cpu_readback) {
    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);
    cpu_readback_enabled_ = enable_cpu_readback;
    if (!init_shaders()) {
        return false;
    }
    MELLO_LOG_INFO(TAG, "Viewer converter ready (GPU compute, keyed-mutex shared RGBA, cpu_readback=%s)",
                   cpu_readback_enabled_ ? "on" : "off");
    return true;
}

void StagingTexture::release_targets() {
    if (shared_rgba_handle_) {
        ::CloseHandle(reinterpret_cast<HANDLE>(shared_rgba_handle_));
    }
    shared_rgba_handle_ = nullptr;
    shared_mutex_.Reset();
    shared_tex_.Reset();
    rgba_uav_.Reset();
    rgba_tex_.Reset();
    staging_.Reset();
    width_ = 0;
    height_ = 0;
}

bool StagingTexture::ensure_targets(uint32_t width, uint32_t height) {
    if (rgba_tex_ && width == width_ && height == height_) return true;
    release_targets();

    D3D11_TEXTURE2D_DESC desc{};
    desc.Width = width;
    desc.Height = height;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.SampleDesc.Count = 1;
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = D3D11_BIND_UNORDERED_ACCESS;
    HRESULT hr = device_->CreateTexture2D(&desc, nullptr, &rgba_tex_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateTexture2D (RGBA %ux%u) failed: 0x%08X", width, height, hr);
        release_targets();
        return false;
    }
    hr = device_->CreateUnorderedAccessView(rgba_tex_.Get(), nullptr, &rgba_uav_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateUnorderedAccessView failed: 0x%08X", hr);
        release_targets();
        return false;
    }

    D3D11_TEXTURE2D_DESC shared_desc = desc;
    shared_desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    shared_desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX | D3D11_RESOURCE_MISC_SHARED_NTHANDLE;
    hr = device_->CreateTexture2D(&shared_desc, nullptr, &shared_tex_);
    Microsoft::WRL::ComPtr<IDXGIResource1> dxgi_res;
    HANDLE handle = nullptr;
    if (SUCCEEDED(hr)) hr = shared_tex_.As(&shared_mutex_);
    if (SUCCEEDED(hr)) hr = shared_tex_.As(&dxgi_res);
    if (SUCCEEDED(hr)) {
        hr = dxgi_res->CreateSharedHandle(nullptr, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                                          nullptr, &handle);
    }
    if (FAILED(hr) || !handle) {
        MELLO_LOG_ERROR(TAG, "Shared RGBA texture (keyed mutex) failed: 0x%08X", hr);
        release_targets();
        return false;
    }
    shared_rgba_handle_ = handle;
    ++shared_generation_;

    if (cpu_readback_enabled_) {
        D3D11_TEXTURE2D_DESC staging_desc = desc;
        staging_desc.BindFlags = 0;
        staging_desc.Usage = D3D11_USAGE_STAGING;
        staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
        hr = device_->CreateTexture2D(&staging_desc, nullptr, &staging_);
        if (FAILED(hr)) {
            MELLO_LOG_ERROR(TAG, "Staging texture failed: 0x%08X", hr);
            release_targets();
            return false;
        }
    }

    width_ = width;
    height_ = height;
    MELLO_LOG_INFO(TAG, "Viewer frame targets %ux%u (shared texture #%u)", width, height, shared_generation_);
    return true;
}

StagingTexture::SourceViews* StagingTexture::views_for(const DecodedPicture& picture) {
    ++use_counter_;
    for (SourceViews& v : views_) {
        if (v.texture.Get() == picture.texture.Get()) {
            v.last_use = use_counter_;
            return &v;
        }
    }
    SourceViews fresh;
    fresh.texture = picture.texture;
    fresh.last_use = use_counter_;
    D3D11_SHADER_RESOURCE_VIEW_DESC srv_desc{};
    srv_desc.Format = DXGI_FORMAT_R8_UNORM;
    srv_desc.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
    srv_desc.Texture2D.MipLevels = 1;
    HRESULT hr = device_->CreateShaderResourceView(picture.texture.Get(), &srv_desc, &fresh.first);
    if (SUCCEEDED(hr) && picture.format == DXGI_FORMAT_NV12) {
        srv_desc.Format = DXGI_FORMAT_R8G8_UNORM;
        hr = device_->CreateShaderResourceView(picture.texture.Get(), &srv_desc, &fresh.chroma);
    }
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Source view for the GPU convert failed: 0x%08X", hr);
        return nullptr;
    }
    if (views_.size() >= kMaxCachedSources) {
        auto oldest = std::min_element(views_.begin(), views_.end(),
                                       [](const SourceViews& a, const SourceViews& b) {
                                           return a.last_use < b.last_use;
                                       });
        *oldest = std::move(fresh);
        return &*oldest;
    }
    views_.push_back(std::move(fresh));
    return &views_.back();
}

bool StagingTexture::convert(const DecodedPicture& picture, bool copy_to_cpu) {
    if (!cs_r8_ || !picture.texture || picture.width == 0 || picture.height == 0) return false;
    if (!ensure_targets(picture.width, picture.height)) return false;
    SourceViews* views = views_for(picture);
    if (!views) return false;

    const bool nv12 = picture.format == DXGI_FORMAT_NV12;
    const ConvertConstants constants{picture.width, picture.height,
                                     nv12 ? 0u : (picture.uv_row ? picture.uv_row : picture.height), 0};
    context_->UpdateSubresource(cb_.Get(), 0, nullptr, &constants, 0, 0);

    ID3D11UnorderedAccessView* uavs[] = {rgba_uav_.Get()};
    ID3D11Buffer* cbs[] = {cb_.Get()};
    if (nv12) {
        context_->CSSetShader(cs_nv12_.Get(), nullptr, 0);
        ID3D11ShaderResourceView* srvs[] = {views->first.Get(), views->chroma.Get()};
        context_->CSSetShaderResources(1, 2, srvs);
    } else {
        context_->CSSetShader(cs_r8_.Get(), nullptr, 0);
        ID3D11ShaderResourceView* srvs[] = {views->first.Get()};
        context_->CSSetShaderResources(0, 1, srvs);
    }
    context_->CSSetUnorderedAccessViews(0, 1, uavs, nullptr);
    context_->CSSetConstantBuffers(0, 1, cbs);
    context_->Dispatch((picture.width + 15) / 16, (picture.height + 15) / 16, 1);

    ID3D11ShaderResourceView* null_srv[] = {nullptr, nullptr, nullptr};
    ID3D11UnorderedAccessView* null_uav[] = {nullptr};
    context_->CSSetShaderResources(0, 3, null_srv);
    context_->CSSetUnorderedAccessViews(0, 1, null_uav, nullptr);

    // Into the shared texture under its keyed mutex: the presenter reads it
    // under the same key, so it never sees half a frame.
    const HRESULT acquired = shared_mutex_->AcquireSync(0, kKeyedMutexTimeoutMs);
    if (acquired != S_OK) {
        MELLO_LOG_WARN(TAG, "Shared frame busy (keyed mutex 0x%08X): frame skipped", acquired);
        return false;
    }
    context_->CopyResource(shared_tex_.Get(), rgba_tex_.Get());
    shared_mutex_->ReleaseSync(0);

    if (copy_to_cpu && staging_) {
        context_->CopyResource(staging_.Get(), rgba_tex_.Get());
    }
    return true;
}

bool StagingTexture::read_rgba(uint8_t* out) {
    if (!staging_ || !out) {
        return false;
    }
    const auto t0 = std::chrono::steady_clock::now();
    D3D11_MAPPED_SUBRESOURCE mapped{};
    HRESULT hr = context_->Map(staging_.Get(), 0, D3D11_MAP_READ, 0, &mapped);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Map failed: hr=0x%08X", hr);
        return false;
    }
    const float stall_ms =
        std::chrono::duration<float, std::milli>(std::chrono::steady_clock::now() - t0).count();
    if (stall_ms > 50.0f) {
        MELLO_LOG_WARN(TAG, "Map() stall %.1fms -- possible GPU pipeline pressure", stall_ms);
    }
    const uint8_t* src = static_cast<const uint8_t*>(mapped.pData);
    const uint32_t row_bytes = width_ * 4;
    if (mapped.RowPitch == row_bytes) {
        memcpy(out, src, static_cast<size_t>(row_bytes) * height_);
    } else {
        for (uint32_t row = 0; row < height_; ++row) {
            memcpy(out + static_cast<size_t>(row) * row_bytes, src + static_cast<size_t>(row) * mapped.RowPitch,
                   row_bytes);
        }
    }
    context_->Unmap(staging_.Get(), 0);
    return true;
}

void StagingTexture::shutdown() {
    release_targets();
    views_.clear();
    cb_.Reset();
    cs_r8_.Reset();
    cs_nv12_.Reset();
    context_.Reset();
    device_.Reset();
}

} // namespace mello::video
#endif
