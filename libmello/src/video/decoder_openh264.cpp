#ifdef _WIN32
#include "decoder_openh264.hpp"
#include "openh264_dynload.hpp"
#include "../util/log.hpp"
#include <cstring>

// The OpenH264 v2.6.0 API headers (third_party/openh264/include/wels, the
// version of the Cisco DLL), in their own namespace as in
// encoder_openh264.cpp. They replace a hand-written copy of the ABI whose
// SDecodingParam put the bitstream type (AVC = 2) on bParseOnly, so the DLL
// decoded nothing ("bParseOnly should be false"), and whose SBufferInfo
// lacked pDst[3], so the DLL wrote 24 bytes past it on the stack.
#include <stdbool.h>
namespace mello::video::wels_abi {
#include "wels/codec_api.h"
}

namespace mello::video {

namespace wa = wels_abi;
static constexpr const char* TAG = "video/decoder";

bool OpenH264Decoder::is_available() {
    return openh264::load();
}

bool OpenH264Decoder::initialize(const GraphicsDevice& device, const DecoderConfig& config) {
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
        MELLO_LOG_ERROR(TAG, "OpenH264 decoder: Failed to create upload texture: hr=0x%08X", hr);
        return false;
    }

    if (!openh264::load()) {
        MELLO_LOG_WARN(TAG, "OpenH264 DLL not available");
        return false;
    }

    auto& dll = openh264::api();
    long rv = dll.create_decoder(reinterpret_cast<void**>(&decoder_));
    if (rv != 0 || !decoder_) {
        MELLO_LOG_ERROR(TAG, "OpenH264: WelsCreateDecoder failed: %ld", rv);
        return false;
    }

    wa::SDecodingParam param{};
    memset(&param, 0, sizeof(param));
    param.sVideoProperty.size = sizeof(param.sVideoProperty);
    param.sVideoProperty.eVideoBsType = wa::VIDEO_BITSTREAM_AVC;
    param.bParseOnly = false;

    rv = decoder_->Initialize(&param);
    if (rv != 0) {
        MELLO_LOG_ERROR(TAG, "OpenH264: Initialize failed: %ld", rv);
        dll.destroy_decoder(decoder_);
        decoder_ = nullptr;
        return false;
    }

    MELLO_LOG_INFO(TAG, "Selected decoder: OpenH264 (Cisco DLL) codec=H264 resolution=%ux%u",
        config.width, config.height);
    return true;
}

void OpenH264Decoder::shutdown() {
    if (decoder_) {
        decoder_->Uninitialize();
        if (openh264::is_loaded()) {
            openh264::api().destroy_decoder(decoder_);
        }
        decoder_ = nullptr;
    }
    upload_tex_.Reset();
    context_.Reset();
    device_.Reset();
    nv12_buf_.clear();
}

DecodeFeedResult OpenH264Decoder::decode(const uint8_t* data, size_t size, bool is_keyframe) {
    (void)is_keyframe;
    if (!decoder_) return DecodeFeedResult::Error;

    unsigned char* yuv[3] = { nullptr, nullptr, nullptr };
    wa::SBufferInfo buf_info{};
    memset(&buf_info, 0, sizeof(buf_info));

    const int rv = decoder_->DecodeFrameNoDelay(data, static_cast<int>(size), yuv, &buf_info);

    if (rv != wa::dsErrorFree) {
        MELLO_LOG_ERROR(TAG, "OpenH264: DecodeFrameNoDelay failed: state=0x%X", rv);
        return DecodeFeedResult::Error;
    }
    if (buf_info.iBufferStatus == 0) return DecodeFeedResult::Accepted;
    if (buf_info.iBufferStatus != 1 || !yuv[0] || !yuv[1] || !yuv[2]) {
        MELLO_LOG_ERROR(TAG, "OpenH264: invalid decoded frame status=%d", buf_info.iBufferStatus);
        return DecodeFeedResult::Error;
    }

    uint32_t w = config_.width;
    uint32_t h = config_.height;
    int y_stride  = buf_info.UsrData.sSystemBuffer.iStride[0];
    int uv_stride = buf_info.UsrData.sSystemBuffer.iStride[1];

    // Convert I420 -> NV12 for D3D11 texture upload
    uint8_t* nv12_y  = nv12_buf_.data();
    uint8_t* nv12_uv = nv12_y + w * h;

    // Copy Y plane
    for (uint32_t row = 0; row < h; ++row) {
        memcpy(nv12_y + row * w, yuv[0] + row * y_stride, w);
    }

    // Interleave U + V into NV12 UV plane
    uint32_t uv_h = h / 2;
    uint32_t uv_w = w / 2;
    for (uint32_t row = 0; row < uv_h; ++row) {
        const uint8_t* u_row = yuv[1] + row * uv_stride;
        const uint8_t* v_row = yuv[2] + row * uv_stride;
        uint8_t* dst = nv12_uv + row * w;
        for (uint32_t col = 0; col < uv_w; ++col) {
            dst[col * 2]     = u_row[col];
            dst[col * 2 + 1] = v_row[col];
        }
    }

    context_->UpdateSubresource(
        upload_tex_.Get(), 0, nullptr,
        nv12_buf_.data(), w,
        static_cast<UINT>(nv12_buf_.size()));

    return DecodeFeedResult::FrameReady;
}

ID3D11Texture2D* OpenH264Decoder::get_frame() {
    return upload_tex_.Get();
}

} // namespace mello::video
#endif
