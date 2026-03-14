#ifdef _WIN32
#include "decoder_dav1d.hpp"
#include "../util/log.hpp"
#include <cstring>

namespace mello::video {

static constexpr const char* TAG = "video/decoder";

bool Dav1dDecoder::is_available() {
#ifdef MELLO_HAS_DAV1D
    return true;
#else
    return false;
#endif
}

bool Dav1dDecoder::initialize(const GraphicsDevice& device, const DecoderConfig& config) {
#ifdef MELLO_HAS_DAV1D
    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);
    config_ = config;

    nv12_buf_.resize(static_cast<size_t>(config.width) * config.height * 3 / 2);

    D3D11_TEXTURE2D_DESC tex_desc{};
    tex_desc.Width  = config.width;
    tex_desc.Height = config.height;
    tex_desc.MipLevels = 1;
    tex_desc.ArraySize = 1;
    tex_desc.Format = DXGI_FORMAT_NV12;
    tex_desc.SampleDesc.Count = 1;
    tex_desc.Usage = D3D11_USAGE_DEFAULT;
    tex_desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;

    HRESULT hr = device_->CreateTexture2D(&tex_desc, nullptr, &upload_tex_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "dav1d: Failed to create upload texture: hr=0x%08X", hr);
        return false;
    }

    dav1d_default_settings(&settings_);
    settings_.n_threads = 2;
    settings_.max_frame_delay = 1;

    int rv = dav1d_open(&ctx_, &settings_);
    if (rv < 0) {
        MELLO_LOG_ERROR(TAG, "dav1d: dav1d_open failed: %d", rv);
        return false;
    }

    MELLO_LOG_INFO(TAG, "Selected decoder: dav1d codec=AV1 resolution=%ux%u",
        config.width, config.height);
    return true;
#else
    (void)device; (void)config;
    MELLO_LOG_ERROR(TAG, "dav1d not available at build time");
    return false;
#endif
}

void Dav1dDecoder::shutdown() {
#ifdef MELLO_HAS_DAV1D
    if (ctx_) {
        dav1d_flush(ctx_);
        dav1d_close(&ctx_);
        ctx_ = nullptr;
    }
#endif
    upload_tex_.Reset();
    context_.Reset();
    device_.Reset();
    nv12_buf_.clear();
}

bool Dav1dDecoder::decode(const uint8_t* data, size_t size, bool is_keyframe) {
    (void)is_keyframe;

#ifdef MELLO_HAS_DAV1D
    if (!ctx_) return false;

    Dav1dData dav1d_data{};
    int rv = dav1d_data_wrap(&dav1d_data, data, size, nullptr, nullptr);
    if (rv < 0) return false;

    rv = dav1d_send_data(ctx_, &dav1d_data);
    if (rv < 0 && rv != DAV1D_ERR(EAGAIN)) {
        dav1d_data_unref(&dav1d_data);
        return false;
    }

    Dav1dPicture pic{};
    rv = dav1d_get_picture(ctx_, &pic);
    if (rv < 0) return false;

    // Only handle 8-bit YUV420
    if (pic.p.bpc != 8 || pic.p.layout != DAV1D_PIXEL_LAYOUT_I420) {
        MELLO_LOG_WARN(TAG, "dav1d: unsupported pixel format bpc=%d layout=%d", pic.p.bpc, pic.p.layout);
        dav1d_picture_unref(&pic);
        return false;
    }

    uint32_t w = config_.width;
    uint32_t h = config_.height;

    // Convert I420 -> NV12
    uint8_t* nv12_y  = nv12_buf_.data();
    uint8_t* nv12_uv = nv12_y + w * h;

    const uint8_t* y_src = static_cast<const uint8_t*>(pic.data[0]);
    const uint8_t* u_src = static_cast<const uint8_t*>(pic.data[1]);
    const uint8_t* v_src = static_cast<const uint8_t*>(pic.data[2]);
    ptrdiff_t y_stride = pic.stride[0];
    ptrdiff_t uv_stride = pic.stride[1];

    for (uint32_t row = 0; row < h; ++row) {
        memcpy(nv12_y + row * w, y_src + row * y_stride, w);
    }

    uint32_t uv_h = h / 2;
    uint32_t uv_w = w / 2;
    for (uint32_t row = 0; row < uv_h; ++row) {
        const uint8_t* u_row = u_src + row * uv_stride;
        const uint8_t* v_row = v_src + row * uv_stride;
        uint8_t* dst = nv12_uv + row * w;
        for (uint32_t col = 0; col < uv_w; ++col) {
            dst[col * 2]     = u_row[col];
            dst[col * 2 + 1] = v_row[col];
        }
    }

    dav1d_picture_unref(&pic);

    context_->UpdateSubresource(
        upload_tex_.Get(), 0, nullptr,
        nv12_buf_.data(), w,
        static_cast<UINT>(nv12_buf_.size()));

    return true;
#else
    (void)data; (void)size;
    return false;
#endif
}

ID3D11Texture2D* Dav1dDecoder::get_frame() {
    return upload_tex_.Get();
}

} // namespace mello::video
#endif
