#ifdef _WIN32
#include "decoder_nvdec.hpp"
#include "nvdec_host_layout.hpp"
#include "../util/log.hpp"
#include <Windows.h>
#include <dxgi.h>

namespace mello::video {

static constexpr const char* TAG = "video/decoder";

static HMODULE load_nvcuvid_dll() {
    return LoadLibraryA("nvcuvid.dll");
}

static HMODULE load_cuda_dll() {
    return LoadLibraryA("nvcuda.dll");
}

// CUDA_MEMCPY2D layout (no cuda.h available — matched to CUDA driver API struct)
struct CudaMemcpy2D {
    size_t            srcXInBytes;
    size_t            srcY;
    unsigned int      srcMemoryType; // 1=HOST, 2=DEVICE, 3=ARRAY
    const void*       srcHost;
    unsigned long long srcDevice;
    void*             srcArray;
    size_t            srcPitch;
    size_t            dstXInBytes;
    size_t            dstY;
    unsigned int      dstMemoryType;
    void*             dstHost;
    unsigned long long dstDevice;
    void*             dstArray;
    size_t            dstPitch;
    size_t            WidthInBytes;
    size_t            Height;
};

constexpr unsigned int kMemoryTypeDevice = 2;
constexpr unsigned int kMemoryTypeArray = 3;
constexpr unsigned int kRegisterFlagsNone = 0;

bool NvdecDecoder::is_available() {
    HMODULE cuvid = load_nvcuvid_dll();
    HMODULE cuda  = load_cuda_dll();
    bool ok = (cuvid != nullptr && cuda != nullptr);
    if (cuvid) FreeLibrary(cuvid);
    if (cuda)  FreeLibrary(cuda);
    return ok;
}

bool NvdecDecoder::load_cuda_entry_points() {
    cuCtxPush_ = reinterpret_cast<CuCtxPushCurrent_t>(GetProcAddress(cuda_dll_, "cuCtxPushCurrent_v2"));
    cuCtxPop_  = reinterpret_cast<CuCtxPopCurrent_t>(GetProcAddress(cuda_dll_, "cuCtxPopCurrent_v2"));
    cuRegister_ = reinterpret_cast<CuGraphicsD3D11RegisterResource_t>(
        GetProcAddress(cuda_dll_, "cuGraphicsD3D11RegisterResource"));
    cuUnregister_ = reinterpret_cast<CuGraphicsUnregisterResource_t>(
        GetProcAddress(cuda_dll_, "cuGraphicsUnregisterResource"));
    cuMap_ = reinterpret_cast<CuGraphicsMapResources_t>(
        GetProcAddress(cuda_dll_, "cuGraphicsMapResources"));
    cuUnmap_ = reinterpret_cast<CuGraphicsUnmapResources_t>(
        GetProcAddress(cuda_dll_, "cuGraphicsUnmapResources"));
    cuMappedArray_ = reinterpret_cast<CuGraphicsSubResourceGetMappedArray_t>(
        GetProcAddress(cuda_dll_, "cuGraphicsSubResourceGetMappedArray"));
    cuMemcpy2D_ = reinterpret_cast<CuMemcpy2D_t>(GetProcAddress(cuda_dll_, "cuMemcpy2D_v2"));
    return cuCtxPush_ && cuCtxPop_ && cuRegister_ && cuUnregister_ && cuMap_ && cuUnmap_ &&
           cuMappedArray_ && cuMemcpy2D_;
}

bool NvdecDecoder::initialize(const GraphicsDevice& device, const DecoderConfig& config) {
    device_ = device.d3d11();
    config_ = config;

    cuda_dll_  = load_cuda_dll();
    cuvid_dll_ = load_nvcuvid_dll();
    auto fail = [this](const char* why) {
        MELLO_LOG_DEBUG(TAG, "Probing NVDEC... %s", why);
        shutdown();
        return false;
    };
    if (!cuda_dll_ || !cuvid_dll_) return fail("not available (DLLs not found)");

    auto cuInit      = reinterpret_cast<CuInit_t>(GetProcAddress(cuda_dll_, "cuInit"));
    auto cuDeviceGet = reinterpret_cast<CuDeviceGet_t>(GetProcAddress(cuda_dll_, "cuDeviceGet"));
    auto cuCtxCreate = reinterpret_cast<CuCtxCreate_t>(GetProcAddress(cuda_dll_, "cuCtxCreate_v2"));
    auto cuD3D11GetDevice =
        reinterpret_cast<CuD3D11GetDevice_t>(GetProcAddress(cuda_dll_, "cuD3D11GetDevice"));
    if (!cuInit || !cuDeviceGet || !cuCtxCreate || !load_cuda_entry_points()) {
        return fail("CUDA entry points not found");
    }
    if (cuInit(0) != 0) return fail("cuInit failed");

    // The CUDA device of libmello's adapter: interop registers textures of
    // that adapter, which a context on another GPU cannot see.
    int cu_device = 0;
    bool matched = false;
    if (cuD3D11GetDevice) {
        Microsoft::WRL::ComPtr<IDXGIDevice> dxgi_device;
        Microsoft::WRL::ComPtr<IDXGIAdapter> adapter;
        if (SUCCEEDED(device_.As(&dxgi_device)) && SUCCEEDED(dxgi_device->GetAdapter(&adapter)) &&
            cuD3D11GetDevice(&cu_device, adapter.Get()) == 0) {
            matched = true;
        }
    }
    if (!matched && cuDeviceGet(&cu_device, 0) != 0) return fail("cuDeviceGet failed");
    if (cuCtxCreate(&cu_context_, 0, cu_device) != 0) {
        cu_context_ = nullptr;
        return fail("cuCtxCreate failed (no CUDA device on the video adapter)");
    }

    CUVIDPARSERPARAMS parser_params{};
    parser_params.CodecType           = (config.codec == VideoCodec::AV1) ? cudaVideoCodec_AV1 : cudaVideoCodec_H264;
    parser_params.ulMaxNumDecodeSurfaces = 8;
    parser_params.ulMaxDisplayDelay      = 0; // Low latency: display at once
    parser_params.pUserData              = this;
    parser_params.pfnSequenceCallback    = handle_video_sequence;
    parser_params.pfnDecodePicture       = handle_picture_decode;
    parser_params.pfnDisplayPicture      = handle_picture_display;

    auto cuvidCreateVideoParser_fn = reinterpret_cast<decltype(&cuvidCreateVideoParser)>(
        GetProcAddress(cuvid_dll_, "cuvidCreateVideoParser"));
    if (!cuvidCreateVideoParser_fn || cuvidCreateVideoParser_fn(&parser_, &parser_params) != CUDA_SUCCESS) {
        parser_ = nullptr;
        return fail("cuvidCreateVideoParser failed");
    }

    MELLO_LOG_DEBUG(TAG, "Probing NVDEC... ok");
    MELLO_LOG_INFO(TAG, "Selected decoder: NVDEC codec=%s resolution=%ux%u interop=cuda-d3d11 device=%s",
        config.codec == VideoCodec::H264 ? "H264" : "AV1",
        config.width, config.height, matched ? "adapter" : "0");
    return true;
}

void NvdecDecoder::release_slot(Slot& slot) {
    if (slot.resource && cuUnregister_) {
        bool pushed = cuCtxPush_ && cu_context_ && cuCtxPush_(cu_context_) == CUDA_SUCCESS;
        cuUnregister_(slot.resource);
        if (pushed) { void* dummy = nullptr; cuCtxPop_(&dummy); }
    }
    slot = Slot{};
}

void NvdecDecoder::shutdown() {
    for (Slot& slot : slots_) release_slot(slot);
    if (parser_ && cuvid_dll_) {
        auto fn = reinterpret_cast<decltype(&cuvidDestroyVideoParser)>(
            GetProcAddress(cuvid_dll_, "cuvidDestroyVideoParser"));
        if (fn) fn(parser_);
    }
    parser_ = nullptr;
    if (decoder_ && cuvid_dll_) {
        auto fn = reinterpret_cast<decltype(&cuvidDestroyDecoder)>(
            GetProcAddress(cuvid_dll_, "cuvidDestroyDecoder"));
        if (fn) fn(decoder_);
    }
    decoder_ = nullptr;
    if (cu_context_ && cuda_dll_) {
        auto cuCtxDestroy = reinterpret_cast<CuCtxDestroy_t>(GetProcAddress(cuda_dll_, "cuCtxDestroy_v2"));
        if (cuCtxDestroy) cuCtxDestroy(cu_context_);
    }
    cu_context_ = nullptr;
    if (cuvid_dll_) { FreeLibrary(cuvid_dll_); cuvid_dll_ = nullptr; }
    if (cuda_dll_)  { FreeLibrary(cuda_dll_);  cuda_dll_ = nullptr; }
    pending_display_.clear();
    display_width_ = 0;
    display_height_ = 0;
    decoder_coded_width_ = 0;
    decoder_coded_height_ = 0;
    decoder_codec_ = -1;
    decoder_chroma_format_ = -1;
    sequence_callback_count_ = 0;
}

// A slot texture at the picture size, registered with CUDA. A new size (a
// new SPS) makes a new texture; the old one lives on in the viewer ring
// while the present thread still holds it.
bool NvdecDecoder::ensure_slot(int index, uint32_t width, uint32_t height) {
    Slot& slot = slots_[index];
    if (slot.texture && slot.width == width && slot.height == height) return true;
    release_slot(slot);

    D3D11_TEXTURE2D_DESC desc{};
    desc.Width  = width;
    desc.Height = height + height / 2;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = DXGI_FORMAT_R8_UNORM;
    desc.SampleDesc.Count = 1;
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    HRESULT hr = device_->CreateTexture2D(&desc, nullptr, &slot.texture);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "NVDEC: slot texture %ux%u failed: hr=0x%08X", width, height, hr);
        slot = Slot{};
        return false;
    }
    const int rc = cuRegister_(&slot.resource, slot.texture.Get(), kRegisterFlagsNone);
    if (rc != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuGraphicsD3D11RegisterResource failed: %d", rc);
        slot = Slot{};
        return false;
    }
    slot.width = width;
    slot.height = height;
    MELLO_LOG_INFO(TAG, "NVDEC: slot %d is %ux%u (R8 NV12 layout, CUDA interop)", index, width, height);
    return true;
}

