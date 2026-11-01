#if defined(__linux__)
#include "decoder_openh264.hpp"
#include "openh264_dynload.hpp"
#include "../util/log.hpp"
#include <cstring>

// The OpenH264 v2.6.0 API headers in their own namespace, as in
// decoder_openh264.cpp.
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
    (void)device; // CPU only
    config_ = config;

    if (!openh264::load()) {
        MELLO_LOG_WARN(TAG, "OpenH264 library not available");
        return false;
    }

    auto& lib = openh264::api();
    long rv = lib.create_decoder(reinterpret_cast<void**>(&decoder_));
    if (rv != 0 || !decoder_) {
        MELLO_LOG_ERROR(TAG, "OpenH264: WelsCreateDecoder failed: %ld", rv);
        decoder_ = nullptr;
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
        lib.destroy_decoder(decoder_);
        decoder_ = nullptr;
        return false;
    }

    MELLO_LOG_INFO(TAG, "Selected decoder: OpenH264 (CPU) codec=H264 resolution=%ux%u",
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

    // The picture size comes from the SPS, not from the viewer's config.
    const uint32_t w = static_cast<uint32_t>(buf_info.UsrData.sSystemBuffer.iWidth) & ~1u;
    const uint32_t h = static_cast<uint32_t>(buf_info.UsrData.sSystemBuffer.iHeight) & ~1u;
    if (w == 0 || h == 0) {
        MELLO_LOG_ERROR(TAG, "OpenH264: decoded frame without a size");
        return DecodeFeedResult::Error;
    }
    const size_t y_stride  = static_cast<size_t>(buf_info.UsrData.sSystemBuffer.iStride[0]);
    const size_t uv_stride = static_cast<size_t>(buf_info.UsrData.sSystemBuffer.iStride[1]);

    // The decoder's own buffer changes on the next decode, so the picture is
    // copied into the slot. The ring never decodes into a slot it presents.
    Slot& slot = slots_[slot_index];
    const size_t y_size = static_cast<size_t>(w) * h;
    const uint32_t uv_w = w / 2;
    const uint32_t uv_h = h / 2;
    const size_t uv_size = static_cast<size_t>(uv_w) * uv_h;
    slot.i420.resize(y_size + 2 * uv_size);
    uint8_t* dst_y = slot.i420.data();
    uint8_t* dst_u = dst_y + y_size;
    uint8_t* dst_v = dst_u + uv_size;
    for (uint32_t row = 0; row < h; ++row) {
        memcpy(dst_y + static_cast<size_t>(row) * w, yuv[0] + row * y_stride, w);
    }
    for (uint32_t row = 0; row < uv_h; ++row) {
        memcpy(dst_u + static_cast<size_t>(row) * uv_w, yuv[1] + row * uv_stride, uv_w);
        memcpy(dst_v + static_cast<size_t>(row) * uv_w, yuv[2] + row * uv_stride, uv_w);
    }
    slot.width = w;
    slot.height = h;
    return DecodeFeedResult::FrameReady;
}

bool OpenH264Decoder::picture(int slot_index, CpuPicture& out) const {
    if (slot_index < 0 || slot_index >= kOutputSlots) return false;
    const Slot& slot = slots_[slot_index];
    if (slot.i420.empty() || slot.width == 0 || slot.height == 0) return false;
    const size_t y_size = static_cast<size_t>(slot.width) * slot.height;
    const size_t uv_size = y_size / 4;
    out.y = slot.i420.data();
    out.u = out.y + y_size;
    out.v = out.u + uv_size;
    out.y_stride = slot.width;
    out.uv_stride = slot.width / 2;
    out.width = slot.width;
    out.height = slot.height;
    return true;
}

} // namespace mello::video
#endif
