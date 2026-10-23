#ifdef _WIN32
#include "encoder_amf.hpp"
#include "../util/log.hpp"
#include <Windows.h>
#include <algorithm>
#include <chrono>
#include <thread>

#include <AMF/core/Data.h>
#include <AMF/core/Surface.h>
typedef AMF_RESULT(AMF_CDECL_CALL* AMFInit_Fn)(amf_uint64, amf::AMFFactory**);

namespace mello::video {

static constexpr const char* TAG = "video/encoder";
// The longest the encode thread waits for the output of one frame.
static constexpr std::chrono::milliseconds kQueryOutputBudget{50};

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
    cost_tier_ = 0;
    cost_tier_unavailable_ = false;

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
        const amf_int64 vbv_bits = static_cast<amf_int64>(vbv_bits_for(config.bitrate_kbps, fps));

        if (!set_required(AMF_VIDEO_ENCODER_USAGE,
                          static_cast<amf_int64>(AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY),
                          "usage=ultra-low-latency") ||
            !set_required(AMF_VIDEO_ENCODER_LOWLATENCY_MODE, true, "low-latency-mode") ||
            !set_required(AMF_VIDEO_ENCODER_PREENCODE_ENABLE,
                          static_cast<amf_int64>(AMF_VIDEO_ENCODER_PREENCODE_DISABLED),
                          "preencode=disabled") ||
            !set_required(AMF_VIDEO_ENCODER_PRE_ANALYSIS_ENABLE, false, "pre-analysis=disabled") ||
            !set_required(AMF_VIDEO_ENCODER_PROFILE,
                          static_cast<amf_int64>(AMF_VIDEO_ENCODER_PROFILE_HIGH),
                          "profile=High") ||
            !set_required(AMF_VIDEO_ENCODER_PROFILE_LEVEL,
                          static_cast<amf_int64>(AMF_H264_LEVEL__5_1),
                          "level=5.1") ||
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
            !set_required(AMF_VIDEO_ENCODER_PEAK_BITRATE,
                          bitrate_bps + bitrate_bps / 4, "peak-bitrate") ||
            !set_required(AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD,
                          static_cast<amf_int64>(AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_PEAK_CONSTRAINED_VBR),
                          "rate-control=VBR-peak-constrained") ||
            !set_required(AMF_VIDEO_ENCODER_FRAMERATE, AMFConstructRate(config.fps, 1), "frame-rate") ||
            !set_required(AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE, vbv_bits, "VBV=0.5s")) {
            encoder_.Release(); context_->Terminate(); context_.Release();
            factory_ = nullptr;
            FreeLibrary(dll_); dll_ = nullptr;
            return false;
        }
    }

    if (codec_ == VideoCodec::H264) {
        // Half-full VBV at the start, as NVENC (vbvInitialDelay = vbv / 2).
        // VUI: BT.709 limited range, the pipeline's conversion, so decoders
        // and recordings read the colours right. A driver without one of
        // these keeps its default; none of them is required.
        auto set_optional = [&](const wchar_t* property, const auto& value, const char* label) {
            const AMF_RESULT property_res = encoder_->SetProperty(property, value);
            if (property_res != AMF_OK) {
                MELLO_LOG_WARN(TAG, "AMF: optional property %s rejected: %d", label, property_res);
            }
        };
        set_optional(AMF_VIDEO_ENCODER_INITIAL_VBV_BUFFER_FULLNESS, static_cast<amf_int64>(32),
                     "initial VBV fullness 50 %");
        set_optional(AMF_VIDEO_ENCODER_OUTPUT_FULL_RANGE_COLOR, false, "VUI limited range");
        set_optional(AMF_VIDEO_ENCODER_OUTPUT_COLOR_PROFILE,
                     static_cast<amf_int64>(AMF_VIDEO_CONVERTER_COLOR_PROFILE_709), "VUI BT.709 matrix");
        set_optional(AMF_VIDEO_ENCODER_OUTPUT_TRANSFER_CHARACTERISTIC,
                     static_cast<amf_int64>(AMF_COLOR_TRANSFER_CHARACTERISTIC_BT709), "VUI BT.709 transfer");
        set_optional(AMF_VIDEO_ENCODER_OUTPUT_COLOR_PRIMARIES,
                     static_cast<amf_int64>(AMF_COLOR_PRIMARIES_BT709), "VUI BT.709 primaries");
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
            actual_profile != AMF_VIDEO_ENCODER_PROFILE_HIGH ||
            actual_level != AMF_H264_LEVEL__5_1) {
            MELLO_LOG_ERROR(TAG,
                "AMF: effective H264 profile/level mismatch (profile status=%d value=%lld, level status=%d value=%lld; required High/5.1)",
                profile_res, static_cast<long long>(actual_profile),
                level_res, static_cast<long long>(actual_level));
            encoder_->Terminate(); encoder_.Release();
            context_->Terminate(); context_.Release();
            factory_ = nullptr;
            FreeLibrary(dll_); dll_ = nullptr;
            return false;
        }
        // What the usage chose for the features the cost tiers give up, so a
        // field log shows tier 0.
        amf_int64 preset = -1;
        bool vbaq = false, boost = false;
        encoder_->GetProperty(AMF_VIDEO_ENCODER_QUALITY_PRESET, &preset);
        encoder_->GetProperty(AMF_VIDEO_ENCODER_ENABLE_VBAQ, &vbaq);
        encoder_->GetProperty(AMF_VIDEO_ENCODER_HIGH_MOTION_QUALITY_BOOST_ENABLE, &boost);
        MELLO_LOG_INFO(TAG, "AMF: tier 0 quality preset=%lld vbaq=%d high-motion-boost=%d",
                       static_cast<long long>(preset), vbaq ? 1 : 0, boost ? 1 : 0);
        MELLO_LOG_INFO(TAG,
            "AMF: effective H264 profile=High level=5.1 usage=ULL low-latency=on preencode=off vbv=%lld gop=%u B-frames=disabled",
            static_cast<long long>(vbv_bits_for(config.bitrate_kbps, config.fps)),
            config.keyframe_interval);
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

    // Query output. AMF is asynchronous even in ultra-low-latency usage: the
    // frame just submitted can come out a little later. One QueryOutput call
    // returned nothing for it and handed it out with the next frame (one
    // frame of lag, and the wrong frame for the timestamp). Poll until it
    // comes, at most kQueryOutputBudget.
    amf::AMFDataPtr data;
    const auto deadline = std::chrono::steady_clock::now() + kQueryOutputBudget;
    for (;;) {
        res = encoder_->QueryOutput(&data);
        if (res == AMF_OK && data) break;
        if (res != AMF_OK && res != AMF_REPEAT && res != AMF_NEED_MORE_INPUT) {
            MELLO_LOG_ERROR(TAG, "AMF: QueryOutput failed: %d (seq=%llu)", res, frame_seq_);
            return false;
        }
        if (std::chrono::steady_clock::now() >= deadline) {
            MELLO_LOG_WARN(TAG, "AMF: no output %lld ms after submit (seq=%llu)",
                           static_cast<long long>(kQueryOutputBudget.count()), frame_seq_);
            return false;
        }
        std::this_thread::sleep_for(std::chrono::microseconds(500));
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

AmfEncoder::TierSettings AmfEncoder::tier_settings(int tier) {
    TierSettings t{};
    if (tier >= 1) t.quality_preset = AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED;
    if (tier >= 2) t.adaptive_off = true;
    return t;
}

// Runs on the encode thread, through EncoderControl. Each step gives up the
// most expensive feature left, as NVENC's tiers do. A step that changes
// nothing (the driver's usage default was already that) goes on to the next.
// The properties are set and the encoder re-initialised (ReInit), because
// some of them are static on some drivers; the next frame is an IDR.
bool AmfEncoder::reduce_cost_tier() {
    if (!encoder_ || codec_ != VideoCodec::H264 || cost_tier_ >= kMaxCostTier ||
        cost_tier_unavailable_) {
        return false;
    }
    const int previous = cost_tier_;
    for (int next = cost_tier_ + 1; next <= kMaxCostTier; ++next) {
        const TierSettings t = tier_settings(next);
        bool changed = false;
        bool ok = true;
        auto set_if_different = [&](const wchar_t* property, amf_int64 value) {
            amf_int64 current = -1;
            if (encoder_->GetProperty(property, &current) == AMF_OK && current == value) return;
            ok = ok && encoder_->SetProperty(property, value) == AMF_OK;
            changed = true;
        };
        auto turn_off = [&](const wchar_t* property) {
            bool current = true;
            if (encoder_->GetProperty(property, &current) == AMF_OK && !current) return;
            // Not every driver has every property: a refused "off" for a
            // feature the driver does not have is no failure.
            if (encoder_->SetProperty(property, false) == AMF_OK) changed = true;
        };
        if (t.quality_preset >= 0) {
            set_if_different(AMF_VIDEO_ENCODER_QUALITY_PRESET, t.quality_preset);
        }
        if (t.adaptive_off) {
            turn_off(AMF_VIDEO_ENCODER_ENABLE_VBAQ);
            turn_off(AMF_VIDEO_ENCODER_HIGH_MOTION_QUALITY_BOOST_ENABLE);
            turn_off(AMF_VIDEO_ENCODER_PRE_ANALYSIS_ENABLE);
        }
        if (!ok) {
            cost_tier_ = previous;
            cost_tier_unavailable_ = true;
            MELLO_LOG_ERROR(TAG, "AMF: cost tier %d->%d refused by the driver (quality downgrade "
                                 "unavailable for this session)", previous, next);
            return false;
        }
        cost_tier_ = next;
        if (!changed) continue;
        const AMF_RESULT res = encoder_->ReInit(config_.width, config_.height);
        if (res != AMF_OK) {
            cost_tier_unavailable_ = true;
            MELLO_LOG_ERROR(TAG, "AMF: ReInit for cost tier %d failed: %d (quality downgrade "
                                 "unavailable for this session)", next, res);
            return true;
        }
        force_idr_ = true;
        MELLO_LOG_WARN(TAG, "AMF: encoder cannot hold the frame budget: cost tier %d -> %d "
                            "(preset=%s adaptive=%s)", previous, next,
                       t.quality_preset == AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED ? "speed" : "init",
                       t.adaptive_off ? "off" : "init");
        return true;
    }
    MELLO_LOG_INFO(TAG, "AMF: cost tier %d -> %d changed nothing (the usage defaults were "
                        "already the cheaper settings)", previous, cost_tier_);
    return cost_tier_ != previous;
}

void AmfEncoder::request_keyframe() {
    force_idr_ = true;
    MELLO_LOG_DEBUG(TAG, "Keyframe requested (AMF seq=%llu)", frame_seq_);
}

void AmfEncoder::set_bitrate(uint32_t kbps) {
    if (encoder_ && codec_ == VideoCodec::H264) {
        const uint32_t fps = config_.fps > 0 ? config_.fps : 60;
        const amf_int64 bitrate_bps = static_cast<amf_int64>(kbps * 1000);
        const amf_int64 vbv_bits = static_cast<amf_int64>(vbv_bits_for(kbps, fps));
        const AMF_RESULT target_res =
            encoder_->SetProperty(AMF_VIDEO_ENCODER_TARGET_BITRATE, bitrate_bps);
        const AMF_RESULT peak_res =
            encoder_->SetProperty(AMF_VIDEO_ENCODER_PEAK_BITRATE,
                                  bitrate_bps + bitrate_bps / 4);
        const AMF_RESULT vbv_res =
            encoder_->SetProperty(AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE, vbv_bits);
        if (target_res != AMF_OK || peak_res != AMF_OK || vbv_res != AMF_OK) {
            MELLO_LOG_ERROR(TAG,
                "AMF: bitrate reconfigure rejected (target=%d peak=%d vbv=%d kbps=%u)",
                target_res, peak_res, vbv_res, kbps);
        } else {
            // IDR only on a large down-step, as NVENC does. Every step was an
            // IDR before, and the host steps the bitrate many times a second.
            const bool idr = bitrate_step_forces_idr(config_.bitrate_kbps, kbps);
            if (idr) force_idr_ = true;
            MELLO_LOG_DEBUG(TAG, "AMF: reconfigured bitrate=%ukbps vbv=%lld forceIDR=%d",
                kbps, static_cast<long long>(vbv_bits), idr ? 1 : 0);
        }
    }
    config_.bitrate_kbps = kbps;
}

// A geometry rung (stage 4): AMF changes the size with ReInit, as the cost
// tiers do. The next frame is an IDR with SPS and PPS.
bool AmfEncoder::reconfigure_size(uint32_t width, uint32_t height) {
    if (!encoder_ || width == 0 || height == 0) return false;
    const AMF_RESULT res = encoder_->ReInit(static_cast<amf_int32>(width),
                                            static_cast<amf_int32>(height));
    if (res != AMF_OK) {
        MELLO_LOG_WARN(TAG, "AMF: size %ux%u -> %ux%u ReInit refused: %d (re-initialize)",
                       config_.width, config_.height, width, height, res);
        return false;
    }
    MELLO_LOG_INFO(TAG, "AMF: size %ux%u -> %ux%u with ReInit", config_.width, config_.height,
                   width, height);
    config_.width = width;
    config_.height = height;
    force_idr_ = true;
    return true;
}

void AmfEncoder::set_framerate(uint32_t fps) {
    if (fps == 0 || fps == config_.fps) return;
    config_.fps = fps;
    if (!encoder_ || codec_ != VideoCodec::H264) return;
    const AMF_RESULT rate_res = encoder_->SetProperty(AMF_VIDEO_ENCODER_FRAMERATE, AMFConstructRate(fps, 1));
    const AMF_RESULT vbv_res = encoder_->SetProperty(
        AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE, static_cast<amf_int64>(vbv_bits_for(config_.bitrate_kbps, fps)));
    if (rate_res != AMF_OK || vbv_res != AMF_OK) {
        MELLO_LOG_ERROR(TAG, "AMF: framerate %u reconfigure rejected (rate=%d vbv=%d)", fps,
                        rate_res, vbv_res);
        return;
    }
    force_idr_ = true;
    MELLO_LOG_INFO(TAG, "AMF: framerate %u fps", fps);
}

void AmfEncoder::get_stats(EncoderStats& out) const {
    out = stats_;
}

bool AmfEncoder::supports_codec(VideoCodec codec) const {
    return codec == VideoCodec::H264 || codec == VideoCodec::AV1;
}

} // namespace mello::video
#endif