DecodeFeedResult NvdecDecoder::decode(const uint8_t* data, size_t size, bool is_keyframe, int slot) {
    (void)is_keyframe;
    if (!parser_ || slot < 0 || slot >= kOutputSlots) return DecodeFeedResult::Error;

    // The CUDA context is current for the parser callbacks and the copy.
    if (cuCtxPush_(cu_context_) != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuCtxPushCurrent failed");
        return DecodeFeedResult::Error;
    }

    pending_display_.clear();
    CUVIDSOURCEDATAPACKET packet{};
    packet.payload      = data;
    packet.payload_size = static_cast<unsigned long>(size);
    packet.flags        = CUVID_PKT_TIMESTAMP | CUVID_PKT_ENDOFPICTURE;
    packet.timestamp    = 0;

    auto cuvidParseVideoData_fn = reinterpret_cast<decltype(&cuvidParseVideoData)>(
        GetProcAddress(cuvid_dll_, "cuvidParseVideoData"));
    CUresult res = cuvidParseVideoData_fn ? cuvidParseVideoData_fn(parser_, &packet) : -1;

    bool frame_ready = false;
    bool copy_failed = false;
    if (res == CUDA_SUCCESS && !pending_display_.empty()) {
        // The newest surface wins the slot.
        const CUVIDPARSERDISPINFO disp = pending_display_.back();
        if (copy_to_slot(disp.picture_index, slot)) {
            frame_ready = true;
        } else {
            copy_failed = true;
        }
    }
    pending_display_.clear();

    void* dummy = nullptr;
    const int pop_result = cuCtxPop_(&dummy);

    if (res != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuvidParseVideoData failed: %d", res);
        return DecodeFeedResult::Error;
    }
    if (pop_result != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuCtxPopCurrent failed: %d", pop_result);
        return DecodeFeedResult::Error;
    }
    if (copy_failed) return DecodeFeedResult::Error;
    return frame_ready ? DecodeFeedResult::FrameReady : DecodeFeedResult::Accepted;
}

