#ifdef _WIN32
#include "encoder_amf.hpp"
#include "../util/log.hpp"
#include <Windows.h>
#include <algorithm>
#include <chrono>

#include <AMF/core/Data.h>
#include <AMF/core/Surface.h>
typedef AMF_RESULT(AMF_CDECL_CALL* AMFInit_Fn)(amf_uint64, amf::AMFFactory**);

namespace mello::video {

static constexpr const char* TAG = "video/encoder";

static HMODULE load_amf_dll() {
    return LoadLibraryA("amfrt64.dll");
}

bool AmfEncoder::is_available() {
    HMODULE dll = load_amf_dll();
    if (dll) { FreeLibrary(dll); return true; }
    return false;
}

bool AmfEncoder::initialize(const GraphicsDevice& device, const EncoderConfig& config) {
    device_ = device.d3d11();
    config_ = config;
    codec_  = config.codec;
    stats_  = {};
    frame_seq_ = 0;

    dll_ = load_amf_dll();
    if (!dll_) {
        MELLO_LOG_DEBUG(TAG, "Probing AMF... not available (amfrt64.dll not found)");
        return false;
    }

    auto amf_init = reinterpret_cast<AMFInit_Fn>(GetProcAddress(dll_, AMF_INIT_FUNCTION_NAME));
    if (!amf_init) {
        MELLO_LOG_DEBUG(TAG, "Probing AMF... init function not found");
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    AMF_RESULT res = amf_init(AMF_FULL_VERSION, &factory_);
    if (res != AMF_OK || !factory_) {
        MELLO_LOG_DEBUG(TAG, "Probing AMF... AMFInit failed: %d", res);
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    // Create context and bind D3D11 device
    res = factory_->CreateContext(&context_);
    if (res != AMF_OK) {
        MELLO_LOG_DEBUG(TAG, "Probing AMF... CreateContext failed: %d", res);
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    res = context_->InitDX11(device_.Get());
    if (res != AMF_OK) {
        MELLO_LOG_DEBUG(TAG, "Probing AMF... InitDX11 failed: %d", res);
        context_.Release(); FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    // Create encoder component
    const wchar_t* codec_id = (codec_ == VideoCodec::AV1)
        ? AMFVideoEncoder_AV1
        : AMFVideoEncoderVCE_AVC;

    res = factory_->CreateComponent(context_, codec_id, &encoder_);
    if (res != AMF_OK) {
        MELLO_LOG_DEBUG(TAG, "Probing AMF... CreateComponent failed: %d", res);
        context_->Terminate(); context_.Release();
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    // Configure for ultra-low latency
    if (codec_ == VideoCodec::H264) {
        auto set_required = [&](const wchar_t* property, const auto& value, const char* label) {
            const AMF_RESULT property_res = encoder_->SetProperty(property, value);
            if (property_res != AMF_OK) {
                MELLO_LOG_ERROR(TAG, "AMF: required property %s rejected: %d", label, property_res);
                return false;
            }
            return true;
        };

        const uint32_t fps = config.fps > 0 ? config.fps : 60;
        const amf_int64 bitrate_bps = static_cast<amf_int64>(config.bitrate_kbps * 1000);
        const amf_int64 vbv_bits = std::max<amf_int64>(1, bitrate_bps / static_cast<amf_int64>(fps));

        if (!set_required(AMF_VIDEO_ENCODER_USAGE,
                          static_cast<amf_int64>(AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY),
                          "usage=ultra-low-latency") ||
            !set_required(AMF_VIDEO_ENCODER_LOWLATENCY_MODE, true, "low-latency-mode") ||
            !set_required(AMF_VIDEO_ENCODER_PREENCODE_ENABLE,
                          static_cast<amf_int64>(AMF_VIDEO_ENCODER_PREENCODE_DISABLED),
                          "preencode=disabled") ||
            !set_required(AMF_VIDEO_ENCODER_PRE_ANALYSIS_ENABLE, false, "pre-analysis=disabled") ||
            !set_required(AMF_VIDEO_ENCODER_PROFILE,
                          static_cast<amf_int64>(AMF_VIDEO_ENCODER_PROFILE_MAIN),
                          "profile=Main") ||
            !set_required(AMF_VIDEO_ENCODER_PROFILE_LEVEL,
                          static_cast<amf_int64>(AMF_H264_LEVEL__4_2),
                          "level=4.2") ||
            !set_required(AMF_VIDEO_ENCODER_B_PIC_PATTERN,
                          static_cast<amf_int64>(0),
                          "B-frames=disabled") ||
            !set_required(AMF_VIDEO_ENCODER_B_REFERENCE_ENABLE, false, "B-ref=disabled") ||
            !set_required(AMF_VIDEO_ENCODER_IDR_PERIOD,
                          static_cast<amf_int64>(config.keyframe_interval),
                          "IDR period") ||
            !set_required(AMF_VIDEO_ENCODER_HEADER_INSERTION_SPACING,
                          static_cast<amf_int64>(config.keyframe_interval),
                          "SPS/PPS insertion spacing") ||
            !set_required(AMF_VIDEO_ENCODER_TARGET_BITRATE, bitrate_bps, "target-bitrate") ||
            !set_required(AMF_VIDEO_ENCODER_PEAK_BITRATE, bitrate_bps, "peak-bitrate") ||
            !set_required(AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD,
                          static_cast<amf_int64>(AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR),
                          "rate-control=CBR") ||
            !set_required(AMF_VIDEO_ENCODER_FRAMERATE, AMFConstructRate(config.fps, 1), "frame-rate") ||
            !set_required(AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE, vbv_bits, "VBV=1-frame")) {
            encoder_.Release(); context_->Terminate(); context_.Release();
            factory_ = nullptr;
            FreeLibrary(dll_); dll_ = nullptr;
            return false;
        }
    }

    res = encoder_->Init(amf::AMF_SURFACE_NV12, config.width, config.height);
    if (res != AMF_OK) {
        MELLO_LOG_ERROR(TAG, "AMF: encoder Init failed: %d", res);
        encoder_.Release(); context_->Terminate(); context_.Release();
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    if (codec_ == VideoCodec::H264) {
        amf_int64 actual_profile = AMF_VIDEO_ENCODER_PROFILE_UNKNOWN;
        amf_int64 actual_level = 0;
        const AMF_RESULT profile_res =
            encoder_->GetProperty(AMF_VIDEO_ENCODER_PROFILE, &actual_profile);
        const AMF_RESULT level_res =
            encoder_->GetProperty(AMF_VIDEO_ENCODER_PROFILE_LEVEL, &actual_level);
        if (profile_res != AMF_OK || level_res != AMF_OK ||
            actual_profile != AMF_VIDEO_ENCODER_PROFILE_MAIN ||
            actual_level != AMF_H264_LEVEL__4_2) {
            MELLO_LOG_ERROR(TAG,
                "AMF: effective H264 profile/level mismatch (profile status=%d value=%lld, level status=%d value=%lld; required Main/4.2)",
                profile_res, static_cast<long long>(actual_profile),
                level_res, static_cast<long long>(actual_level));
            encoder_->Terminate(); encoder_.Release();
            context_->Terminate(); context_.Release();
            factory_ = nullptr;
            FreeLibrary(dll_); dll_ = nullptr;
            return false;
        }
        MELLO_LOG_INFO(TAG,
            "AMF: effective H264 profile=Main level=4.2 usage=ULL low-latency=on preencode=off vbv=%lld B-frames=disabled",
            static_cast<long long>(std::max<amf_int64>(
                1, static_cast<amf_int64>(config.bitrate_kbps * 1000) /
                   static_cast<amf_int64>(config.fps > 0 ? config.fps : 60))));
    }

    MELLO_LOG_DEBUG(TAG, "Probing AMF... ok");
    MELLO_LOG_INFO(TAG, "Selected encoder: AMF codec=%s resolution=%ux%u fps=%u bitrate=%ukbps",
        config.codec == VideoCodec::H264 ? "H264" : "AV1",
        config.width, config.height, config.fps, config.bitrate_kbps);
    return true;
}

void AmfEncoder::shutdown() {
    if (encoder_) { encoder_->Terminate(); encoder_.Release(); }
    if (context_) { context_->Terminate(); context_.Release(); }
    factory_ = nullptr;
    if (dll_) { FreeLibrary(dll_); dll_ = nullptr; }
}

bool AmfEncoder::encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) {
    if (!encoder_) return false;

    // Wrap the D3D11 NV12 texture as an AMF surface (zero-copy)
    amf::AMFSurfacePtr surface;
    AMF_RESULT res = context_->CreateSurfaceFromDX11Native(nv12_texture, &surface, nullptr);
    if (res != AMF_OK || !surface) {
        MELLO_LOG_ERROR(TAG, "AMF: CreateSurfaceFromDX11Native failed: %d (seq=%llu)", res, frame_seq_);
        return false;
    }

    if (force_idr_) {
        if (codec_ == VideoCodec::H264) {
            const AMF_RESULT force_res = surface->SetProperty(
                AMF_VIDEO_ENCODER_FORCE_PICTURE_TYPE,
                static_cast<amf_int64>(AMF_VIDEO_ENCODER_PICTURE_TYPE_IDR));
            const AMF_RESULT sps_res =
                surface->SetProperty(AMF_VIDEO_ENCODER_INSERT_SPS, true);
            const AMF_RESULT pps_res =
                surface->SetProperty(AMF_VIDEO_ENCODER_INSERT_PPS, true);
            if (force_res != AMF_OK || sps_res != AMF_OK || pps_res != AMF_OK) {
                MELLO_LOG_ERROR(TAG,
                    "AMF: forced IDR/SPS/PPS properties rejected (IDR=%d SPS=%d PPS=%d seq=%llu)",
                    force_res, sps_res, pps_res, frame_seq_);
                return false;
            }
        }
        force_idr_ = false;
    }

    // Submit input
    res = encoder_->SubmitInput(surface);
    if (res != AMF_OK && res != AMF_INPUT_FULL) {
        MELLO_LOG_ERROR(TAG, "AMF: SubmitInput failed: %d (seq=%llu)", res, frame_seq_);
        return false;
    }

    // Query output
    amf::AMFDataPtr data;
    res = encoder_->QueryOutput(&data);
    if (res != AMF_OK || !data) {
        return false; // No output yet (async pipeline)
    }

    amf::AMFBufferPtr buffer(data);
    if (!buffer) return false;

    out.data.assign(
        static_cast<const uint8_t*>(buffer->GetNative()),
        static_cast<const uint8_t*>(buffer->GetNative()) + buffer->GetSize());
    out.timestamp_us = static_cast<uint64_t>(
        std::chrono::duration_cast<std::chrono::microseconds>(
            std::chrono::steady_clock::now().time_since_epoch()).count());

    // Check if keyframe via output property
    amf_int64 pic_type = 0;
    if (codec_ == VideoCodec::H264) {
        data->GetProperty(AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE, &pic_type);
        out.is_keyframe = (pic_type == AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_IDR);
    } else {
        out.is_keyframe = false; // AV1 check would go here
    }

    frame_seq_++;
    stats_.bytes_sent += out.data.size();
    stats_.fps_actual = config_.fps;
    stats_.bitrate_kbps = config_.bitrate_kbps;
    if (out.is_keyframe) {
        stats_.keyframes_sent++;
        MELLO_LOG_DEBUG(TAG, "Keyframe encoded (AMF seq=%llu)", frame_seq_);
    }

    return true;
}

void AmfEncoder::request_keyframe() {
    force_idr_ = true;
    MELLO_LOG_DEBUG(TAG, "Keyframe requested (AMF seq=%llu)", frame_seq_);
}

void AmfEncoder::set_bitrate(uint32_t kbps) {
    if (encoder_ && codec_ == VideoCodec::H264) {
        const uint32_t fps = config_.fps > 0 ? config_.fps : 60;
        const amf_int64 bitrate_bps = static_cast<amf_int64>(kbps * 1000);
        const amf_int64 vbv_bits = std::max<amf_int64>(1, bitrate_bps / static_cast<amf_int64>(fps));
        const AMF_RESULT target_res =
            encoder_->SetProperty(AMF_VIDEO_ENCODER_TARGET_BITRATE, bitrate_bps);
        const AMF_RESULT peak_res =
            encoder_->SetProperty(AMF_VIDEO_ENCODER_PEAK_BITRATE, bitrate_bps);
        const AMF_RESULT vbv_res =
            encoder_->SetProperty(AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE, vbv_bits);
        if (target_res != AMF_OK || peak_res != AMF_OK || vbv_res != AMF_OK) {
            MELLO_LOG_ERROR(TAG,
                "AMF: bitrate reconfigure rejected (target=%d peak=%d vbv=%d kbps=%u)",
                target_res, peak_res, vbv_res, kbps);
        } else {
            force_idr_ = true;
            MELLO_LOG_DEBUG(TAG, "AMF: reconfigured bitrate=%ukbps vbv=%lld forceIDR=1",
                kbps, static_cast<long long>(vbv_bits));
        }
    }
    config_.bitrate_kbps = kbps;
}

void AmfEncoder::get_stats(EncoderStats& out) const {
    out = stats_;
}

bool AmfEncoder::supports_codec(VideoCodec codec) const {
    return codec == VideoCodec::H264 || codec == VideoCodec::AV1;
}

} // namespace mello::video
#endif
