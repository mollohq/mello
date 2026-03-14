#ifdef _WIN32
#include "encoder_qsv.hpp"
#include "../util/log.hpp"
#include <Windows.h>
#include <chrono>

namespace mello::video {

static constexpr const char* TAG = "video/encoder";

static HMODULE load_vpl_dll() {
    HMODULE dll = LoadLibraryA("libvpl.dll");
    if (!dll) dll = LoadLibraryA("libmfx64.dll");
    return dll;
}

template<typename T>
static T get_fn(HMODULE dll, const char* name) {
    return reinterpret_cast<T>(GetProcAddress(dll, name));
}

static bool load_dispatch_table(HMODULE dll, MfxDispatchFn& fn) {
    fn.Load                 = get_fn<decltype(fn.Load)>(dll, "MFXLoad");
    fn.Unload               = get_fn<decltype(fn.Unload)>(dll, "MFXUnload");
    fn.CreateConfig         = get_fn<decltype(fn.CreateConfig)>(dll, "MFXCreateConfig");
    fn.SetConfigFilterProperty = get_fn<decltype(fn.SetConfigFilterProperty)>(dll, "MFXSetConfigFilterProperty");
    fn.CreateSession        = get_fn<decltype(fn.CreateSession)>(dll, "MFXCreateSession");
    fn.Close                = get_fn<decltype(fn.Close)>(dll, "MFXClose");
    fn.CoreSetHandle        = get_fn<decltype(fn.CoreSetHandle)>(dll, "MFXVideoCORE_SetHandle");
    fn.CoreSyncOp           = get_fn<decltype(fn.CoreSyncOp)>(dll, "MFXVideoCORE_SyncOperation");
    fn.EncInit              = get_fn<decltype(fn.EncInit)>(dll, "MFXVideoENCODE_Init");
    fn.EncReset             = get_fn<decltype(fn.EncReset)>(dll, "MFXVideoENCODE_Reset");
    fn.EncClose             = get_fn<decltype(fn.EncClose)>(dll, "MFXVideoENCODE_Close");
    fn.EncGetVideoParam     = get_fn<decltype(fn.EncGetVideoParam)>(dll, "MFXVideoENCODE_GetVideoParam");
    fn.EncFrameAsync        = get_fn<decltype(fn.EncFrameAsync)>(dll, "MFXVideoENCODE_EncodeFrameAsync");

    return fn.Load && fn.Unload && fn.CreateSession && fn.Close &&
           fn.CoreSetHandle && fn.CoreSyncOp &&
           fn.EncInit && fn.EncClose && fn.EncFrameAsync;
}

bool QsvEncoder::is_available() {
    HMODULE dll = load_vpl_dll();
    if (dll) { FreeLibrary(dll); return true; }
    return false;
}

bool QsvEncoder::initialize(const GraphicsDevice& device, const EncoderConfig& config) {
    device_ = device.d3d11();
    config_ = config;
    stats_  = {};
    frame_seq_ = 0;

    dll_ = load_vpl_dll();
    if (!dll_) {
        MELLO_LOG_DEBUG(TAG, "Probing QSV... not available (oneVPL runtime missing)");
        return false;
    }

    if (!load_dispatch_table(dll_, fn_)) {
        MELLO_LOG_DEBUG(TAG, "Probing QSV... failed to resolve oneVPL entry points");
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    loader_ = fn_.Load();
    if (!loader_) {
        MELLO_LOG_DEBUG(TAG, "Probing QSV... MFXLoad() failed");
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    if (fn_.CreateConfig && fn_.SetConfigFilterProperty) {
        mfxConfig cfg = fn_.CreateConfig(loader_);
        mfxVariant val;
        val.Type = MFX_VARIANT_TYPE_U32;
        val.Data.U32 = MFX_IMPL_TYPE_HARDWARE;
        fn_.SetConfigFilterProperty(cfg, (mfxU8*)"mfxImplDescription.Impl", val);
        val.Data.U32 = MFX_ACCEL_MODE_VIA_D3D11;
        fn_.SetConfigFilterProperty(cfg, (mfxU8*)"mfxImplDescription.AccelerationMode", val);
    }

    mfxStatus sts = fn_.CreateSession(loader_, 0, &session_);
    if (sts != MFX_ERR_NONE) {
        MELLO_LOG_DEBUG(TAG, "Probing QSV... MFXCreateSession failed: %d", sts);
        fn_.Unload(loader_); loader_ = nullptr;
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    sts = fn_.CoreSetHandle(session_, MFX_HANDLE_D3D11_DEVICE, device_.Get());
    if (sts != MFX_ERR_NONE) {
        MELLO_LOG_DEBUG(TAG, "Probing QSV... SetHandle failed: %d", sts);
        fn_.Close(session_); session_ = nullptr;
        fn_.Unload(loader_); loader_ = nullptr;
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    memset(&video_params_, 0, sizeof(video_params_));
    video_params_.mfx.CodecId                  = MFX_CODEC_AVC;
    video_params_.mfx.TargetUsage              = MFX_TARGETUSAGE_BEST_SPEED;
    video_params_.mfx.TargetKbps               = static_cast<mfxU16>(config.bitrate_kbps);
    video_params_.mfx.MaxKbps                  = static_cast<mfxU16>(config.bitrate_kbps);
    video_params_.mfx.RateControlMethod        = MFX_RATECONTROL_CBR;
    video_params_.mfx.FrameInfo.FrameRateExtN  = config.fps;
    video_params_.mfx.FrameInfo.FrameRateExtD  = 1;
    video_params_.mfx.FrameInfo.FourCC         = MFX_FOURCC_NV12;
    video_params_.mfx.FrameInfo.ChromaFormat   = MFX_CHROMAFORMAT_YUV420;
    video_params_.mfx.FrameInfo.Width          = (config.width + 15) & ~15;
    video_params_.mfx.FrameInfo.Height         = (config.height + 15) & ~15;
    video_params_.mfx.FrameInfo.CropW          = static_cast<mfxU16>(config.width);
    video_params_.mfx.FrameInfo.CropH          = static_cast<mfxU16>(config.height);
    video_params_.mfx.GopPicSize               = static_cast<mfxU16>(config.keyframe_interval);
    video_params_.mfx.GopRefDist               = 1;
    video_params_.mfx.NumRefFrame              = 1;
    video_params_.IOPattern                    = MFX_IOPATTERN_IN_VIDEO_MEMORY;

    sts = fn_.EncInit(session_, &video_params_);
    if (sts != MFX_ERR_NONE && sts != MFX_WRN_PARTIAL_ACCELERATION) {
        MELLO_LOG_ERROR(TAG, "QSV: MFXVideoENCODE_Init failed: %d", sts);
        fn_.Close(session_); session_ = nullptr;
        fn_.Unload(loader_); loader_ = nullptr;
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    if (fn_.EncGetVideoParam) {
        mfxVideoParam actual_params{};
        fn_.EncGetVideoParam(session_, &actual_params);
        uint32_t bs_size = actual_params.mfx.BufferSizeInKB * 1024;
        if (bs_size == 0) bs_size = config.width * config.height * 2;
        bs_buf_.resize(bs_size);
    } else {
        bs_buf_.resize(config.width * config.height * 2);
    }

    memset(&bitstream_, 0, sizeof(bitstream_));
    bitstream_.Data      = bs_buf_.data();
    bitstream_.MaxLength = static_cast<mfxU32>(bs_buf_.size());

    MELLO_LOG_DEBUG(TAG, "Probing QSV... ok");
    MELLO_LOG_INFO(TAG, "Selected encoder: QSV-oneVPL codec=H264 resolution=%ux%u fps=%u bitrate=%ukbps",
        config.width, config.height, config.fps, config.bitrate_kbps);
    return true;
}

void QsvEncoder::shutdown() {
    if (session_) {
        if (fn_.EncClose) fn_.EncClose(session_);
        if (fn_.Close)    fn_.Close(session_);
        session_ = nullptr;
    }
    if (loader_) {
        if (fn_.Unload) fn_.Unload(loader_);
        loader_ = nullptr;
    }
    if (dll_) { FreeLibrary(dll_); dll_ = nullptr; }
    bs_buf_.clear();
    fn_ = {};
}

bool QsvEncoder::encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) {
    if (!session_ || !fn_.EncFrameAsync) return false;

    mfxFrameSurface1 surface{};
    surface.Info = video_params_.mfx.FrameInfo;
    surface.Data.MemType = MFX_MEMTYPE_VIDEO_MEMORY_ENCODER_TARGET;
    surface.Data.MemId   = nv12_texture;

    mfxEncodeCtrl ctrl{};
    mfxEncodeCtrl* ctrl_ptr = nullptr;
    if (force_idr_) {
        ctrl.FrameType = MFX_FRAMETYPE_I | MFX_FRAMETYPE_IDR | MFX_FRAMETYPE_REF;
        ctrl_ptr = &ctrl;
        force_idr_ = false;
    }

    bitstream_.DataLength = 0;
    bitstream_.DataOffset = 0;

    mfxSyncPoint sync;
    mfxStatus sts = fn_.EncFrameAsync(session_, ctrl_ptr, &surface, &bitstream_, &sync);

    if (sts == MFX_ERR_MORE_DATA) return false;
    if (sts != MFX_ERR_NONE && sts != MFX_WRN_DEVICE_BUSY) {
        MELLO_LOG_ERROR(TAG, "QSV: EncodeFrameAsync failed: %d (seq=%llu)", sts, frame_seq_);
        return false;
    }

    sts = fn_.CoreSyncOp(session_, sync, 1000);
    if (sts != MFX_ERR_NONE) {
        MELLO_LOG_ERROR(TAG, "QSV: SyncOperation failed: %d (seq=%llu)", sts, frame_seq_);
        return false;
    }

    out.data.assign(
        bitstream_.Data + bitstream_.DataOffset,
        bitstream_.Data + bitstream_.DataOffset + bitstream_.DataLength);
    out.is_keyframe  = (bitstream_.FrameType & MFX_FRAMETYPE_IDR) != 0;
    out.timestamp_us = static_cast<uint64_t>(
        std::chrono::duration_cast<std::chrono::microseconds>(
            std::chrono::steady_clock::now().time_since_epoch()).count());

    frame_seq_++;
    stats_.bytes_sent += out.data.size();
    stats_.fps_actual = config_.fps;
    stats_.bitrate_kbps = config_.bitrate_kbps;
    if (out.is_keyframe) stats_.keyframes_sent++;

    return true;
}

void QsvEncoder::request_keyframe() {
    force_idr_ = true;
    MELLO_LOG_DEBUG(TAG, "Keyframe requested (QSV seq=%llu)", frame_seq_);
}

void QsvEncoder::set_bitrate(uint32_t kbps) {
    if (session_ && fn_.EncReset) {
        video_params_.mfx.TargetKbps = static_cast<mfxU16>(kbps);
        video_params_.mfx.MaxKbps    = static_cast<mfxU16>(kbps);
        fn_.EncReset(session_, &video_params_);
    }
    config_.bitrate_kbps = kbps;
}

void QsvEncoder::get_stats(EncoderStats& out) const {
    out = stats_;
}

bool QsvEncoder::supports_codec(VideoCodec codec) const {
    return codec == VideoCodec::H264;
}

} // namespace mello::video
#endif