// CUDA surface to the slot texture, on the GPU. The CUDA context is current.
bool NvdecDecoder::copy_to_slot(int picture_index, int slot_index) {
    if (!decoder_ || display_width_ == 0 || display_height_ == 0) return false;
    const uint32_t w = display_width_ & ~1u;
    const uint32_t h = display_height_ & ~1u;
    if (!ensure_slot(slot_index, w, h)) return false;
    Slot& slot = slots_[slot_index];

    auto map_frame = reinterpret_cast<decltype(&cuvidMapVideoFrame64)>(
        GetProcAddress(cuvid_dll_, "cuvidMapVideoFrame64"));
    auto unmap_frame = reinterpret_cast<decltype(&cuvidUnmapVideoFrame64)>(
        GetProcAddress(cuvid_dll_, "cuvidUnmapVideoFrame64"));
    if (!map_frame || !unmap_frame) return false;

    CUVIDPROCPARAMS proc{};
    proc.progressive_frame = 1;
    unsigned int pitch = 0;
    unsigned long long dev_ptr = 0;
    if (map_frame(decoder_, picture_index, &dev_ptr, &pitch, &proc) != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuvidMapVideoFrame failed");
        return false;
    }

    bool ok = false;
    void* resource = slot.resource;
    if (cuMap_(1, &resource, nullptr) == CUDA_SUCCESS) {
        void* array = nullptr;
        if (cuMappedArray_(&array, resource, 0, 0) == CUDA_SUCCESS && array) {
            // Luma rows from row 0, chroma rows from the surface's coded
            // height into the slot's chroma rows (nvdec_host_layout).
            const NvdecHostCopy plan = plan_nvdec_host_copy(w, h, decoder_coded_height_);
            const size_t copy_w = (w <= pitch) ? w : pitch;
            auto copy_rows = [&](uint32_t src_row, uint32_t dst_row, uint32_t rows) {
                CudaMemcpy2D cp{};
                cp.srcMemoryType = kMemoryTypeDevice;
                cp.srcDevice     = dev_ptr;
                cp.srcY          = src_row;
                cp.srcPitch      = pitch;
                cp.dstMemoryType = kMemoryTypeArray;
                cp.dstArray      = array;
                cp.dstY          = dst_row;
                cp.WidthInBytes  = copy_w;
                cp.Height        = rows;
                return cuMemcpy2D_(&cp);
            };
            int rc = copy_rows(0, 0, plan.luma_rows);
            if (rc == 0) rc = copy_rows(plan.chroma_src_row, plan.chroma_dst_row, plan.chroma_rows);
            if (rc != 0) {
                MELLO_LOG_ERROR(TAG, "NVDEC: CUDA copy into the slot texture failed: %d", rc);
            } else {
                ok = true;
            }
        } else {
            MELLO_LOG_ERROR(TAG, "NVDEC: cuGraphicsSubResourceGetMappedArray failed");
        }
        // Unmap orders the copy before any later D3D11 use of the texture.
        cuUnmap_(1, &resource, nullptr);
    } else {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuGraphicsMapResources failed");
    }

    if (unmap_frame(decoder_, dev_ptr) != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuvidUnmapVideoFrame failed");
        return false;
    }
    return ok;
}

