#ifdef _WIN32
#include "decoder_nvdec.hpp"
#include "../util/log.hpp"
#include <Windows.h>

namespace mello::video {

static constexpr const char* TAG = "video/decoder";

static HMODULE load_nvcuvid_dll() {
    return LoadLibraryA("nvcuvid.dll");
}

static HMODULE load_cuda_dll() {
    HMODULE dll = LoadLibraryA("nvcuda.dll");
    return dll;
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

typedef int (*CuMemcpy2D_t)(const CudaMemcpy2D*);
typedef int (*CuGfxD3D11Register_t)(void** resource, void* d3dResource, unsigned int flags);
typedef int (*CuGfxMapResources_t)(unsigned int count, void** resources, void* stream);
typedef int (*CuGfxUnmapResources_t)(unsigned int count, void** resources, void* stream);
typedef int (*CuGfxGetMappedArray_t)(void** array, void* resource, unsigned int arrayIndex, unsigned int mipLevel);
typedef int (*CuGfxUnregister_t)(void* resource);

bool NvdecDecoder::is_available() {
    HMODULE cuvid = load_nvcuvid_dll();
    HMODULE cuda  = load_cuda_dll();
    bool ok = (cuvid != nullptr && cuda != nullptr);
    if (cuvid) FreeLibrary(cuvid);
    if (cuda)  FreeLibrary(cuda);
    return ok;
}

bool NvdecDecoder::initialize(const GraphicsDevice& device, const DecoderConfig& config) {
    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);
    config_ = config;

    cuda_dll_  = load_cuda_dll();
    cuvid_dll_ = load_nvcuvid_dll();
    if (!cuda_dll_ || !cuvid_dll_) {
        MELLO_LOG_DEBUG(TAG, "Probing NVDEC... not available (DLLs not found)");
        if (cuda_dll_)  { FreeLibrary(cuda_dll_);  cuda_dll_ = nullptr; }
        if (cuvid_dll_) { FreeLibrary(cuvid_dll_); cuvid_dll_ = nullptr; }
        return false;
    }

    // Initialize CUDA context
    auto cuInit      = reinterpret_cast<CuInit_t>(GetProcAddress(cuda_dll_, "cuInit"));
    auto cuDeviceGet = reinterpret_cast<CuDeviceGet_t>(GetProcAddress(cuda_dll_, "cuDeviceGet"));
    auto cuCtxCreate = reinterpret_cast<CuCtxCreate_t>(GetProcAddress(cuda_dll_, "cuCtxCreate_v2"));

    if (!cuInit || !cuDeviceGet || !cuCtxCreate) {
        MELLO_LOG_DEBUG(TAG, "Probing NVDEC... CUDA entry points not found");
        FreeLibrary(cuda_dll_);  cuda_dll_ = nullptr;
        FreeLibrary(cuvid_dll_); cuvid_dll_ = nullptr;
        return false;
    }

    if (cuInit(0) != 0) {
        MELLO_LOG_DEBUG(TAG, "Probing NVDEC... cuInit failed");
        FreeLibrary(cuda_dll_);  cuda_dll_ = nullptr;
        FreeLibrary(cuvid_dll_); cuvid_dll_ = nullptr;
        return false;
    }

    int cu_device = 0;
    if (cuDeviceGet(&cu_device, 0) != 0) {
        MELLO_LOG_DEBUG(TAG, "Probing NVDEC... cuDeviceGet failed");
        FreeLibrary(cuda_dll_);  cuda_dll_ = nullptr;
        FreeLibrary(cuvid_dll_); cuvid_dll_ = nullptr;
        return false;
    }

    if (cuCtxCreate(&cu_context_, 0, cu_device) != 0) {
        MELLO_LOG_DEBUG(TAG, "Probing NVDEC... cuCtxCreate failed");
        FreeLibrary(cuda_dll_);  cuda_dll_ = nullptr;
        FreeLibrary(cuvid_dll_); cuvid_dll_ = nullptr;
        return false;
    }

    // Create video parser
    CUVIDPARSERPARAMS parser_params{};
    parser_params.CodecType           = (config.codec == VideoCodec::AV1) ? cudaVideoCodec_AV1 : cudaVideoCodec_H264;
    parser_params.ulMaxNumDecodeSurfaces = 4;
    parser_params.ulMaxDisplayDelay      = 0; // Low-latency: display immediately
    parser_params.pUserData              = this;
    parser_params.pfnSequenceCallback    = handle_video_sequence;
    parser_params.pfnDecodePicture       = handle_picture_decode;
    parser_params.pfnDisplayPicture      = handle_picture_display;

    auto cuvidCreateVideoParser_fn = reinterpret_cast<decltype(&cuvidCreateVideoParser)>(
        GetProcAddress(cuvid_dll_, "cuvidCreateVideoParser"));
    if (!cuvidCreateVideoParser_fn || cuvidCreateVideoParser_fn(&parser_, &parser_params) != CUDA_SUCCESS) {
        MELLO_LOG_DEBUG(TAG, "Probing NVDEC... cuvidCreateVideoParser failed");
        auto cuCtxDestroy = reinterpret_cast<CuCtxDestroy_t>(GetProcAddress(cuda_dll_, "cuCtxDestroy_v2"));
        if (cuCtxDestroy) cuCtxDestroy(cu_context_);
        cu_context_ = nullptr;
        FreeLibrary(cuda_dll_);  cuda_dll_ = nullptr;
        FreeLibrary(cuvid_dll_); cuvid_dll_ = nullptr;
        return false;
    }

    // CUDA can't interop with NV12 planar textures. Instead, use R8_UNORM
    // sized w × (h*3/2) — same memory layout as NV12 (Y then UV), but a
    // single-plane format CUDA can register and copy to directly.
    auto cuGfxRegister = reinterpret_cast<CuGfxD3D11Register_t>(
        GetProcAddress(cuda_dll_, "cuGraphicsD3D11RegisterResource"));

    if (cuGfxRegister) {
        D3D11_TEXTURE2D_DESC tex_desc{};
        tex_desc.Width  = config.width;
        tex_desc.Height = config.height + config.height / 2; // Y + UV
        tex_desc.MipLevels = 1;
        tex_desc.ArraySize = 1;
        tex_desc.Format = DXGI_FORMAT_R8_UNORM;
        tex_desc.SampleDesc.Count = 1;
        tex_desc.Usage = D3D11_USAGE_DEFAULT;
        tex_desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;

        HRESULT hr = device_->CreateTexture2D(&tex_desc, nullptr, &frame_tex_);
        if (SUCCEEDED(hr)) {
            int rc = cuGfxRegister(&cuda_gfx_resource_, frame_tex_.Get(), 0);
            if (rc == 0 && cuda_gfx_resource_) {
                use_interop_ = true;
                MELLO_LOG_INFO(TAG, "NVDEC: CUDA-D3D11 interop enabled (zero-copy R8 %ux%u)",
                    tex_desc.Width, tex_desc.Height);
            } else {
                MELLO_LOG_WARN(TAG, "NVDEC: cuGraphicsD3D11RegisterResource failed (%d), trying NV12 fallback", rc);
                cuda_gfx_resource_ = nullptr;
                frame_tex_.Reset();
            }
        } else {
            MELLO_LOG_WARN(TAG, "NVDEC: R8 texture creation failed (0x%08X), trying NV12 fallback", hr);
        }
    }

    if (!use_interop_) {
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
            MELLO_LOG_ERROR(TAG, "NVDEC: Failed to create output texture: hr=0x%08X", hr);
            return false;
        }
        nv12_buf_.resize(static_cast<size_t>(config.width) * config.height * 3 / 2);
    }

