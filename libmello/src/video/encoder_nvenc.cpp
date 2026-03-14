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

#ifdef MELLO_HAS_NVENC
    auto pfn_create = reinterpret_cast<PFN_NvEncodeAPICreateInstance>(
        GetProcAddress(dll_, "NvEncodeAPICreateInstance"));
    if (!pfn_create) {
        MELLO_LOG_DEBUG(TAG, "Probing NVENC... not available (entry point not found)");
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    fn_ = {NV_ENCODE_API_FUNCTION_LIST_VER};
    NVENCSTATUS status = pfn_create(&fn_);
    if (status != NV_ENC_SUCCESS) {
        MELLO_LOG_DEBUG(TAG, "Probing NVENC... NvEncodeAPICreateInstance failed: %d", status);
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    // Open encode session with D3D11 device
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS session_params = {NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER};
    session_params.device     = device_.Get();
    session_params.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
    session_params.apiVersion = NVENCAPI_VERSION;

    status = fn_.nvEncOpenEncodeSessionEx(&session_params, &encoder_);
    if (status != NV_ENC_SUCCESS) {
        MELLO_LOG_DEBUG(TAG, "Probing NVENC... nvEncOpenEncodeSessionEx failed: %d", status);
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    // Initialize encoder
    GUID codec_guid = (config.codec == VideoCodec::AV1) ? NV_ENC_CODEC_AV1_GUID : NV_ENC_CODEC_H264_GUID;
    GUID preset_guid = NV_ENC_PRESET_P1_GUID; // Lowest latency

    NV_ENC_PRESET_CONFIG preset_config = {NV_ENC_PRESET_CONFIG_VER, {NV_ENC_CONFIG_VER}};
    status = fn_.nvEncGetEncodePresetConfigEx(encoder_, codec_guid, preset_guid,
                                              NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, &preset_config);
    if (status != NV_ENC_SUCCESS) {
        MELLO_LOG_WARN(TAG, "nvEncGetEncodePresetConfigEx failed: %d, trying default", status);
        status = fn_.nvEncGetEncodePresetConfig(encoder_, codec_guid, preset_guid, &preset_config);
    }

    NV_ENC_CONFIG enc_config = preset_config.presetCfg;
    enc_config.version = NV_ENC_CONFIG_VER;

    // Rate control: CBR, no B-frames, low-latency
    enc_config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    enc_config.rcParams.averageBitRate  = config.bitrate_kbps * 1000;
    enc_config.rcParams.maxBitRate      = config.bitrate_kbps * 1000;
    enc_config.rcParams.vbvBufferSize   = config.bitrate_kbps * 1000; // 1-second VBV
    enc_config.frameIntervalP = 1; // No B-frames
    enc_config.gopLength      = config.keyframe_interval;

    if (config.codec == VideoCodec::H264) {
        enc_config.encodeCodecConfig.h264Config.idrPeriod       = config.keyframe_interval;
        enc_config.encodeCodecConfig.h264Config.enableIntraRefresh = 0;
        enc_config.encodeCodecConfig.h264Config.repeatSPSPPS    = 1;
    }

    NV_ENC_INITIALIZE_PARAMS init_params = {NV_ENC_INITIALIZE_PARAMS_VER};
    init_params.encodeGUID    = codec_guid;
    init_params.presetGUID    = preset_guid;
    init_params.encodeWidth   = config.width;
    init_params.encodeHeight  = config.height;
    init_params.darWidth      = config.width;
    init_params.darHeight     = config.height;
    init_params.frameRateNum  = config.fps;
    init_params.frameRateDen  = 1;
    init_params.enablePTD     = 1;
    init_params.encodeConfig  = &enc_config;
    init_params.tuningInfo    = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;

    status = fn_.nvEncInitializeEncoder(encoder_, &init_params);
    if (status != NV_ENC_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "nvEncInitializeEncoder failed: %d", status);
        fn_.nvEncDestroyEncoder(encoder_); encoder_ = nullptr;
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    // Create output bitstream buffer
    NV_ENC_CREATE_BITSTREAM_BUFFER bstream = {NV_ENC_CREATE_BITSTREAM_BUFFER_VER};
    status = fn_.nvEncCreateBitstreamBuffer(encoder_, &bstream);
    if (status != NV_ENC_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "nvEncCreateBitstreamBuffer failed: %d", status);
        fn_.nvEncDestroyEncoder(encoder_); encoder_ = nullptr;
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }
    out_buf_ = bstream.bitstreamBuffer;

    MELLO_LOG_DEBUG(TAG, "Probing NVENC... ok");
    MELLO_LOG_INFO(TAG, "Selected encoder: NVENC codec=%s resolution=%ux%u fps=%u bitrate=%ukbps",
        config.codec == VideoCodec::H264 ? "H264" : "AV1",
        config.width, config.height, config.fps, config.bitrate_kbps);

    return true;
#else
    MELLO_LOG_DEBUG(TAG, "Probing NVENC... SDK headers not available at build time");
    FreeLibrary(dll_); dll_ = nullptr;
    return false;
#endif
}

void NvencEncoder::shutdown() {
#ifdef MELLO_HAS_NVENC
    if (encoder_) {
        if (reg_res_) {
            fn_.nvEncUnregisterResource(encoder_, reg_res_);
            reg_res_ = nullptr;
        }
        if (out_buf_) {
            fn_.nvEncDestroyBitstreamBuffer(encoder_, out_buf_);
            out_buf_ = nullptr;
        }
        fn_.nvEncDestroyEncoder(encoder_);
        encoder_ = nullptr;
    }
#endif
    if (dll_) {
        FreeLibrary(dll_);
        dll_ = nullptr;
    }
}

bool NvencEncoder::encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) {
#ifdef MELLO_HAS_NVENC
    if (!encoder_) return false;

    // Register the input texture if this is the first frame or texture changed.
    // For simplicity, we re-register each frame. A real optimization would cache
    // the registration when the texture pointer doesn't change.
    if (reg_res_) {
        fn_.nvEncUnregisterResource(encoder_, reg_res_);
        reg_res_ = nullptr;
    }

    NV_ENC_REGISTER_RESOURCE reg = {NV_ENC_REGISTER_RESOURCE_VER};
    reg.resourceType          = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
    reg.resourceToRegister    = nv12_texture;
    reg.width                 = config_.width;
    reg.height                = config_.height;
    reg.bufferFormat          = NV_ENC_BUFFER_FORMAT_NV12;
    reg.bufferUsage           = NV_ENC_INPUT_IMAGE;

    NVENCSTATUS status = fn_.nvEncRegisterResource(encoder_, &reg);
    if (status != NV_ENC_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVENC: nvEncRegisterResource failed: %d (seq=%llu)", status, frame_seq_);
        return false;
    }
    reg_res_ = reg.registeredResource;

    // Map the registered resource
    NV_ENC_MAP_INPUT_RESOURCE map = {NV_ENC_MAP_INPUT_RESOURCE_VER};
    map.registeredResource = reg_res_;

    status = fn_.nvEncMapInputResource(encoder_, &map);
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
#else
    (void)nv12_texture; (void)out;
    return false;
#endif
}

