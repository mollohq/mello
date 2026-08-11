#ifdef _WIN32
#include "encoder_nvenc.hpp"
#include "../util/log.hpp"
#include <Windows.h>
#include <chrono>

namespace mello::video {

static constexpr const char* TAG = "video/encoder";

// VBV spans ~0.5s of the max rate, floored at 4 frames of bits. A one-frame
// VBV (~17 KB at 8 Mbps/60 fps) starves IDRs and high-motion frames into
// visible quality pumping; 0.5s keeps rate control tight for interactive
// latency while giving keyframes room.
static uint32_t compute_vbv_bits(uint32_t avg_bps, uint32_t max_bps, uint32_t fps) {
    uint32_t frame_bits = fps > 0 ? avg_bps / fps : 1u;
    if (frame_bits == 0) frame_bits = 1;
    uint32_t vbv = max_bps / 2;
    if (vbv < frame_bits * 4) vbv = frame_bits * 4;
    return vbv;
}

typedef NVENCSTATUS(NVENCAPI* PFN_NvEncodeAPIGetMaxSupportedVersion)(uint32_t*);
typedef NVENCSTATUS(NVENCAPI* PFN_NvEncodeAPICreateInstance)(NV_ENCODE_API_FUNCTION_LIST*);

static HMODULE load_nvenc_dll() {
    HMODULE dll = LoadLibraryA("nvEncodeAPI64.dll");
    if (!dll) dll = LoadLibraryA("nvEncodeAPI.dll");
    return dll;
}

bool NvencEncoder::is_available() {
    HMODULE dll = load_nvenc_dll();
    if (dll) {
        FreeLibrary(dll);
        return true;
    }
    return false;
}

bool NvencEncoder::initialize(const GraphicsDevice& device, const EncoderConfig& config) {
    device_ = device.d3d11();
    config_ = config;
    stats_ = {};
    frame_seq_ = 0;

    dll_ = load_nvenc_dll();
    if (!dll_) {
        MELLO_LOG_DEBUG(TAG, "Probing NVENC... not available (nvEncodeAPI64.dll not found)");
        return false;
    }

    auto pfn_create = reinterpret_cast<PFN_NvEncodeAPICreateInstance>(
        GetProcAddress(dll_, "NvEncodeAPICreateInstance"));
    auto pfn_max_ver = reinterpret_cast<PFN_NvEncodeAPIGetMaxSupportedVersion>(
        GetProcAddress(dll_, "NvEncodeAPIGetMaxSupportedVersion"));
    if (!pfn_create) {
        MELLO_LOG_WARN(TAG, "NVENC: NvEncodeAPICreateInstance entry point not found");
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    if (pfn_max_ver) {
        uint32_t driver_packed = 0;
        pfn_max_ver(&driver_packed);
        uint32_t drv_major = driver_packed >> 4;
        uint32_t drv_minor = driver_packed & 0xF;
        MELLO_LOG_INFO(TAG, "NVENC: SDK header v%d.%d, driver supports up to v%u.%u",
            NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION, drv_major, drv_minor);
    }

    MELLO_LOG_INFO(TAG, "NVENC: NVENCAPI_VERSION=0x%08X FnListVer=0x%08X SessionVer=0x%08X",
        (uint32_t)NVENCAPI_VERSION, (uint32_t)NV_ENCODE_API_FUNCTION_LIST_VER,
        (uint32_t)NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER);
    MELLO_LOG_INFO(TAG, "NVENC: ConfigVer=0x%08X InitVer=0x%08X PresetCfgVer=0x%08X RcVer=0x%08X",
        (uint32_t)NV_ENC_CONFIG_VER, (uint32_t)NV_ENC_INITIALIZE_PARAMS_VER,
        (uint32_t)NV_ENC_PRESET_CONFIG_VER, (uint32_t)NV_ENC_RC_PARAMS_VER);

    fn_ = {NV_ENCODE_API_FUNCTION_LIST_VER};
    NVENCSTATUS status = pfn_create(&fn_);
    if (status != NV_ENC_SUCCESS) {
        MELLO_LOG_WARN(TAG, "NVENC: NvEncodeAPICreateInstance failed: %d", status);
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS session_params = {NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER};
    session_params.device     = device_.Get();
    session_params.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
    session_params.apiVersion = NVENCAPI_VERSION;

    status = fn_.nvEncOpenEncodeSessionEx(&session_params, &encoder_);
    if (status != NV_ENC_SUCCESS) {
        MELLO_LOG_WARN(TAG, "NVENC: nvEncOpenEncodeSessionEx failed: %d (apiVersion=0x%08X)",
            status, (uint32_t)NVENCAPI_VERSION);
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }
    MELLO_LOG_INFO(TAG, "NVENC: session opened OK (handle=%p)", encoder_);

    // Verify session with simplest possible call
    uint32_t guid_count = 0;
    status = fn_.nvEncGetEncodeGUIDCount(encoder_, &guid_count);
    MELLO_LOG_INFO(TAG, "NVENC: nvEncGetEncodeGUIDCount => status=%d count=%u", status, guid_count);

    if (status == NV_ENC_SUCCESS && guid_count > 0) {
        std::vector<GUID> guids(guid_count);
        uint32_t actual = 0;
        fn_.nvEncGetEncodeGUIDs(encoder_, guids.data(), guid_count, &actual);
        for (uint32_t i = 0; i < actual; i++) {
            const char* name = "unknown";
            if (guids[i] == NV_ENC_CODEC_H264_GUID) name = "H264";
            else if (guids[i] == NV_ENC_CODEC_HEVC_GUID) name = "HEVC";
            else if (guids[i] == NV_ENC_CODEC_AV1_GUID) name = "AV1";
            MELLO_LOG_INFO(TAG, "NVENC:   codec[%u] = %s", i, name);
        }
    }

    GUID codec_guid = (config.codec == VideoCodec::AV1) ? NV_ENC_CODEC_AV1_GUID : NV_ENC_CODEC_H264_GUID;

    // Try preset config with multiple combos (diagnostic)
    NV_ENC_PRESET_CONFIG preset_config;
    memset(&preset_config, 0, sizeof(preset_config));
    preset_config.version = NV_ENC_PRESET_CONFIG_VER;
    preset_config.presetCfg.version = NV_ENC_CONFIG_VER;

    struct { GUID preset; NV_ENC_TUNING_INFO tuning; const char* label; } attempts[] = {
        { NV_ENC_PRESET_P4_GUID, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, "P4+ULL" },
        { NV_ENC_PRESET_P1_GUID, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, "P1+ULL" },
        { NV_ENC_PRESET_P1_GUID, NV_ENC_TUNING_INFO_LOW_LATENCY,       "P1+LL"  },
    };

    bool have_preset = false;
    GUID used_preset = NV_ENC_PRESET_P4_GUID;
    NV_ENC_TUNING_INFO used_tuning = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    const char* used_preset_label = "P4+ULL";
    for (auto& a : attempts) {
        memset(&preset_config, 0, sizeof(preset_config));
        preset_config.version = NV_ENC_PRESET_CONFIG_VER;
        preset_config.presetCfg.version = NV_ENC_CONFIG_VER;
        status = fn_.nvEncGetEncodePresetConfigEx(encoder_, codec_guid, a.preset, a.tuning, &preset_config);
        MELLO_LOG_INFO(TAG, "NVENC: PresetConfigEx(%s) => %d", a.label, status);
        if (status == NV_ENC_SUCCESS) {
            have_preset = true;
            used_preset = a.preset;
            used_tuning = a.tuning;
            used_preset_label = a.label;
            break;
        }
    }

    NV_ENC_CONFIG enc_config;
    if (have_preset) {
        enc_config = preset_config.presetCfg;
    } else {
        MELLO_LOG_WARN(TAG, "NVENC: all preset queries failed — building config from scratch");
        memset(&enc_config, 0, sizeof(enc_config));
    }
    enc_config.version = NV_ENC_CONFIG_VER;
    enc_config.rcParams.version = NV_ENC_RC_PARAMS_VER;

    // VBR with moderate headroom: 1.25x max lets keyframes get extra bits
    // without large bandwidth spikes. VBV spans ~0.5s of the max rate (see
    // compute_vbv_bits) so IDRs are not rate-starved.
    const uint32_t fps = config.fps > 0 ? config.fps : 60;
    const uint32_t avg = config.bitrate_kbps * 1000;
    const uint32_t max = avg + avg / 4;
    const uint32_t vbv = compute_vbv_bits(avg, max, fps);
    enc_config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_VBR;
    enc_config.rcParams.averageBitRate  = avg;
    enc_config.rcParams.maxBitRate      = max;
    enc_config.rcParams.vbvBufferSize   = vbv;
    enc_config.rcParams.vbvInitialDelay = vbv / 2;
    enc_config.rcParams.enableLookahead   = 0;
    enc_config.rcParams.enableExtLookahead = 0;
    enc_config.rcParams.lookaheadDepth  = 0;
    enc_config.rcParams.enableTemporalAQ = 1;
    enc_config.rcParams.enableAQ        = 1;
    enc_config.rcParams.aqStrength      = 8;
    enc_config.rcParams.zeroReorderDelay = 1;
    enc_config.frameIntervalP = 1;
    enc_config.gopLength      = config.keyframe_interval;

    if (config.codec == VideoCodec::H264) {
        enc_config.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
        enc_config.encodeCodecConfig.h264Config.idrPeriod         = config.keyframe_interval;
        enc_config.encodeCodecConfig.h264Config.level             = NV_ENC_LEVEL_H264_42;
        enc_config.encodeCodecConfig.h264Config.enableIntraRefresh = 0;
        enc_config.encodeCodecConfig.h264Config.repeatSPSPPS      = 1;
        // Signal BT.709 limited-range in the VUI: the conversion pipeline is
        // BT.709 studio-range, and signaling it makes external decoders and
        // recordings interpret colors correctly.
        enc_config.encodeCodecConfig.h264Config.h264VUIParameters.videoSignalTypePresentFlag = 1;
        enc_config.encodeCodecConfig.h264Config.h264VUIParameters.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        enc_config.encodeCodecConfig.h264Config.h264VUIParameters.videoFullRangeFlag = 0; // limited (studio) range
        enc_config.encodeCodecConfig.h264Config.h264VUIParameters.colourDescriptionPresentFlag = 1;
        enc_config.encodeCodecConfig.h264Config.h264VUIParameters.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        enc_config.encodeCodecConfig.h264Config.h264VUIParameters.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
        enc_config.encodeCodecConfig.h264Config.h264VUIParameters.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT709;
        // Full two-pass (multiPass lives in rcParams in this SDK; the full-res
        // variant is NV_ENC_TWO_PASS_FULL_RESOLUTION — NV_ENC_MULTI_PASS_FULL
        // does not exist). Improves quality at the same bitrate.
        enc_config.rcParams.multiPass = NV_ENC_TWO_PASS_FULL_RESOLUTION;
    }

    NV_ENC_INITIALIZE_PARAMS init_params;
    memset(&init_params, 0, sizeof(init_params));
    init_params.version            = NV_ENC_INITIALIZE_PARAMS_VER;
    init_params.encodeGUID         = codec_guid;
    init_params.presetGUID         = used_preset;
    init_params.encodeWidth        = config.width;
    init_params.encodeHeight       = config.height;
    init_params.darWidth           = config.width;
    init_params.darHeight          = config.height;
    init_params.frameRateNum       = config.fps;
    init_params.frameRateDen       = 1;
    init_params.enablePTD          = 1;
    init_params.enableEncodeAsync  = 1;
    init_params.encodeConfig       = &enc_config;
    init_params.tuningInfo         = used_tuning;

    MELLO_LOG_INFO(TAG, "NVENC: nvEncInitializeEncoder %ux%u async=%d (initVer=0x%08X cfgVer=0x%08X rcVer=0x%08X)",
        config.width, config.height, init_params.enableEncodeAsync,
        init_params.version, enc_config.version, enc_config.rcParams.version);

    status = fn_.nvEncInitializeEncoder(encoder_, &init_params);

    // Some drivers/configs don't support async — fall back to sync mode
    if (status != NV_ENC_SUCCESS && init_params.enableEncodeAsync) {
        MELLO_LOG_INFO(TAG, "NVENC: async init failed (%d), falling back to sync mode", status);
        init_params.enableEncodeAsync = 0;
        status = fn_.nvEncInitializeEncoder(encoder_, &init_params);
    }

    if (status != NV_ENC_SUCCESS) {
        MELLO_LOG_WARN(TAG, "NVENC: nvEncInitializeEncoder failed: %d — retrying with LOW_LATENCY tuning", status);
        init_params.tuningInfo = NV_ENC_TUNING_INFO_LOW_LATENCY;
        status = fn_.nvEncInitializeEncoder(encoder_, &init_params);
    }
    if (status != NV_ENC_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVENC: nvEncInitializeEncoder final failure: %d", status);
        fn_.nvEncDestroyEncoder(encoder_); encoder_ = nullptr;
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }
    base_config_ = enc_config;
    MELLO_LOG_INFO(TAG,
        "NVENC: effective preset=%s RC=VBR avg=%u max=%u vbv=%u spatialAQ=8 temporalAQ=1 multipass=full profile=High B-frames=disabled lookahead=disabled",
        used_preset_label, avg, max, vbv);
    if (config.codec == VideoCodec::H264) {
        MELLO_LOG_INFO(TAG, "NVENC: initialized H264 profile=High level=4.2 repeatSPSPPS=enabled");
    }

    NV_ENC_CREATE_BITSTREAM_BUFFER bstream = {NV_ENC_CREATE_BITSTREAM_BUFFER_VER};
    status = fn_.nvEncCreateBitstreamBuffer(encoder_, &bstream);
    if (status != NV_ENC_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "nvEncCreateBitstreamBuffer failed: %d", status);
        fn_.nvEncDestroyEncoder(encoder_); encoder_ = nullptr;
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }
    out_buf_ = bstream.bitstreamBuffer;

    // Register completion event if encoder was initialized in async mode.
    if (init_params.enableEncodeAsync) {
        completion_event_ = CreateEventA(nullptr, FALSE, FALSE, nullptr);
        if (completion_event_) {
            NV_ENC_EVENT_PARAMS event_params = {NV_ENC_EVENT_PARAMS_VER};
            event_params.completionEvent = completion_event_;
            status = fn_.nvEncRegisterAsyncEvent(encoder_, &event_params);
            if (status == NV_ENC_SUCCESS) {
                async_mode_ = true;
                MELLO_LOG_INFO(TAG, "NVENC: async encode enabled (completion event registered)");
            } else {
                MELLO_LOG_WARN(TAG, "NVENC: async event registration failed (%d), using sync fallback", status);
                CloseHandle(completion_event_);
                completion_event_ = nullptr;
            }
        }
    }
    if (!async_mode_) {
        MELLO_LOG_INFO(TAG, "NVENC: using synchronous encode mode");
    }

    MELLO_LOG_INFO(TAG, "Selected encoder: NVENC codec=%s resolution=%ux%u fps=%u bitrate=%ukbps",
        config.codec == VideoCodec::H264 ? "H264" : "AV1",
        config.width, config.height, config.fps, config.bitrate_kbps);

    return true;
}

void NvencEncoder::shutdown() {
    if (encoder_) {
        for (auto& [tex, reg] : reg_cache_) {
            fn_.nvEncUnregisterResource(encoder_, reg);
        }
        reg_cache_.clear();
        if (completion_event_) {
            NV_ENC_EVENT_PARAMS event_params = {NV_ENC_EVENT_PARAMS_VER};
            event_params.completionEvent = completion_event_;
            fn_.nvEncUnregisterAsyncEvent(encoder_, &event_params);
            CloseHandle(completion_event_);
            completion_event_ = nullptr;
            async_mode_ = false;
        }
        if (out_buf_) {
            fn_.nvEncDestroyBitstreamBuffer(encoder_, out_buf_);
            out_buf_ = nullptr;
        }
        fn_.nvEncDestroyEncoder(encoder_);
        encoder_ = nullptr;
    }
    if (dll_) {
        FreeLibrary(dll_);
        dll_ = nullptr;
    }
}

NV_ENC_REGISTERED_PTR NvencEncoder::get_or_register(ID3D11Texture2D* tex) {
    auto it = reg_cache_.find(tex);
    if (it != reg_cache_.end()) return it->second;

    NV_ENC_REGISTER_RESOURCE reg = {NV_ENC_REGISTER_RESOURCE_VER};
    reg.resourceType          = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
    reg.resourceToRegister    = tex;
    reg.width                 = config_.width;
    reg.height                = config_.height;
    reg.bufferFormat          = NV_ENC_BUFFER_FORMAT_NV12;
    reg.bufferUsage           = NV_ENC_INPUT_IMAGE;

    NVENCSTATUS status = fn_.nvEncRegisterResource(encoder_, &reg);
    if (status != NV_ENC_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVENC: nvEncRegisterResource failed: %d (seq=%llu)", status, frame_seq_);
        return nullptr;
    }
    reg_cache_[tex] = reg.registeredResource;
    MELLO_LOG_DEBUG(TAG, "NVENC: registered texture %p (cache size=%zu)", tex, reg_cache_.size());
    return reg.registeredResource;
}

bool NvencEncoder::encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) {
    if (!encoder_) return false;

    NV_ENC_REGISTERED_PTR reg_res = get_or_register(nv12_texture);
    if (!reg_res) return false;

    NV_ENC_MAP_INPUT_RESOURCE map = {NV_ENC_MAP_INPUT_RESOURCE_VER};
    map.registeredResource = reg_res;

    NVENCSTATUS status = fn_.nvEncMapInputResource(encoder_, &map);
    if (status != NV_ENC_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVENC: nvEncMapInputResource failed: %d (seq=%llu)", status, frame_seq_);
        return false;
    }
    mapped_input_ = map.mappedResource;

    NV_ENC_PIC_PARAMS pic = {NV_ENC_PIC_PARAMS_VER};
    pic.inputBuffer       = mapped_input_;
    pic.bufferFmt         = NV_ENC_BUFFER_FORMAT_NV12;
    pic.inputWidth        = config_.width;
    pic.inputHeight       = config_.height;
    pic.outputBitstream   = out_buf_;
    pic.pictureStruct     = NV_ENC_PIC_STRUCT_FRAME;
    if (async_mode_) {
        pic.completionEvent = completion_event_;
    }

    if (force_idr_) {
        pic.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
        force_idr_ = false;
    }

    const auto t_submit_start = std::chrono::steady_clock::now();
    status = fn_.nvEncEncodePicture(encoder_, &pic);
    last_submit_ms_ = std::chrono::duration<double, std::milli>(
        std::chrono::steady_clock::now() - t_submit_start).count();
    // Not async: nothing waits, so the wait phase is zero rather than stale.
    last_wait_ms_ = 0.0;
    if (status != NV_ENC_SUCCESS && status != NV_ENC_ERR_NEED_MORE_INPUT) {
        MELLO_LOG_ERROR(TAG, "NVENC: nvEncEncodePicture failed: %d (seq=%llu)", status, frame_seq_);
        fn_.nvEncUnmapInputResource(encoder_, mapped_input_);
        mapped_input_ = nullptr;
        return false;
    }

    // In async mode, wait for the GPU to signal completion before locking.
    // This frees the CPU during the actual GPU encode work.
    if (async_mode_) {
        const auto t_wait_start = std::chrono::steady_clock::now();
        DWORD wait_result = WaitForSingleObject(completion_event_, 500);
        last_wait_ms_ = std::chrono::duration<double, std::milli>(
            std::chrono::steady_clock::now() - t_wait_start).count();
        if (wait_result != WAIT_OBJECT_0) {
            MELLO_LOG_ERROR(TAG, "NVENC: async completion event timeout (seq=%llu wait=%lu)", frame_seq_, wait_result);
            fn_.nvEncUnmapInputResource(encoder_, mapped_input_);
            mapped_input_ = nullptr;
            return false;
        }
    }

    NV_ENC_LOCK_BITSTREAM lock = {NV_ENC_LOCK_BITSTREAM_VER};
    lock.outputBitstream = out_buf_;

    const auto t_lock_start = std::chrono::steady_clock::now();
    status = fn_.nvEncLockBitstream(encoder_, &lock);
    last_lock_ms_ = std::chrono::duration<double, std::milli>(
        std::chrono::steady_clock::now() - t_lock_start).count();
    if (status != NV_ENC_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVENC: nvEncLockBitstream failed: %d (seq=%llu)", status, frame_seq_);
        fn_.nvEncUnmapInputResource(encoder_, mapped_input_);
        mapped_input_ = nullptr;
        return false;
    }

    out.data.assign(
        static_cast<const uint8_t*>(lock.bitstreamBufferPtr),
        static_cast<const uint8_t*>(lock.bitstreamBufferPtr) + lock.bitstreamSizeInBytes);
    out.is_keyframe   = (lock.pictureType == NV_ENC_PIC_TYPE_IDR);
    out.timestamp_us  = static_cast<uint64_t>(
        std::chrono::duration_cast<std::chrono::microseconds>(
            std::chrono::steady_clock::now().time_since_epoch()).count());

    fn_.nvEncUnlockBitstream(encoder_, out_buf_);
    fn_.nvEncUnmapInputResource(encoder_, mapped_input_);
    mapped_input_ = nullptr;

    frame_seq_++;
    stats_.bytes_sent += out.data.size();

    // Measured rolling stats: echoing the configured fps/bitrate here made
    // every downstream consumer (telemetry, certification) read config, not
    // reality.
    stats_window_frames_++;
    stats_window_bytes_ += out.data.size();
    const auto stats_now = std::chrono::steady_clock::now();
    if (stats_window_start_.time_since_epoch().count() == 0) {
        stats_window_start_ = stats_now;
    }
    const auto stats_elapsed_us =
        std::chrono::duration_cast<std::chrono::microseconds>(
            stats_now - stats_window_start_).count();
    if (stats_elapsed_us >= 1'000'000) {
        stats_.fps_actual = static_cast<uint32_t>(
            (static_cast<uint64_t>(stats_window_frames_) * 1'000'000)
            / static_cast<uint64_t>(stats_elapsed_us));
        stats_.bitrate_kbps = static_cast<uint32_t>(
            (stats_window_bytes_ * 8) / static_cast<uint64_t>(stats_elapsed_us / 1'000));
        stats_window_frames_ = 0;
        stats_window_bytes_ = 0;
        stats_window_start_ = stats_now;
    }
    if (out.is_keyframe) {
        stats_.keyframes_sent++;
        MELLO_LOG_DEBUG(TAG, "Keyframe encoded (reason=%s seq=%llu)",
            (frame_seq_ % config_.keyframe_interval == 0) ? "scheduled interval" : "requested",
            frame_seq_);
    }

    return true;
}

void NvencEncoder::request_keyframe() {
    force_idr_ = true;
    MELLO_LOG_DEBUG(TAG, "Keyframe requested (NVENC seq=%llu)", frame_seq_);
}

void NvencEncoder::set_bitrate(uint32_t kbps) {
    if (encoder_) {
        const uint32_t fps = config_.fps > 0 ? config_.fps : 60;
        const uint32_t avg = kbps * 1000;
        const uint32_t max = avg + avg / 4;
        const uint32_t vbv = compute_vbv_bits(avg, max, fps);

        // Reconfigure from the full init-time config: NVENC re-init does not
        // merge with the live session config, so the previous sparse config
        // silently zeroed GOP length, idrPeriod, and the H.264 profile.
        NV_ENC_CONFIG enc_config = base_config_;
        enc_config.version = NV_ENC_CONFIG_VER;
        enc_config.rcParams.version = NV_ENC_RC_PARAMS_VER;
        enc_config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_VBR;
        enc_config.rcParams.averageBitRate  = avg;
        enc_config.rcParams.maxBitRate      = max;
        enc_config.rcParams.vbvBufferSize   = vbv;
        enc_config.rcParams.vbvInitialDelay = vbv / 2;

        NV_ENC_RECONFIGURE_PARAMS reconfig;
        memset(&reconfig, 0, sizeof(reconfig));
        reconfig.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        reconfig.reInitEncodeParams.version = NV_ENC_INITIALIZE_PARAMS_VER;
        reconfig.reInitEncodeParams.encodeWidth  = config_.width;
        reconfig.reInitEncodeParams.encodeHeight = config_.height;
        reconfig.reInitEncodeParams.frameRateNum = config_.fps;
        reconfig.reInitEncodeParams.frameRateDen = 1;
        reconfig.reInitEncodeParams.encodeConfig = &enc_config;
        // IDR only on large down-steps: congestion events can leave damaged
        // reference chains, so a forced keyframe helps resync. Routine REMB
        // adjustments must not keyframe-storm the stream.
        const bool big_down_step =
            config_.bitrate_kbps > 0 && kbps * 4 < config_.bitrate_kbps * 3;
        reconfig.forceIDR = big_down_step ? 1 : 0;

        const NVENCSTATUS reconfig_status =
            fn_.nvEncReconfigureEncoder(encoder_, &reconfig);
        if (reconfig_status != NV_ENC_SUCCESS) {
            MELLO_LOG_ERROR(TAG,
                "NVENC: nvEncReconfigureEncoder failed: %d (bitrate=%ukbps)",
                reconfig_status, kbps);
        } else {
            if (big_down_step) {
                force_idr_ = true;
            }
            MELLO_LOG_DEBUG(TAG,
                "NVENC: reconfigured bitrate=%ukbps vbv=%u forceIDR=%d",
                kbps, vbv, reconfig.forceIDR);
        }
    }
    config_.bitrate_kbps = kbps;
}

void NvencEncoder::set_framerate(uint32_t fps) {
    if (fps == 0 || fps == config_.fps) {
        return;
    }
    const uint32_t previous = config_.fps;
    config_.fps = fps;

    if (!encoder_) {
        return;
    }

    // Rebuild from the full init-time config for the same reason set_bitrate
    // does: the driver does not merge sparse configs on re-init. The VBV is
    // recomputed because it is expressed in bits and sized off the framerate —
    // leaving it would hand a 30fps stream a 60fps buffer.
    const uint32_t avg = config_.bitrate_kbps * 1000;
    const uint32_t max = avg + avg / 4;
    const uint32_t vbv = compute_vbv_bits(avg, max, fps);

    NV_ENC_CONFIG enc_config = base_config_;
    enc_config.version = NV_ENC_CONFIG_VER;
    enc_config.rcParams.version = NV_ENC_RC_PARAMS_VER;
    enc_config.rcParams.vbvBufferSize   = vbv;
    enc_config.rcParams.vbvInitialDelay = vbv / 2;

    NV_ENC_RECONFIGURE_PARAMS reconfig;
    memset(&reconfig, 0, sizeof(reconfig));
    reconfig.version = NV_ENC_RECONFIGURE_PARAMS_VER;
    reconfig.reInitEncodeParams.version = NV_ENC_INITIALIZE_PARAMS_VER;
    reconfig.reInitEncodeParams.encodeWidth  = config_.width;
    reconfig.reInitEncodeParams.encodeHeight = config_.height;
    reconfig.reInitEncodeParams.frameRateNum = fps;
    reconfig.reInitEncodeParams.frameRateDen = 1;
    reconfig.reInitEncodeParams.encodeConfig = &enc_config;
    // Cadence change: start a clean GOP rather than leaving viewers on
    // references timed against the old framerate.
    reconfig.forceIDR = 1;

    const NVENCSTATUS status = fn_.nvEncReconfigureEncoder(encoder_, &reconfig);
    if (status != NV_ENC_SUCCESS) {
        config_.fps = previous;
        MELLO_LOG_ERROR(TAG, "NVENC: framerate %u->%u reconfigure failed: %d",
                        previous, fps, status);
        return;
    }

    base_config_ = enc_config;
    force_idr_ = true;
    MELLO_LOG_INFO(TAG, "NVENC: framerate %u -> %u fps (vbv=%u)", previous, fps, vbv);
}

// Tier 0 is the Phase-2 quality configuration. Each step removes the most
// expensive remaining feature rather than degrading everything at once.
//
// Two-pass full-resolution goes first: it encodes every frame twice, so it is
// by far the largest single cost, and on an older NVENC generation it is the
// difference between holding 60fps and not. Temporal AQ goes second.
void NvencEncoder::apply_cost_tier_locked(NV_ENC_CONFIG& cfg) const {
    switch (cost_tier_) {
        case 0:
            break;
        case 1:
            cfg.rcParams.multiPass = NV_ENC_MULTI_PASS_DISABLED;
            break;
        default:
            cfg.rcParams.multiPass        = NV_ENC_MULTI_PASS_DISABLED;
            cfg.rcParams.enableTemporalAQ = 0;
            cfg.rcParams.aqStrength       = 4;
            break;
    }
}

bool NvencEncoder::reduce_cost_tier() {
    if (!encoder_ || cost_tier_ >= kMaxCostTier || cost_tier_unavailable_) {
        return false;
    }
    const int previous = cost_tier_;
    ++cost_tier_;

    NV_ENC_CONFIG enc_config = base_config_;
    enc_config.version = NV_ENC_CONFIG_VER;
    enc_config.rcParams.version = NV_ENC_RC_PARAMS_VER;
    apply_cost_tier_locked(enc_config);

    NV_ENC_RECONFIGURE_PARAMS reconfig;
    memset(&reconfig, 0, sizeof(reconfig));
    reconfig.version = NV_ENC_RECONFIGURE_PARAMS_VER;
    reconfig.reInitEncodeParams.version = NV_ENC_INITIALIZE_PARAMS_VER;
    reconfig.reInitEncodeParams.encodeWidth  = config_.width;
    reconfig.reInitEncodeParams.encodeHeight = config_.height;
    reconfig.reInitEncodeParams.frameRateNum = config_.fps;
    reconfig.reInitEncodeParams.frameRateDen = 1;
    reconfig.reInitEncodeParams.encodeConfig = &enc_config;
    // The rate-control model changes shape here, so start a clean GOP rather
    // than leaving viewers on references produced under the old configuration.
    reconfig.forceIDR = 1;
    // multiPass and temporalAQ are not rate-control knobs — they change the
    // encoder's internal setup, and the driver rejects that as an incompatible
    // reconfiguration (NV_ENC_ERR_UNSUPPORTED_PARAM) unless the encoder state is
    // reset with it. Without this the downgrade silently never happens, which is
    // exactly what it did: every attempt failed and the tier reverted.
    reconfig.resetEncoder = 1;

    const NVENCSTATUS status = fn_.nvEncReconfigureEncoder(encoder_, &reconfig);
    if (status != NV_ENC_SUCCESS) {
        cost_tier_ = previous;
        cost_tier_unavailable_ = true;
        MELLO_LOG_ERROR(TAG,
            "NVENC: cost tier %d->%d reconfigure failed: %d (keeping tier %d, quality downgrade unavailable for this session)",
            previous, previous + 1, status, previous);
        return false;
    }

    // Persist so a later set_bitrate reconfigure does not resurrect the
    // features we just gave up — it rebuilds from base_config_.
    base_config_ = enc_config;
    force_idr_ = true;
    MELLO_LOG_WARN(TAG,
        "NVENC: encoder cannot hold the frame budget — cost tier %d -> %d "
        "(multipass=%s temporalAQ=%u aqStrength=%u)",
        previous, cost_tier_,
        enc_config.rcParams.multiPass == NV_ENC_MULTI_PASS_DISABLED ? "off" : "full",
        enc_config.rcParams.enableTemporalAQ,
        enc_config.rcParams.aqStrength);
    return true;
}

void NvencEncoder::get_phase_timing(EncodePhaseTiming& out) const {
    out.submit_ms = last_submit_ms_;
    out.wait_ms   = last_wait_ms_;
    out.lock_ms   = last_lock_ms_;
}

void NvencEncoder::get_stats(EncoderStats& out) const {
    out = stats_;
}

bool NvencEncoder::supports_codec(VideoCodec codec) const {
    return codec == VideoCodec::H264 || codec == VideoCodec::AV1;
}

} // namespace mello::video
#endif