    MELLO_LOG_DEBUG(TAG, "Probing NVDEC... ok");
    MELLO_LOG_INFO(TAG, "Selected decoder: NVDEC codec=%s resolution=%ux%u interop=%s",
        config.codec == VideoCodec::H264 ? "H264" : "AV1",
        config.width, config.height,
        use_interop_ ? "true" : "false");
    return true;
}

void NvdecDecoder::shutdown() {
    if (parser_) {
        auto cuvidDestroyVideoParser_fn = reinterpret_cast<decltype(&cuvidDestroyVideoParser)>(
            GetProcAddress(cuvid_dll_, "cuvidDestroyVideoParser"));
        if (cuvidDestroyVideoParser_fn) cuvidDestroyVideoParser_fn(parser_);
        parser_ = nullptr;
    }
    if (decoder_) {
        auto cuvidDestroyDecoder_fn = reinterpret_cast<decltype(&cuvidDestroyDecoder)>(
            GetProcAddress(cuvid_dll_, "cuvidDestroyDecoder"));
        if (cuvidDestroyDecoder_fn) cuvidDestroyDecoder_fn(decoder_);
        decoder_ = nullptr;
    }
    if (cuda_gfx_resource_) {
        auto cuGfxUnreg = reinterpret_cast<CuGfxUnregister_t>(
            GetProcAddress(cuda_dll_, "cuGraphicsUnregisterResource"));
        if (cuGfxUnreg) cuGfxUnreg(cuda_gfx_resource_);
        cuda_gfx_resource_ = nullptr;
        use_interop_ = false;
    }
    if (cu_context_) {
        auto cuCtxDestroy = reinterpret_cast<CuCtxDestroy_t>(GetProcAddress(cuda_dll_, "cuCtxDestroy_v2"));
        if (cuCtxDestroy) cuCtxDestroy(cu_context_);
        cu_context_ = nullptr;
    }
    if (cuvid_dll_) { FreeLibrary(cuvid_dll_); cuvid_dll_ = nullptr; }
    if (cuda_dll_)  { FreeLibrary(cuda_dll_);  cuda_dll_ = nullptr; }
    frame_tex_.Reset();
    nv12_buf_.clear();
}

