#ifdef _WIN32
#include "decoder_amf.hpp"
#include "../util/log.hpp"
#include <Windows.h>

#include <AMF/core/Data.h>
#include <AMF/core/Surface.h>
#include <AMF/core/Buffer.h>
typedef AMF_RESULT(AMF_CDECL_CALL* AMFInit_Fn)(amf_uint64, amf::AMFFactory**);

namespace mello::video {

static constexpr const char* TAG = "video/decoder";

static HMODULE load_amf_dll() { return LoadLibraryA("amfrt64.dll"); }

bool AmfDecoder::is_available() {
    HMODULE dll = load_amf_dll();
    if (dll) { FreeLibrary(dll); return true; }
    return false;
}

bool AmfDecoder::initialize(const GraphicsDevice& device, const DecoderConfig& config) {
    device_ = device.d3d11();
    config_ = config;

    dll_ = load_amf_dll();
    if (!dll_) {
        MELLO_LOG_DEBUG(TAG, "Probing AMF decode... not available (amfrt64.dll not found)");
        return false;
    }

    auto amf_init = reinterpret_cast<AMFInit_Fn>(GetProcAddress(dll_, AMF_INIT_FUNCTION_NAME));
    if (!amf_init) {
        MELLO_LOG_DEBUG(TAG, "Probing AMF decode... init function not found");
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    AMF_RESULT res = amf_init(AMF_FULL_VERSION, &factory_);
    if (res != AMF_OK || !factory_) {
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    res = factory_->CreateContext(&context_);
    if (res != AMF_OK) {
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    res = context_->InitDX11(device_.Get());
    if (res != AMF_OK) {
        context_.Release(); FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    const wchar_t* codec_id = (config.codec == VideoCodec::AV1)
        ? AMFVideoDecoderHW_AV1
        : AMFVideoDecoderUVD_H264_AVC;

    res = factory_->CreateComponent(context_, codec_id, &decoder_);
    if (res != AMF_OK) {
        MELLO_LOG_DEBUG(TAG, "Probing AMF decode... CreateComponent failed: %d", res);
        context_->Terminate(); context_.Release();
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    res = decoder_->Init(amf::AMF_SURFACE_NV12, config.width, config.height);
    if (res != AMF_OK) {
        MELLO_LOG_ERROR(TAG, "AMF decode: Init failed: %d", res);
        decoder_.Release(); context_->Terminate(); context_.Release();
        FreeLibrary(dll_); dll_ = nullptr;
        return false;
    }

    // Output texture
    D3D11_TEXTURE2D_DESC tex_desc{};
    tex_desc.Width  = config.width;
    tex_desc.Height = config.height;
    tex_desc.MipLevels = 1;
    tex_desc.ArraySize = 1;
    tex_desc.Format = DXGI_FORMAT_NV12;
    tex_desc.SampleDesc.Count = 1;
    tex_desc.Usage = D3D11_USAGE_DEFAULT;
    tex_desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;

    HRESULT hr = device_->CreateTexture2D(&tex_desc, nullptr, &frame_tex_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "AMF decode: Failed to create output texture: hr=0x%08X", hr);
        return false;
    }

    MELLO_LOG_DEBUG(TAG, "Probing AMF decode... ok");
    MELLO_LOG_INFO(TAG, "Selected decoder: AMF-Decode codec=%s resolution=%ux%u",
        config.codec == VideoCodec::H264 ? "H264" : "AV1",
        config.width, config.height);
    return true;
}

void AmfDecoder::shutdown() {
    if (decoder_) { decoder_->Terminate(); decoder_.Release(); }
    if (context_) { context_->Terminate(); context_.Release(); }
    factory_ = nullptr;
    frame_tex_.Reset();
    if (dll_) { FreeLibrary(dll_); dll_ = nullptr; }
}

DecodeFeedResult AmfDecoder::decode(const uint8_t* data, size_t size, bool is_keyframe) {
    if (!decoder_ || !context_) return DecodeFeedResult::Error;
    (void)is_keyframe;

    // Create AMF buffer from encoded data
    amf::AMFBufferPtr buffer;
    AMF_RESULT res = context_->AllocBuffer(amf::AMF_MEMORY_HOST, size, &buffer);
    if (res != AMF_OK || !buffer) {
        MELLO_LOG_ERROR(TAG, "AMF decode: AllocBuffer failed: %d", res);
        return DecodeFeedResult::Error;
    }

    memcpy(buffer->GetNative(), data, size);

    bool frame_ready = false;
    auto drain_output = [&]() -> bool {
        while (true) {
            amf::AMFDataPtr output;
            AMF_RESULT query_res = decoder_->QueryOutput(&output);
            if (query_res == AMF_REPEAT || (query_res == AMF_OK && !output)) return true;
            if (query_res != AMF_OK) {
                MELLO_LOG_ERROR(TAG, "AMF decode: QueryOutput failed: %d", query_res);
                return false;
            }

            // Unwrap AMF surface to D3D11 texture
            amf::AMFSurfacePtr surface(output);
            if (!surface) {
                MELLO_LOG_ERROR(TAG, "AMF decode: QueryOutput returned non-surface data");
                return false;
            }

            amf::AMFPlane* plane = surface->GetPlane(amf::AMF_PLANE_Y);
            if (!plane) {
                MELLO_LOG_ERROR(TAG, "AMF decode: output surface has no Y plane");
                return false;
            }

            // The AMF surface's native DX11 texture
            ID3D11Texture2D* amf_tex = static_cast<ID3D11Texture2D*>(plane->GetNative());
            if (!amf_tex) {
                MELLO_LOG_ERROR(TAG, "AMF decode: output surface has no DX11 texture");
                return false;
            }

            Microsoft::WRL::ComPtr<ID3D11DeviceContext> ctx;
            device_->GetImmediateContext(&ctx);
            ctx->CopyResource(frame_tex_.Get(), amf_tex);
            frame_ready = true;
        }
    };

    res = decoder_->SubmitInput(buffer);
    if (res == AMF_DECODER_NO_FREE_SURFACES) {
        if (!drain_output()) return DecodeFeedResult::Error;
        res = decoder_->SubmitInput(buffer);
    }
    if (res == AMF_DECODER_NO_FREE_SURFACES) {
        MELLO_LOG_ERROR(TAG, "AMF decode: no free surfaces after drain and retry");
        return DecodeFeedResult::Error;
    }
    if (res != AMF_OK) {
        MELLO_LOG_ERROR(TAG, "AMF decode: SubmitInput failed: %d", res);
        return DecodeFeedResult::Error;
    }

    if (!drain_output()) return DecodeFeedResult::Error;
    return frame_ready ? DecodeFeedResult::FrameReady : DecodeFeedResult::Accepted;
}

ID3D11Texture2D* AmfDecoder::get_frame() {
    return frame_tex_.Get();
}

bool AmfDecoder::supports_codec(VideoCodec codec) const {
    return codec == VideoCodec::H264 || codec == VideoCodec::AV1;
}

} // namespace mello::video
#endif
