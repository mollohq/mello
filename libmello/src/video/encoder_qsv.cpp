#ifdef _WIN32
#include "encoder_qsv.hpp"
#include "../util/log.hpp"
#include <Windows.h>
#include <chrono>

namespace mello::video {

static constexpr const char* TAG = "video/encoder";

bool QsvEncoder::is_available() {
    HMODULE dll = LoadLibraryA("libvpl.dll");
    if (!dll) dll = LoadLibraryA("libmfx64.dll");
    if (dll) { FreeLibrary(dll); return true; }
    return false;
}

bool QsvEncoder::initialize(const GraphicsDevice& device, const EncoderConfig& config) {
    device_ = device.d3d11();
    config_ = config;
    stats_  = {};
    frame_seq_ = 0;

    if (!is_available()) {
        MELLO_LOG_DEBUG(TAG, "Probing QSV... not available (oneVPL runtime missing)");
        return false;
    }

#ifdef MELLO_HAS_QSV
    loader_ = MFXLoad();
    if (!loader_) {
        MELLO_LOG_DEBUG(TAG, "Probing QSV... MFXLoad() failed");
        return false;
    }

    // Filter for HW implementation with D3D11 support
    mfxConfig cfg = MFXCreateConfig(loader_);
    mfxVariant val;

    val.Type = MFX_VARIANT_TYPE_U32;
    val.Data.U32 = MFX_IMPL_TYPE_HARDWARE;
    MFXSetConfigFilterProperty(cfg, (mfxU8*)"mfxImplDescription.Impl", val);

    val.Data.U32 = MFX_ACCEL_MODE_VIA_D3D11;
    MFXSetConfigFilterProperty(cfg, (mfxU8*)"mfxImplDescription.AccelerationMode", val);

    mfxStatus sts = MFXCreateSession(loader_, 0, &session_);
    if (sts != MFX_ERR_NONE) {
        MELLO_LOG_DEBUG(TAG, "Probing QSV... MFXCreateSession failed: %d", sts);
        MFXUnload(loader_); loader_ = nullptr;
        return false;
    }

    // Set D3D11 device handle
    sts = MFXVideoCORE_SetHandle(session_, MFX_HANDLE_D3D11_DEVICE, device_.Get());
    if (sts != MFX_ERR_NONE) {
        MELLO_LOG_DEBUG(TAG, "Probing QSV... SetHandle failed: %d", sts);
        MFXClose(session_); session_ = nullptr;
        MFXUnload(loader_); loader_ = nullptr;
        return false;
    }

    // Configure encoder parameters
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
    video_params_.mfx.GopRefDist               = 1; // No B-frames
    video_params_.mfx.NumRefFrame              = 1;
    video_params_.IOPattern                    = MFX_IOPATTERN_IN_VIDEO_MEMORY;

    sts = MFXVideoENCODE_Init(session_, &video_params_);
    if (sts != MFX_ERR_NONE && sts != MFX_WRN_PARTIAL_ACCELERATION) {
        MELLO_LOG_ERROR(TAG, "QSV: MFXVideoENCODE_Init failed: %d", sts);
        MFXClose(session_); session_ = nullptr;
        MFXUnload(loader_); loader_ = nullptr;
        return false;
    }

    // Allocate bitstream buffer
    mfxVideoParam actual_params{};
    MFXVideoENCODE_GetVideoParam(session_, &actual_params);
    uint32_t bs_size = actual_params.mfx.BufferSizeInKB * 1024;
    if (bs_size == 0) bs_size = config.width * config.height * 2;
    bs_buf_.resize(bs_size);

    memset(&bitstream_, 0, sizeof(bitstream_));
    bitstream_.Data      = bs_buf_.data();
    bitstream_.MaxLength = static_cast<mfxU32>(bs_buf_.size());

    MELLO_LOG_DEBUG(TAG, "Probing QSV... ok");
    MELLO_LOG_INFO(TAG, "Selected encoder: QSV-oneVPL codec=H264 resolution=%ux%u fps=%u bitrate=%ukbps",
        config.width, config.height, config.fps, config.bitrate_kbps);
    return true;
#else
    MELLO_LOG_DEBUG(TAG, "Probing QSV... SDK headers not available at build time");
    return false;
#endif
}

void QsvEncoder::shutdown() {
#ifdef MELLO_HAS_QSV
    if (session_) {
        MFXVideoENCODE_Close(session_);
        MFXClose(session_);
        session_ = nullptr;
    }
    if (loader_) {
        MFXUnload(loader_);
        loader_ = nullptr;
    }
    bs_buf_.clear();
#endif
}

bool QsvEncoder::encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) {
#ifdef MELLO_HAS_QSV
    if (!session_) return false;

    // Wrap the NV12 texture as an mfxFrameSurface1
    mfxFrameSurface1 surface{};
    surface.Info = video_params_.mfx.FrameInfo;
    surface.Data.MemType = MFX_MEMTYPE_D3D11_TEXTURE;
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
    mfxStatus sts = MFXVideoENCODE_EncodeFrameAsync(session_, ctrl_ptr, &surface, &bitstream_, &sync);

    if (sts == MFX_ERR_MORE_DATA) return false;
    if (sts != MFX_ERR_NONE && sts != MFX_WRN_DEVICE_BUSY) {
        MELLO_LOG_ERROR(TAG, "QSV: EncodeFrameAsync failed: %d (seq=%llu)", sts, frame_seq_);
        return false;
    }

    sts = MFXVideoCORE_SyncOperation(session_, sync, 1000);
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
#else
    (void)nv12_texture; (void)out;
    return false;
#endif
}

void QsvEncoder::request_keyframe() {
    force_idr_ = true;
    MELLO_LOG_DEBUG(TAG, "Keyframe requested (QSV seq=%llu)", frame_seq_);
}

void QsvEncoder::set_bitrate(uint32_t kbps) {
#ifdef MELLO_HAS_QSV
    if (session_) {
        video_params_.mfx.TargetKbps = static_cast<mfxU16>(kbps);
        video_params_.mfx.MaxKbps    = static_cast<mfxU16>(kbps);
        MFXVideoENCODE_Reset(session_, &video_params_);
    }
#endif
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
