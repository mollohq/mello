#ifdef _WIN32
#include "encoder_openh264.hpp"
#include "openh264_dynload.hpp"
#include "../util/log.hpp"

#include <stdbool.h>
#include <algorithm>
#include <cstring>

// The OpenH264 v2.6.0 API headers (third_party/openh264/include/wels, BSD-2,
// the same version as the Cisco DLL). They go into their own namespace:
// decoder_openh264.cpp declares a minimal C ABI with the same global names
// (ISVCDecoder, SDecodingParam), and two definitions of one name would break
// the one-definition rule. <stdbool.h> is included above, so its include
// guard keeps it out of the namespace.
namespace mello::video::wels_abi {
#include "wels/codec_api.h"
}

namespace mello::video {

namespace wa = wels_abi;
static constexpr const char* TAG = "video/encoder";

OpenH264Encoder::~OpenH264Encoder() {
    shutdown();
}

bool OpenH264Encoder::is_available() {
    return openh264::load() && openh264::encoder_api_loaded();
}

bool OpenH264Encoder::initialize(const GraphicsDevice& device, const EncoderConfig& config) {
    if (config.codec != VideoCodec::H264) {
        MELLO_LOG_WARN(TAG, "OpenH264: only H.264 is supported");
        return false;
    }
    if (!is_available()) {
        MELLO_LOG_WARN(TAG, "OpenH264: DLL or encoder entry points not available");
        return false;
    }
    if (config.width == 0 || config.height == 0 || (config.width & 1) || (config.height & 1)) {
        MELLO_LOG_ERROR(TAG, "OpenH264: invalid size %ux%u", config.width, config.height);
        return false;
    }
    config_ = config;
    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);

    D3D11_TEXTURE2D_DESC desc{};
    desc.Width = config.width;
    desc.Height = config.height;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = DXGI_FORMAT_NV12;
    desc.SampleDesc.Count = 1;
    desc.Usage = D3D11_USAGE_STAGING;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    HRESULT hr = device_->CreateTexture2D(&desc, nullptr, &staging_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "OpenH264: staging texture failed: hr=0x%08X", hr);
        return false;
    }

    const size_t luma = static_cast<size_t>(config.width) * config.height;
    y_.resize(luma);
    u_.resize(luma / 4);
    v_.resize(luma / 4);

    wa::ISVCEncoder* enc = nullptr;
    const int rv = openh264::encoder_api().create_encoder(reinterpret_cast<void**>(&enc));
    if (rv != 0 || !enc) {
        MELLO_LOG_ERROR(TAG, "OpenH264: WelsCreateSVCEncoder failed: %d", rv);
        return false;
    }

    wa::SEncParamExt p{};
    enc->GetDefaultParams(&p);
    const uint32_t fps = config.fps > 0 ? config.fps : 60;
    const int bps = static_cast<int>(config.bitrate_kbps) * 1000;
    p.iUsageType = wa::CAMERA_VIDEO_REAL_TIME;
    p.iPicWidth = static_cast<int>(config.width);
    p.iPicHeight = static_cast<int>(config.height);
    p.iTargetBitrate = bps;
    // Same headroom as NVENC VBR: an IDR may use up to 1.25x the target.
    p.iMaxBitrate = bps + bps / 4;
    p.iRCMode = wa::RC_BITRATE_MODE;
    p.fMaxFrameRate = static_cast<float>(fps);
    p.iTemporalLayerNum = 1;
    p.iSpatialLayerNum = 1;
    p.iComplexityMode = wa::LOW_COMPLEXITY;
    p.uiIntraPeriod = config.keyframe_interval;
    p.iNumRefFrame = 1;
    p.eSpsPpsIdStrategy = wa::CONSTANT_ID;
    p.bPrefixNalAddingCtrl = false;
    p.bEnableSSEI = false;
    p.iEntropyCodingModeFlag = 0;
    // A real-time sender never skips a frame on its own: the pipeline and the
    // transport decide what to drop, as with NVENC.
    p.bEnableFrameSkip = false;
    p.bEnableLongTermReference = false;
    // One slice, so one thread: the slice layout is the one the hardware
    // encoders give.
    p.iMultipleThreadIdc = 1;
    p.bEnableDenoise = false;
    p.bEnableBackgroundDetection = false;
    p.bEnableAdaptiveQuant = true;
    p.bEnableSceneChangeDetect = false;
    p.bEnableFrameCroppingFlag = true;

    wa::SSpatialLayerConfig& layer = p.sSpatialLayers[0];
    layer.iVideoWidth = static_cast<int>(config.width);
    layer.iVideoHeight = static_cast<int>(config.height);
    layer.fFrameRate = static_cast<float>(fps);
    layer.iSpatialBitrate = bps;
    layer.iMaxSpatialBitrate = p.iMaxBitrate;
    layer.uiProfileIdc = wa::PRO_BASELINE;
    layer.sSliceArgument.uiSliceMode = wa::SM_SINGLE_SLICE;
    layer.sSliceArgument.uiSliceNum = 1;
    // BT.709 studio range, as the preprocessor converts and NVENC signals.
    layer.bVideoSignalTypePresent = true;
    layer.uiVideoFormat = wa::VF_UNDEF;
    layer.bFullRange = false;
    layer.bColorDescriptionPresent = true;
    layer.uiColorPrimaries = wa::CP_BT709;
    layer.uiTransferCharacteristics = wa::TRC_BT709;
    layer.uiColorMatrix = wa::CM_BT709;

    const int ret = enc->InitializeExt(&p);
    if (ret != 0) {
        MELLO_LOG_ERROR(TAG, "OpenH264: InitializeExt failed: %d (%ux%u %u fps %u kbps)", ret,
                        config.width, config.height, fps, config.bitrate_kbps);
        openh264::encoder_api().destroy_encoder(enc);
        return false;
    }
    int trace = wa::WELS_LOG_ERROR;
    enc->SetOption(wa::ENCODER_OPTION_TRACE_LEVEL, &trace);
    int format = wa::videoFormatI420;
    enc->SetOption(wa::ENCODER_OPTION_DATAFORMAT, &format);

    encoder_ = enc;
    applied_fps_ = fps;
    frame_seq_ = 0;
    stats_ = {};
    MELLO_LOG_INFO(TAG,
                   "Selected encoder: OpenH264 (software, MELLO_SOFT_ENCODER) %ux%u %u fps "
                   "%u kbps idr_interval=%u",
                   config.width, config.height, fps, config.bitrate_kbps,
                   config.keyframe_interval);
    return true;
}

