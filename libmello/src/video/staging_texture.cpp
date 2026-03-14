#ifdef _WIN32
#include "staging_texture.hpp"
#include "../util/log.hpp"
#include <algorithm>
#include <chrono>

namespace mello::video {

static constexpr const char* TAG = "video/staging";

bool StagingTexture::initialize(const GraphicsDevice& device, uint32_t width, uint32_t height) {
    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);
    width_  = width;
    height_ = height;

    D3D11_TEXTURE2D_DESC desc{};
    desc.Width  = width;
    desc.Height = height;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = DXGI_FORMAT_NV12;
    desc.SampleDesc.Count = 1;
    desc.Usage = D3D11_USAGE_STAGING;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;

    HRESULT hr = device_->CreateTexture2D(&desc, nullptr, &staging_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Failed to create staging texture: hr=0x%08X", hr);
        return false;
    }

    MELLO_LOG_INFO(TAG, "Staging texture initialized: %ux%u NV12", width, height);
    return true;
}

void StagingTexture::copy_from(ID3D11Texture2D* nv12_source) {
    context_->CopyResource(staging_.Get(), nv12_source);
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

    // NV12 -> RGBA conversion in CPU
    // NV12 layout in staging texture: Y plane at row 0..height-1, UV plane at row height..height*3/2-1
    const uint8_t* y_plane  = static_cast<const uint8_t*>(mapped.pData);
    const uint8_t* uv_plane = y_plane + mapped.RowPitch * height_;

    if (read_count_ < 3) {
        uint32_t cx = width_ / 2;
        uint32_t cy = height_ / 2;
        uint8_t y_tl = y_plane[0];
        uint8_t y_c  = y_plane[cy * mapped.RowPitch + cx];
        uint8_t u_c  = uv_plane[(cy / 2) * mapped.RowPitch + (cx & ~1u)];
        uint8_t v_c  = uv_plane[(cy / 2) * mapped.RowPitch + (cx & ~1u) + 1];
        MELLO_LOG_DEBUG(TAG, "read_rgba[%llu]: pitch=%u Y[0,0]=%u Y[center]=%u UV[center]=(%u,%u)",
            read_count_, mapped.RowPitch, y_tl, y_c, u_c, v_c);
        read_count_++;
    }

    for (uint32_t row = 0; row < height_; ++row) {
        const uint8_t* y_row  = y_plane + row * mapped.RowPitch;
        const uint8_t* uv_row = uv_plane + (row / 2) * mapped.RowPitch;
        uint8_t* dst = out + row * width_ * 4;

        for (uint32_t col = 0; col < width_; ++col) {
            uint8_t y = y_row[col];
            uint8_t u = uv_row[(col & ~1u)];
            uint8_t v = uv_row[(col & ~1u) + 1];

            // BT.601 YUV -> RGB (full range)
            int c = y - 16;
            int d = u - 128;
            int e = v - 128;

            int r = (298 * c + 409 * e + 128) >> 8;
            int g = (298 * c - 100 * d - 208 * e + 128) >> 8;
            int b = (298 * c + 516 * d + 128) >> 8;

            dst[col * 4 + 0] = static_cast<uint8_t>(std::clamp(r, 0, 255));
            dst[col * 4 + 1] = static_cast<uint8_t>(std::clamp(g, 0, 255));
            dst[col * 4 + 2] = static_cast<uint8_t>(std::clamp(b, 0, 255));
            dst[col * 4 + 3] = 255;
        }
    }

    context_->Unmap(staging_.Get(), 0);
}

void StagingTexture::shutdown() {
    staging_.Reset();
    context_.Reset();
    device_.Reset();
}

} // namespace mello::video
#endif