bool NvdecDecoder::decode(const uint8_t* data, size_t size, bool is_keyframe) {
    if (!parser_) return false;
    (void)is_keyframe;

    frame_ready_ = false;

    CUVIDSOURCEDATAPACKET packet{};
    packet.payload      = data;
    packet.payload_size = static_cast<unsigned long>(size);
    packet.flags        = CUVID_PKT_TIMESTAMP;
    packet.timestamp    = 0;

    auto cuvidParseVideoData_fn = reinterpret_cast<decltype(&cuvidParseVideoData)>(
        GetProcAddress(cuvid_dll_, "cuvidParseVideoData"));
    if (!cuvidParseVideoData_fn) return false;

    CUresult res = cuvidParseVideoData_fn(parser_, &packet);
    if (res != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuvidParseVideoData failed: %d", res);
        return false;
    }

    return frame_ready_;
}

int CUDAAPI NvdecDecoder::handle_video_sequence(void* user, CUVIDEOFORMAT* fmt) {
    auto* self = static_cast<NvdecDecoder*>(user);

    CUVIDDECODECREATEINFO create_info{};
    create_info.CodecType   = fmt->codec;
    create_info.ChromaFormat = fmt->chroma_format;
    create_info.OutputFormat = cudaVideoSurfaceFormat_NV12;
    create_info.ulWidth      = fmt->coded_width;
    create_info.ulHeight     = fmt->coded_height;
    create_info.ulTargetWidth  = fmt->coded_width;
    create_info.ulTargetHeight = fmt->coded_height;
    create_info.ulNumDecodeSurfaces = 4;
    create_info.ulNumOutputSurfaces = 1;
    create_info.DeinterlaceMode     = cudaVideoDeinterlaceMode_Weave;

    if (self->decoder_) {
        auto cuvidDestroyDecoder_fn = reinterpret_cast<decltype(&cuvidDestroyDecoder)>(
            GetProcAddress(self->cuvid_dll_, "cuvidDestroyDecoder"));
        if (cuvidDestroyDecoder_fn) cuvidDestroyDecoder_fn(self->decoder_);
        self->decoder_ = nullptr;
    }

    auto cuvidCreateDecoder_fn = reinterpret_cast<decltype(&cuvidCreateDecoder)>(
        GetProcAddress(self->cuvid_dll_, "cuvidCreateDecoder"));
    if (cuvidCreateDecoder_fn) {
        cuvidCreateDecoder_fn(&self->decoder_, &create_info);
    }

    return 1; // Return number of decode surfaces
}

int CUDAAPI NvdecDecoder::handle_picture_decode(void* user, CUVIDPICPARAMS* pic) {
    auto* self = static_cast<NvdecDecoder*>(user);
    if (!self->decoder_) return 0;

    auto cuvidDecodePicture_fn = reinterpret_cast<decltype(&cuvidDecodePicture)>(
        GetProcAddress(self->cuvid_dll_, "cuvidDecodePicture"));
    if (cuvidDecodePicture_fn) {
        cuvidDecodePicture_fn(self->decoder_, pic);
    }
    return 1;
}