void OpenH264Encoder::shutdown() {
    if (encoder_) {
        encoder_->Uninitialize();
        if (openh264::is_loaded() && openh264::encoder_api_loaded()) {
            openh264::encoder_api().destroy_encoder(encoder_);
        }
        encoder_ = nullptr;
    }
    staging_.Reset();
    context_.Reset();
    device_.Reset();
}

bool OpenH264Encoder::read_back(ID3D11Texture2D* nv12_texture) {
    context_->CopyResource(staging_.Get(), nv12_texture);
    D3D11_MAPPED_SUBRESOURCE mapped{};
    const HRESULT hr = context_->Map(staging_.Get(), 0, D3D11_MAP_READ, 0, &mapped);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "OpenH264: Map of the NV12 staging texture failed: hr=0x%08X", hr);
        return false;
    }
    const uint32_t w = config_.width;
    const uint32_t h = config_.height;
    const auto* src = static_cast<const uint8_t*>(mapped.pData);
    for (uint32_t row = 0; row < h; ++row) {
        std::memcpy(y_.data() + static_cast<size_t>(row) * w, src + static_cast<size_t>(row) * mapped.RowPitch, w);
    }
    // NV12 keeps the chroma plane right after `h` rows of the luma pitch.
    const uint8_t* uv = src + static_cast<size_t>(mapped.RowPitch) * h;
    const uint32_t cw = w / 2;
    for (uint32_t row = 0; row < h / 2; ++row) {
        const uint8_t* line = uv + static_cast<size_t>(row) * mapped.RowPitch;
        uint8_t* ud = u_.data() + static_cast<size_t>(row) * cw;
        uint8_t* vd = v_.data() + static_cast<size_t>(row) * cw;
        for (uint32_t col = 0; col < cw; ++col) {
            ud[col] = line[col * 2];
            vd[col] = line[col * 2 + 1];
        }
    }
    context_->Unmap(staging_.Get(), 0);
    return true;
}

void OpenH264Encoder::apply_pending_controls() {
    const uint32_t kbps = pending_bitrate_kbps_.exchange(0, std::memory_order_relaxed);
    if (kbps > 0 && kbps != config_.bitrate_kbps) {
        const int bps = static_cast<int>(kbps) * 1000;
        wa::SBitrateInfo max_info{wa::SPATIAL_LAYER_ALL, bps + bps / 4};
        wa::SBitrateInfo target{wa::SPATIAL_LAYER_ALL, bps};
        // Raise the ceiling first on a step up, lower it last on a step down,
        // so the target never sits above the maximum.
        if (kbps > config_.bitrate_kbps) {
            encoder_->SetOption(wa::ENCODER_OPTION_MAX_BITRATE, &max_info);
            encoder_->SetOption(wa::ENCODER_OPTION_BITRATE, &target);
        } else {
            encoder_->SetOption(wa::ENCODER_OPTION_BITRATE, &target);
            encoder_->SetOption(wa::ENCODER_OPTION_MAX_BITRATE, &max_info);
        }
        MELLO_LOG_DEBUG(TAG, "OpenH264: bitrate %u -> %u kbps", config_.bitrate_kbps, kbps);
        config_.bitrate_kbps = kbps;
    }
    const uint32_t fps = pending_fps_.exchange(0, std::memory_order_relaxed);
    if (fps > 0 && fps != applied_fps_) {
        float rate = static_cast<float>(fps);
        encoder_->SetOption(wa::ENCODER_OPTION_FRAME_RATE, &rate);
        MELLO_LOG_DEBUG(TAG, "OpenH264: frame rate %u -> %u fps", applied_fps_, fps);
        applied_fps_ = fps;
    }
    if (force_idr_.exchange(false, std::memory_order_relaxed)) {
        encoder_->ForceIntraFrame(true);
    }
}

