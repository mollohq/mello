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
    for (Slot& slot : slots_) slot = Slot{};
    context_.Reset();
    device_.Reset();
    nv12_buf_.clear();
}

DecodeFeedResult OpenH264Decoder::decode(const uint8_t* data, size_t size, bool is_keyframe, int slot_index) {
    (void)is_keyframe;
    if (!decoder_ || slot_index < 0 || slot_index >= kOutputSlots) return DecodeFeedResult::Error;

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

    // The picture size comes from the SPS, not from the viewer's config: a
    // host that changes its encode size changes it here.
    const uint32_t w = static_cast<uint32_t>(buf_info.UsrData.sSystemBuffer.iWidth) & ~1u;
    const uint32_t h = static_cast<uint32_t>(buf_info.UsrData.sSystemBuffer.iHeight) & ~1u;
    if (w == 0 || h == 0) {
        MELLO_LOG_ERROR(TAG, "OpenH264: decoded frame without a size");
        return DecodeFeedResult::Error;
    }
    const int y_stride  = buf_info.UsrData.sSystemBuffer.iStride[0];
    const int uv_stride = buf_info.UsrData.sSystemBuffer.iStride[1];

    Slot& slot = slots_[slot_index];
    if (!slot.texture || slot.width != w || slot.height != h) {
        D3D11_TEXTURE2D_DESC tex_desc{};
        tex_desc.Width  = w;
        tex_desc.Height = h;
        tex_desc.MipLevels = 1;
        tex_desc.ArraySize = 1;
        tex_desc.Format = DXGI_FORMAT_NV12;
        tex_desc.SampleDesc.Count = 1;
        tex_desc.Usage = D3D11_USAGE_DEFAULT;
        tex_desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;
        Microsoft::WRL::ComPtr<ID3D11Texture2D> texture;
        HRESULT hr = device_->CreateTexture2D(&tex_desc, nullptr, &texture);
        if (FAILED(hr)) {
            MELLO_LOG_ERROR(TAG, "OpenH264: slot texture %ux%u failed: hr=0x%08X", w, h, hr);
            return DecodeFeedResult::Error;
        }
        slot.texture = texture;
        slot.width = w;
        slot.height = h;
    }

    // I420 to NV12 for the upload.
    nv12_buf_.resize(static_cast<size_t>(w) * h * 3 / 2);
    uint8_t* nv12_y  = nv12_buf_.data();
    uint8_t* nv12_uv = nv12_y + static_cast<size_t>(w) * h;
    for (uint32_t row = 0; row < h; ++row) {
        memcpy(nv12_y + static_cast<size_t>(row) * w, yuv[0] + static_cast<size_t>(row) * y_stride, w);
    }
    const uint32_t uv_h = h / 2;
    const uint32_t uv_w = w / 2;
    for (uint32_t row = 0; row < uv_h; ++row) {
        const uint8_t* u_row = yuv[1] + static_cast<size_t>(row) * uv_stride;
        const uint8_t* v_row = yuv[2] + static_cast<size_t>(row) * uv_stride;
        uint8_t* dst = nv12_uv + static_cast<size_t>(row) * w;
        for (uint32_t col = 0; col < uv_w; ++col) {
            dst[col * 2]     = u_row[col];
            dst[col * 2 + 1] = v_row[col];
        }
    }

    context_->UpdateSubresource(
        slot.texture.Get(), 0, nullptr,
        nv12_buf_.data(), w,
        static_cast<UINT>(nv12_buf_.size()));

    return DecodeFeedResult::FrameReady;
}

bool OpenH264Decoder::picture(int slot_index, DecodedPicture& out) const {
    if (slot_index < 0 || slot_index >= kOutputSlots || !slots_[slot_index].texture) return false;
    const Slot& slot = slots_[slot_index];
    out.texture = slot.texture;
    out.format = DXGI_FORMAT_NV12;
    out.width = slot.width;
    out.height = slot.height;
    out.uv_row = 0;
    return true;
}

} // namespace mello::video
#endif