void NvencEncoder::request_keyframe() {
    force_idr_ = true;
    MELLO_LOG_DEBUG(TAG, "Keyframe requested (NVENC seq=%llu)", frame_seq_);
}

void NvencEncoder::set_bitrate(uint32_t kbps) {
#ifdef MELLO_HAS_NVENC
    if (encoder_) {
        NV_ENC_RECONFIGURE_PARAMS reconfig = {NV_ENC_RECONFIGURE_PARAMS_VER};
        NV_ENC_CONFIG enc_config = {NV_ENC_CONFIG_VER};
        enc_config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
        enc_config.rcParams.averageBitRate  = kbps * 1000;
        enc_config.rcParams.maxBitRate      = kbps * 1000;
        enc_config.rcParams.vbvBufferSize   = kbps * 1000;

        NV_ENC_INITIALIZE_PARAMS init = {NV_ENC_INITIALIZE_PARAMS_VER};
        init.encodeWidth  = config_.width;
        init.encodeHeight = config_.height;
        init.frameRateNum = config_.fps;
        init.frameRateDen = 1;
        init.encodeConfig = &enc_config;

        reconfig.reInitEncodeParams = init;
        reconfig.forceIDR = 1;
        fn_.nvEncReconfigureEncoder(encoder_, &reconfig);
    }
#endif
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
