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

    device_->GetImmediateContext(&d3d_context_);

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
    for (Slot& slot : slots_) slot = Slot{};
    d3d_context_.Reset();
    if (dll_) { FreeLibrary(dll_); dll_ = nullptr; }
}

// The array slice of an AMF surface texture (AMF texture pools are texture
// arrays). Private data on the texture under this GUID, as in the AMF SDK
// samples and FFmpeg's AMF code (AMFTextureArrayIndexGUID).
static const GUID kAmfTextureArrayIndexGuid = {
    0x28115527, 0xe7c3, 0x4b66, {0x99, 0xd3, 0x4f, 0x2a, 0xe6, 0xb4, 0x7f, 0xaf}};

bool AmfDecoder::copy_to_slot(amf::AMFSurface* surface, int slot_index) {
    amf::AMFPlane* plane = surface->GetPlane(amf::AMF_PLANE_Y);
    if (!plane) {
        MELLO_LOG_ERROR(TAG, "AMF decode: output surface has no Y plane");
        return false;
    }
    auto* amf_tex = static_cast<ID3D11Texture2D*>(plane->GetNative());
    if (!amf_tex) {
        MELLO_LOG_ERROR(TAG, "AMF decode: output surface has no DX11 texture");
        return false;
    }
    const uint32_t w = static_cast<uint32_t>(plane->GetWidth()) & ~1u;
    const uint32_t h = static_cast<uint32_t>(plane->GetHeight()) & ~1u;
    if (w == 0 || h == 0) return false;

    Slot& slot = slots_[slot_index];
    if (!slot.texture || slot.width != w || slot.height != h) {
        D3D11_TEXTURE2D_DESC tex_desc{};
        tex_desc.Width  = w;
        tex_desc.Height = h;
        tex_desc.MipLevels = 1;
        tex_desc.ArraySize = 1;
        tex_desc.Format = DXGI_FORMAT_NV12;
        tex_desc.SampleDesc.Count = 1;
        tex_desc.Usage = D3D11_USAGE_DEFAULT;
        tex_desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;
        Microsoft::WRL::ComPtr<ID3D11Texture2D> texture;
        HRESULT hr = device_->CreateTexture2D(&tex_desc, nullptr, &texture);
        if (FAILED(hr)) {
            MELLO_LOG_ERROR(TAG, "AMF decode: slot texture %ux%u failed: hr=0x%08X", w, h, hr);
            return false;
        }
        slot.texture = texture;
        slot.width = w;
        slot.height = h;
    }

    D3D11_TEXTURE2D_DESC src_desc{};
    amf_tex->GetDesc(&src_desc);
    UINT array_index = 0;
    UINT index_size = sizeof(array_index);
    if (src_desc.ArraySize > 1 &&
        FAILED(amf_tex->GetPrivateData(kAmfTextureArrayIndexGuid, &index_size, &array_index))) {
        array_index = 0;
    }
    D3D11_BOX box{0, 0, 0, w, h, 1};
    d3d_context_->CopySubresourceRegion(slot.texture.Get(), 0, 0, 0, 0, amf_tex,
                                    D3D11CalcSubresource(0, array_index, src_desc.MipLevels), &box);
    return true;
}

DecodeFeedResult AmfDecoder::decode(const uint8_t* data, size_t size, bool is_keyframe, int slot_index) {
    if (!decoder_ || !context_ || slot_index < 0 || slot_index >= kOutputSlots) {
        return DecodeFeedResult::Error;
    }
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
            amf::AMFSurfacePtr surface(output);
            if (!surface) {
                MELLO_LOG_ERROR(TAG, "AMF decode: QueryOutput returned non-surface data");
                return false;
            }
            // The newest picture wins the slot.
            if (!copy_to_slot(surface, slot_index)) return false;
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

bool AmfDecoder::picture(int slot_index, DecodedPicture& out) const {
    if (slot_index < 0 || slot_index >= kOutputSlots || !slots_[slot_index].texture) return false;
    const Slot& slot = slots_[slot_index];
    out.texture = slot.texture;
    out.format = DXGI_FORMAT_NV12;
    out.width = slot.width;
    out.height = slot.height;
    out.uv_row = 0;
    return true;
}

bool AmfDecoder::supports_codec(VideoCodec codec) const {
    return codec == VideoCodec::H264 || codec == VideoCodec::AV1;
}

} // namespace mello::video
#endif