int CUDAAPI NvdecDecoder::handle_picture_display(void* user, CUVIDPARSERDISPINFO* disp) {
    auto* self = static_cast<NvdecDecoder*>(user);
    if (!self->decoder_) return 0;

    CUVIDPROCPARAMS proc{};
    proc.progressive_frame = disp->progressive_frame;
    proc.top_field_first   = disp->top_field_first;

    unsigned int pitch = 0;
    unsigned long long dev_ptr = 0;

    auto cuvidMapVideoFrame_fn = reinterpret_cast<decltype(&cuvidMapVideoFrame64)>(
        GetProcAddress(self->cuvid_dll_, "cuvidMapVideoFrame64"));
    auto cuvidUnmapVideoFrame_fn = reinterpret_cast<decltype(&cuvidUnmapVideoFrame64)>(
        GetProcAddress(self->cuvid_dll_, "cuvidUnmapVideoFrame64"));

    if (!cuvidMapVideoFrame_fn || !cuvidUnmapVideoFrame_fn) return 0;

    CUresult res = cuvidMapVideoFrame_fn(self->decoder_, disp->picture_index,
                                          &dev_ptr, &pitch, &proc);
    if (res != CUDA_SUCCESS) return 0;

    uint32_t w = self->config_.width;
    uint32_t h = self->config_.height;

    if (self->use_interop_) {
        // Zero-copy: NVDEC CUDA memory → R8_UNORM frame_tex_ (single copy, GPU-only)
        auto cuGfxMap = reinterpret_cast<CuGfxMapResources_t>(
            GetProcAddress(self->cuda_dll_, "cuGraphicsMapResources"));
        auto cuGfxUnmap = reinterpret_cast<CuGfxUnmapResources_t>(
            GetProcAddress(self->cuda_dll_, "cuGraphicsUnmapResources"));
        auto cuGfxGetArray = reinterpret_cast<CuGfxGetMappedArray_t>(
            GetProcAddress(self->cuda_dll_, "cuGraphicsSubResourceGetMappedArray"));
        auto cuMemcpy2D_fn = reinterpret_cast<CuMemcpy2D_t>(
            GetProcAddress(self->cuda_dll_, "cuMemcpy2D_v2"));

        if (cuGfxMap && cuGfxUnmap && cuGfxGetArray && cuMemcpy2D_fn) {
            cuGfxMap(1, &self->cuda_gfx_resource_, nullptr);

            void* array = nullptr;
            cuGfxGetArray(&array, self->cuda_gfx_resource_, 0, 0);
            if (array) {
                CudaMemcpy2D cp{};
                cp.srcMemoryType = 2; // CU_MEMORYTYPE_DEVICE
                cp.srcDevice     = dev_ptr;
                cp.srcPitch      = pitch;
                cp.dstMemoryType = 3; // CU_MEMORYTYPE_ARRAY
                cp.dstArray      = array;
                cp.WidthInBytes  = w;
                cp.Height        = h + h / 2;
                cuMemcpy2D_fn(&cp);
            }

            cuGfxUnmap(1, &self->cuda_gfx_resource_, nullptr);
        }
    } else {
        // Fallback: CUDA → CPU → D3D11 (two PCIe crossings)
        auto cuMemcpy2D_fn = reinterpret_cast<CuMemcpy2D_t>(
            GetProcAddress(self->cuda_dll_, "cuMemcpy2D_v2"));
        if (cuMemcpy2D_fn) {
            CudaMemcpy2D cp{};
            cp.srcMemoryType = 2;
            cp.srcDevice     = dev_ptr;
            cp.srcPitch      = pitch;
            cp.dstMemoryType = 1; // CU_MEMORYTYPE_HOST
            cp.dstHost       = self->nv12_buf_.data();
            cp.dstPitch      = w;
            cp.WidthInBytes  = w;
            cp.Height        = h + h / 2;
            cuMemcpy2D_fn(&cp);

            self->context_->UpdateSubresource(
                self->frame_tex_.Get(), 0, nullptr,
                self->nv12_buf_.data(), w, 0);
        }
    }

    cuvidUnmapVideoFrame_fn(self->decoder_, dev_ptr);
    self->frame_ready_ = true;
    return 1;
}

ID3D11Texture2D* NvdecDecoder::get_frame() {
    return frame_tex_.Get();
}

DXGI_FORMAT NvdecDecoder::frame_format() const {
    return use_interop_ ? DXGI_FORMAT_R8_UNORM : DXGI_FORMAT_NV12;
}

bool NvdecDecoder::supports_codec(VideoCodec codec) const {
    return codec == VideoCodec::H264 || codec == VideoCodec::AV1;
}

} // namespace mello::video
#endif
