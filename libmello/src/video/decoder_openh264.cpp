#ifdef _WIN32
#include "decoder_openh264.hpp"
#include "openh264_dynload.hpp"
#include "../util/log.hpp"
#include <cstring>

// Minimal OpenH264 ABI definitions for ISVCDecoder.
// These match the Cisco prebuilt DLL's vtable layout.
// We define them here instead of #including wels/codec_api.h because
// we runtime-load the DLL and don't link against it.

enum DECODING_STATE {
    dsErrorFree       = 0x00,
    dsFramePending    = 0x01,
    dsRefLost         = 0x02,
    dsBitstreamError  = 0x04,
    dsDepLayerLost    = 0x08,
    dsNoParamSets     = 0x10,
    dsDataErrorConcealed = 0x20,
    dsRefListNullPtrs = 0x40,
    dsInvalidArgument = 0x1000,
    dsInitialOpt      = 0x2000,
    dsOutOfMemory     = 0x4000,
    dsDstBufNeedExpan = 0x8000,
};

enum VIDEO_BITSTREAM_TYPE {
    VIDEO_BITSTREAM_DEFAULT = 0,
    VIDEO_BITSTREAM_SVC     = 1,
    VIDEO_BITSTREAM_AVC     = 2,
};

enum EVideoFormatType {
    videoFormatRGB     = 1,
    videoFormatRGBA    = 2,
    videoFormatRGB555  = 3,
    videoFormatRGB565  = 4,
    videoFormatBGR     = 5,
    videoFormatBGRA    = 6,
    videoFormatABGR    = 7,
    videoFormatARGB    = 8,
    videoFormatYUY2    = 20,
    videoFormatYVYU    = 21,
    videoFormatUYVY    = 22,
    videoFormatI420    = 23,
    videoFormatYV12    = 24,
    videoFormatInternal = 25,
    videoFormatNV12    = 26,
};

struct SDecodingParam {
    char*    pFileRecPath;
    unsigned uiCpuLoad;
    unsigned char uiTargetDqLayer;
    unsigned char eEcActiveIdc;
    bool     bParseOnly;
    int      sVideoProperty_size;
    unsigned sVideoProperty_eVideoBsType;
};

struct SSysMEMBuffer {
    int iWidth;
    int iHeight;
    int iFormat;
    int iStride[2];
};

struct SBufferInfo {
    int iBufferStatus;
    unsigned long long uiInBsTimeStamp;
    unsigned long long uiOutYuvTimeStamp;
    union {
        SSysMEMBuffer sSystemBuffer;
    } UsrData;
};

// ISVCDecoder vtable layout (COM-like, matches the Cisco DLL)
struct ISVCDecoderVtbl {
    long (*Initialize)(ISVCDecoder*, const SDecodingParam*);
    long (*Uninitialize)(ISVCDecoder*);
    int  (*DecodeFrame2)(ISVCDecoder*, const unsigned char*, int, unsigned char*[3], SBufferInfo*);
    int  (*DecodeFrameNoDelay)(ISVCDecoder*, const unsigned char*, int, unsigned char*[3], SBufferInfo*);
    int  (*DecodeParser)(ISVCDecoder*, const unsigned char*, int, SBufferInfo*);
    long (*SetOption)(ISVCDecoder*, int, void*);
    long (*GetOption)(ISVCDecoder*, int, void*);
};

struct ISVCDecoder {
    ISVCDecoderVtbl* pVtbl;
};

namespace mello::video {

static constexpr const char* TAG = "video/decoder";

bool OpenH264Decoder::is_available() {
    return openh264::load();
}

bool OpenH264Decoder::initialize(const GraphicsDevice& device, const DecoderConfig& config) {
    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);
    config_ = config;

    nv12_buf_.resize(static_cast<size_t>(config.width) * config.height * 3 / 2);

    D3D11_TEXTURE2D_DESC tex_desc{};
    tex_desc.Width  = config.width;
    tex_desc.Height = config.height;
    tex_desc.MipLevels = 1;
    tex_desc.ArraySize = 1;
    tex_desc.Format = DXGI_FORMAT_NV12;
    tex_desc.SampleDesc.Count = 1;
    tex_desc.Usage = D3D11_USAGE_DEFAULT;
    tex_desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;

    HRESULT hr = device_->CreateTexture2D(&tex_desc, nullptr, &upload_tex_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "OpenH264 decoder: Failed to create upload texture: hr=0x%08X", hr);
        return false;
    }

    if (!openh264::load()) {
        MELLO_LOG_WARN(TAG, "OpenH264 DLL not available");
        return false;
    }

    auto& dll = openh264::api();
    long rv = dll.create_decoder(&decoder_);
    if (rv != 0 || !decoder_) {
        MELLO_LOG_ERROR(TAG, "OpenH264: WelsCreateDecoder failed: %ld", rv);
        return false;
    }

    SDecodingParam param{};
    memset(&param, 0, sizeof(param));
    param.sVideoProperty_eVideoBsType = VIDEO_BITSTREAM_AVC;

    rv = decoder_->pVtbl->Initialize(decoder_, &param);
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
        decoder_->pVtbl->Uninitialize(decoder_);
        if (openh264::is_loaded()) {
            openh264::api().destroy_decoder(decoder_);
        }
        decoder_ = nullptr;
    }
    upload_tex_.Reset();
    context_.Reset();
    device_.Reset();
    nv12_buf_.clear();
}

bool OpenH264Decoder::decode(const uint8_t* data, size_t size, bool is_keyframe) {
    (void)is_keyframe;
    if (!decoder_) return false;

    unsigned char* yuv[3] = { nullptr, nullptr, nullptr };
    SBufferInfo buf_info{};
    memset(&buf_info, 0, sizeof(buf_info));

    int rv = decoder_->pVtbl->DecodeFrameNoDelay(
        decoder_, data, static_cast<int>(size), yuv, &buf_info);

    if (rv != 0 || buf_info.iBufferStatus != 1) return false;

    uint32_t w = config_.width;
    uint32_t h = config_.height;
    int y_stride  = buf_info.UsrData.sSystemBuffer.iStride[0];
    int uv_stride = buf_info.UsrData.sSystemBuffer.iStride[1];

    // Convert I420 -> NV12 for D3D11 texture upload
    uint8_t* nv12_y  = nv12_buf_.data();
    uint8_t* nv12_uv = nv12_y + w * h;

    // Copy Y plane
    for (uint32_t row = 0; row < h; ++row) {
        memcpy(nv12_y + row * w, yuv[0] + row * y_stride, w);
    }

    // Interleave U + V into NV12 UV plane
    uint32_t uv_h = h / 2;
    uint32_t uv_w = w / 2;
    for (uint32_t row = 0; row < uv_h; ++row) {
        const uint8_t* u_row = yuv[1] + row * uv_stride;
        const uint8_t* v_row = yuv[2] + row * uv_stride;
        uint8_t* dst = nv12_uv + row * w;
        for (uint32_t col = 0; col < uv_w; ++col) {
            dst[col * 2]     = u_row[col];
            dst[col * 2 + 1] = v_row[col];
        }
    }

    context_->UpdateSubresource(
        upload_tex_.Get(), 0, nullptr,
        nv12_buf_.data(), w,
        static_cast<UINT>(nv12_buf_.size()));

    return true;
}

ID3D11Texture2D* OpenH264Decoder::get_frame() {
    return upload_tex_.Get();
}

} // namespace mello::video
#endif