bool OpenH264Encoder::encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) {
    if (!encoder_ || !nv12_texture) return false;
    const auto t0 = std::chrono::steady_clock::now();
    if (!read_back(nv12_texture)) return false;
    const auto t1 = std::chrono::steady_clock::now();

    apply_pending_controls();

    wa::SSourcePicture pic{};
    pic.iColorFormat = wa::videoFormatI420;
    pic.iPicWidth = static_cast<int>(config_.width);
    pic.iPicHeight = static_cast<int>(config_.height);
    pic.iStride[0] = static_cast<int>(config_.width);
    pic.iStride[1] = static_cast<int>(config_.width / 2);
    pic.iStride[2] = static_cast<int>(config_.width / 2);
    pic.pData[0] = y_.data();
    pic.pData[1] = u_.data();
    pic.pData[2] = v_.data();
    const uint32_t fps = applied_fps_ > 0 ? applied_fps_ : 60;
    pic.uiTimeStamp = static_cast<long long>(frame_seq_ * 1000 / fps);

    wa::SFrameBSInfo info{};
    const int rv = encoder_->EncodeFrame(&pic, &info);
    const auto t2 = std::chrono::steady_clock::now();
    phases_.lock_ms = std::chrono::duration<double, std::milli>(t1 - t0).count();
    phases_.submit_ms = std::chrono::duration<double, std::milli>(t2 - t1).count();
    phases_.wait_ms = 0.0;
    frame_seq_++;
    if (rv != 0) {
        MELLO_LOG_ERROR(TAG, "OpenH264: EncodeFrame failed: %d", rv);
        return false;
    }
    if (info.eFrameType == wa::videoFrameTypeSkip || info.eFrameType == wa::videoFrameTypeInvalid) {
        return false;
    }

    out.data.clear();
    out.data.reserve(static_cast<size_t>(std::max(info.iFrameSizeInBytes, 0)));
    for (int l = 0; l < info.iLayerNum; ++l) {
        const wa::SLayerBSInfo& layer = info.sLayerInfo[l];
        size_t bytes = 0;
        for (int n = 0; n < layer.iNalCount; ++n) {
            bytes += static_cast<size_t>(layer.pNalLengthInByte[n]);
        }
        out.data.insert(out.data.end(), layer.pBsBuf, layer.pBsBuf + bytes);
    }
    if (out.data.empty()) return false;
    out.is_keyframe = (info.eFrameType == wa::videoFrameTypeIDR);
    out.timestamp_us = 0;

    stats_.bytes_sent += out.data.size();
    if (out.is_keyframe) stats_.keyframes_sent++;
    stats_window_frames_++;
    stats_window_bytes_ += out.data.size();
    if (stats_window_start_.time_since_epoch().count() == 0) stats_window_start_ = t2;
    const auto elapsed_us =
        std::chrono::duration_cast<std::chrono::microseconds>(t2 - stats_window_start_).count();
    if (elapsed_us >= 1'000'000) {
        stats_.fps_actual = static_cast<uint32_t>(
            (static_cast<uint64_t>(stats_window_frames_) * 1'000'000) / static_cast<uint64_t>(elapsed_us));
        stats_.bitrate_kbps = static_cast<uint32_t>(
            (stats_window_bytes_ * 8) / static_cast<uint64_t>(elapsed_us / 1'000));
        stats_window_frames_ = 0;
        stats_window_bytes_ = 0;
        stats_window_start_ = t2;
    }
    return true;
}

void OpenH264Encoder::request_keyframe() {
    force_idr_.store(true, std::memory_order_relaxed);
}

void OpenH264Encoder::set_bitrate(uint32_t kbps) {
    if (kbps > 0) pending_bitrate_kbps_.store(kbps, std::memory_order_relaxed);
}

void OpenH264Encoder::set_framerate(uint32_t fps) {
    if (fps > 0) pending_fps_.store(fps, std::memory_order_relaxed);
}

void OpenH264Encoder::get_stats(EncoderStats& out) const {
    out = stats_;
}

void OpenH264Encoder::get_phase_timing(EncodePhaseTiming& out) const {
    out = phases_;
}

} // namespace mello::video
#endif