int CUDAAPI NvdecDecoder::handle_video_sequence(void* user, CUVIDEOFORMAT* fmt) {
    auto* self = static_cast<NvdecDecoder*>(user);
    self->sequence_callback_count_++;

    const uint32_t display_w = static_cast<uint32_t>(fmt->display_area.right - fmt->display_area.left);
    const uint32_t display_h = static_cast<uint32_t>(fmt->display_area.bottom - fmt->display_area.top);
    self->display_width_ = display_w > 0 ? display_w : fmt->coded_width;
    self->display_height_ = display_h > 0 ? display_h : fmt->coded_height;

    const bool decoder_params_unchanged = self->decoder_
        && self->decoder_coded_width_ == fmt->coded_width
        && self->decoder_coded_height_ == fmt->coded_height
        && self->decoder_codec_ == static_cast<int>(fmt->codec)
        && self->decoder_chroma_format_ == static_cast<int>(fmt->chroma_format);
    if (decoder_params_unchanged) {
        return 1;
    }
    MELLO_LOG_INFO(
        TAG,
        "NVDEC sequence callback #%llu: (re)create decoder coded=%ux%u display=%ux%u codec=%d chroma=%d",
        self->sequence_callback_count_,
        fmt->coded_width,
        fmt->coded_height,
        self->display_width_,
        self->display_height_,
        static_cast<int>(fmt->codec),
        static_cast<int>(fmt->chroma_format)
    );

    CUVIDDECODECREATEINFO create_info{};
    create_info.CodecType   = fmt->codec;
    create_info.ChromaFormat = fmt->chroma_format;
    create_info.OutputFormat = cudaVideoSurfaceFormat_NV12;
    create_info.ulWidth      = fmt->coded_width;
    create_info.ulHeight     = fmt->coded_height;
    create_info.ulTargetWidth  = fmt->coded_width;
    create_info.ulTargetHeight = fmt->coded_height;
    create_info.ulNumDecodeSurfaces = 8;
    create_info.ulNumOutputSurfaces = 2;
    create_info.DeinterlaceMode     = cudaVideoDeinterlaceMode_Weave;

    // A picture of the old sequence the parser showed in this parse call
    // (the last one before the new SPS) indexes a surface of the decoder
    // that goes now. Mapped on the new decoder, the index is another
    // surface: a broken picture at the new size. It is not shown.
    if (!self->pending_display_.empty()) {
        MELLO_LOG_INFO(TAG, "NVDEC: %zu picture(s) of the old sequence not shown at the size change",
                       self->pending_display_.size());
        self->pending_display_.clear();
    }

    if (self->decoder_) {
        auto fn = reinterpret_cast<decltype(&cuvidDestroyDecoder)>(
            GetProcAddress(self->cuvid_dll_, "cuvidDestroyDecoder"));
        if (fn) fn(self->decoder_);
        self->decoder_ = nullptr;
    }

    auto cuvidCreateDecoder_fn = reinterpret_cast<decltype(&cuvidCreateDecoder)>(
        GetProcAddress(self->cuvid_dll_, "cuvidCreateDecoder"));
    if (!cuvidCreateDecoder_fn) return 0;

    CUresult rc = cuvidCreateDecoder_fn(&self->decoder_, &create_info);
    if (rc != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuvidCreateDecoder failed: %d (coded=%ux%u)",
            rc, fmt->coded_width, fmt->coded_height);
        self->decoder_ = nullptr;
        return 0;
    }
    self->decoder_coded_width_ = fmt->coded_width;
    self->decoder_coded_height_ = fmt->coded_height;
    self->decoder_codec_ = static_cast<int>(fmt->codec);
    self->decoder_chroma_format_ = static_cast<int>(fmt->chroma_format);
    // ulNumDecodeSurfaces: the parser needs at least this many.
    return 8;
}

