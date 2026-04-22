#ifdef _WIN32
#include "encoder_nvenc.hpp"
#include "../util/log.hpp"
#include <Windows.h>
#include <chrono>

namespace mello::video {

static constexpr const char* TAG = "video/encoder";

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
        { NV_ENC_PRESET_P1_GUID, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, "P1+ULL" },
        { NV_ENC_PRESET_P1_GUID, NV_ENC_TUNING_INFO_LOW_LATENCY,       "P1+LL"  },
        { NV_ENC_PRESET_P4_GUID, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, "P4+ULL" },
    };

    bool have_preset = false;
    GUID used_preset = NV_ENC_PRESET_P1_GUID;
    for (auto& a : attempts) {
        memset(&preset_config, 0, sizeof(preset_config));
        preset_config.version = NV_ENC_PRESET_CONFIG_VER;
        preset_config.presetCfg.version = NV_ENC_CONFIG_VER;
        status = fn_.nvEncGetEncodePresetConfigEx(encoder_, codec_guid, a.preset, a.tuning, &preset_config);
        MELLO_LOG_INFO(TAG, "NVENC: PresetConfigEx(%s) => %d", a.label, status);
        if (status == NV_ENC_SUCCESS) {
            have_preset = true;
            used_preset = a.preset;
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
    // without large bandwidth spikes. 1x VBV keeps rate control tight for
    // smooth bandwidth usage over P2P links.
    uint32_t avg = config.bitrate_kbps * 1000;
    uint32_t max = avg + avg / 4;
    enc_config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_VBR;
    enc_config.rcParams.averageBitRate  = avg;
    enc_config.rcParams.maxBitRate      = max;
    enc_config.rcParams.vbvBufferSize   = avg;
    enc_config.frameIntervalP = 1;
    enc_config.gopLength      = config.keyframe_interval;

    if (config.codec == VideoCodec::H264) {
        enc_config.encodeCodecConfig.h264Config.idrPeriod         = config.keyframe_interval;
        enc_config.encodeCodecConfig.h264Config.enableIntraRefresh = 0;
        enc_config.encodeCodecConfig.h264Config.repeatSPSPPS      = 1;
    }

    NV_ENC_INITIALIZE_PARAMS init_params;
    memset(&init_params, 0, sizeof(init_params));
    init_params.version       = NV_ENC_INITIALIZE_PARAMS_VER;
    init_params.encodeGUID    = codec_guid;
    init_params.presetGUID    = used_preset;
    init_params.encodeWidth   = config.width;
    init_params.encodeHeight  = config.height;
    init_params.darWidth      = config.width;
    init_params.darHeight     = config.height;
    init_params.frameRateNum  = config.fps;
    init_params.frameRateDen  = 1;
    init_params.enablePTD     = 1;
    init_params.encodeConfig  = &enc_config;
    init_params.tuningInfo    = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;

    MELLO_LOG_INFO(TAG, "NVENC: nvEncInitializeEncoder %ux%u (initVer=0x%08X cfgVer=0x%08X rcVer=0x%08X)",
        config.width, config.height, init_params.version, enc_config.version, enc_config.rcParams.version);

    status = fn_.nvEncInitializeEncoder(encoder_, &init_params);
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

    NV_ENC_CREATE_BITSTREAM_BUFFER bstream = {NV_ENC_CREATE_BITSTREAM_BUFFER_VER};
    status = fn_.nvEncCreateBitstreamBuffer(encoder_, &bstream);
    if (status != NV_ENC_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "nvEncCreateBitstreamBuffer failed: %d", status);
        fn_.nvEncDestroyEncoder(encoder_); encoder_ = nullptr;
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }
    out_buf_ = bstream.bitstreamBuffer;

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

    // Encode
    NV_ENC_PIC_PARAMS pic = {NV_ENC_PIC_PARAMS_VER};
    pic.inputBuffer       = mapped_input_;
    pic.bufferFmt         = NV_ENC_BUFFER_FORMAT_NV12;
    pic.inputWidth        = config_.width;
    pic.inputHeight       = config_.height;
    pic.outputBitstream   = out_buf_;
    pic.pictureStruct     = NV_ENC_PIC_STRUCT_FRAME;

    if (force_idr_) {
        pic.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
        force_idr_ = false;
    }

    status = fn_.nvEncEncodePicture(encoder_, &pic);
    if (status != NV_ENC_SUCCESS && status != NV_ENC_ERR_NEED_MORE_INPUT) {
        MELLO_LOG_ERROR(TAG, "NVENC: nvEncEncodePicture failed: %d (seq=%llu)", status, frame_seq_);
        fn_.nvEncUnmapInputResource(encoder_, mapped_input_);
        mapped_input_ = nullptr;
        return false;
    }

    // Lock bitstream and copy to output
    NV_ENC_LOCK_BITSTREAM lock = {NV_ENC_LOCK_BITSTREAM_VER};
    lock.outputBitstream = out_buf_;

    status = fn_.nvEncLockBitstream(encoder_, &lock);
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
    stats_.fps_actual = config_.fps;
    stats_.bitrate_kbps = config_.bitrate_kbps;
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
        uint32_t avg = kbps * 1000;
        uint32_t max = avg + avg / 4;

        NV_ENC_RECONFIGURE_PARAMS reconfig = {NV_ENC_RECONFIGURE_PARAMS_VER};
        NV_ENC_CONFIG enc_config = {NV_ENC_CONFIG_VER};
        enc_config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_VBR;
        enc_config.rcParams.averageBitRate  = avg;
        enc_config.rcParams.maxBitRate      = max;
        enc_config.rcParams.vbvBufferSize   = avg;

        NV_ENC_INITIALIZE_PARAMS init = {NV_ENC_INITIALIZE_PARAMS_VER};
        init.encodeWidth  = config_.width;
        init.encodeHeight = config_.height;
        init.frameRateNum = config_.fps;
        init.frameRateDen = 1;
        init.encodeConfig = &enc_config;

        reconfig.reInitEncodeParams = init;
        // Bitrate changes should not force a keyframe — unnecessary IDRs waste
        // bandwidth and cause quality dips. Keyframes come from request_keyframe()
        // or the scheduled GOP interval only.
        reconfig.forceIDR = 0;
        fn_.nvEncReconfigureEncoder(encoder_, &reconfig);
    }
    config_.bitrate_kbps = kbps;
}

void NvencEncoder::get_stats(EncoderStats& out) const {
    out = stats_;
}

bool NvencEncoder::supports_codec(VideoCodec codec) const {
    return codec == VideoCodec::H264 || codec == VideoCodec::AV1;
}

} // namespace mello::video
#endif