int CUDAAPI NvdecDecoder::handle_picture_decode(void* user, CUVIDPICPARAMS* pic) {
    auto* self = static_cast<NvdecDecoder*>(user);
    if (!self->decoder_) return 0;

    auto cuvidDecodePicture_fn = reinterpret_cast<decltype(&cuvidDecodePicture)>(
        GetProcAddress(self->cuvid_dll_, "cuvidDecodePicture"));
    if (!cuvidDecodePicture_fn) return 0;

    CUresult res = cuvidDecodePicture_fn(self->decoder_, pic);
    if (res != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuvidDecodePicture failed: %d", res);
        return 0;
    }
    return 1;
}

int CUDAAPI NvdecDecoder::handle_picture_display(void* user, CUVIDPARSERDISPINFO* disp) {
    auto* self = static_cast<NvdecDecoder*>(user);
    if (!self->decoder_ || !disp) return 0;
    // Only noted here; decode() copies it after the parse call returns.
    self->pending_display_.push_back(*disp);
    return 1;
}

bool NvdecDecoder::picture(int slot_index, DecodedPicture& out) const {
    if (slot_index < 0 || slot_index >= kOutputSlots || !slots_[slot_index].texture) return false;
    const Slot& slot = slots_[slot_index];
    out.texture = slot.texture;
    out.format = DXGI_FORMAT_R8_UNORM;
    out.width = slot.width;
    out.height = slot.height;
    out.uv_row = slot.height;
    return true;
}

bool NvdecDecoder::supports_codec(VideoCodec codec) const {
    return codec == VideoCodec::H264 || codec == VideoCodec::AV1;
}

} // namespace mello::video
#endif
