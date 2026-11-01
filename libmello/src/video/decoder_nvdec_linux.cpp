#if defined(__linux__)
#include "decoder_nvdec.hpp"
#include "nvdec_host_layout.hpp"
#include "../util/log.hpp"

#include <dlfcn.h>

namespace mello::video {

static constexpr const char* TAG = "video/decoder";

// CUDA_MEMCPY2D of the CUDA driver API (the vendored cuda.h is a stub).
struct CudaMemcpy2D {
    size_t             srcXInBytes;
    size_t             srcY;
    unsigned int       srcMemoryType; // 1 = host, 2 = device, 3 = array
    const void*        srcHost;
    unsigned long long srcDevice;
    void*              srcArray;
    size_t             srcPitch;
    size_t             dstXInBytes;
    size_t             dstY;
    unsigned int       dstMemoryType;
    void*              dstHost;
    unsigned long long dstDevice;
    void*              dstArray;
    size_t             dstPitch;
    size_t             WidthInBytes;
    size_t             Height;
};
constexpr unsigned int kMemoryTypeHost = 1;
constexpr unsigned int kMemoryTypeDevice = 2;

struct NvdecDecoder::Api {
    int (*cuInit)(unsigned int) = nullptr;
    int (*cuDeviceGet)(int*, int) = nullptr;
    int (*cuCtxCreate)(void**, unsigned int, int) = nullptr;
    int (*cuCtxDestroy)(void*) = nullptr;
    int (*cuCtxPushCurrent)(void*) = nullptr;
    int (*cuCtxPopCurrent)(void**) = nullptr;
    int (*cuMemcpy2D)(const void*) = nullptr;
    decltype(&cuvidCreateVideoParser)  createParser = nullptr;
    decltype(&cuvidDestroyVideoParser) destroyParser = nullptr;
    decltype(&cuvidParseVideoData)     parse = nullptr;
    decltype(&cuvidCreateDecoder)      createDecoder = nullptr;
    decltype(&cuvidDestroyDecoder)     destroyDecoder = nullptr;
    decltype(&cuvidDecodePicture)      decodePicture = nullptr;
    decltype(&cuvidMapVideoFrame64)    mapFrame = nullptr;
    decltype(&cuvidUnmapVideoFrame64)  unmapFrame = nullptr;
};

template<typename T>
static bool resolve(void* lib, const char* name, T& out) {
    out = reinterpret_cast<T>(dlsym(lib, name));
    return out != nullptr;
}

bool NvdecDecoder::is_available() {
    void* cuda = dlopen("libcuda.so.1", RTLD_NOW | RTLD_LOCAL);
    void* cuvid = dlopen("libnvcuvid.so.1", RTLD_NOW | RTLD_LOCAL);
    const bool ok = cuda && cuvid;
    if (cuvid) dlclose(cuvid);
    if (cuda) dlclose(cuda);
    return ok;
}

bool NvdecDecoder::initialize(const GraphicsDevice& device, const DecoderConfig& config) {
    (void)device; // CPU copy: no graphics device to share with CUDA
    config_ = config;
    auto fail = [this](const char* why) {
        MELLO_LOG_INFO(TAG, "NVDEC not used: %s", why);
        shutdown();
        return false;
    };
    if (config.codec != VideoCodec::H264) return fail("not H.264");

    cuda_lib_ = dlopen("libcuda.so.1", RTLD_NOW | RTLD_LOCAL);
    cuvid_lib_ = dlopen("libnvcuvid.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!cuda_lib_ || !cuvid_lib_) return fail("libcuda.so.1 or libnvcuvid.so.1 not found");

    api_ = new Api();
    Api& a = *api_;
    bool ok = resolve(cuda_lib_, "cuInit", a.cuInit) && resolve(cuda_lib_, "cuDeviceGet", a.cuDeviceGet) &&
              resolve(cuda_lib_, "cuCtxCreate_v2", a.cuCtxCreate) &&
              resolve(cuda_lib_, "cuCtxDestroy_v2", a.cuCtxDestroy) &&
              resolve(cuda_lib_, "cuCtxPushCurrent_v2", a.cuCtxPushCurrent) &&
              resolve(cuda_lib_, "cuCtxPopCurrent_v2", a.cuCtxPopCurrent) &&
              resolve(cuda_lib_, "cuMemcpy2D_v2", a.cuMemcpy2D) &&
              resolve(cuvid_lib_, "cuvidCreateVideoParser", a.createParser) &&
              resolve(cuvid_lib_, "cuvidDestroyVideoParser", a.destroyParser) &&
              resolve(cuvid_lib_, "cuvidParseVideoData", a.parse) &&
              resolve(cuvid_lib_, "cuvidCreateDecoder", a.createDecoder) &&
              resolve(cuvid_lib_, "cuvidDestroyDecoder", a.destroyDecoder) &&
              resolve(cuvid_lib_, "cuvidDecodePicture", a.decodePicture) &&
              resolve(cuvid_lib_, "cuvidMapVideoFrame64", a.mapFrame) &&
              resolve(cuvid_lib_, "cuvidUnmapVideoFrame64", a.unmapFrame);
    if (!ok) return fail("CUDA or NVDEC entry points not found");

    if (a.cuInit(0) != CUDA_SUCCESS) return fail("cuInit failed (no usable NVIDIA GPU)");
    int cu_device = 0;
    if (a.cuDeviceGet(&cu_device, 0) != CUDA_SUCCESS) return fail("cuDeviceGet failed");
    if (a.cuCtxCreate(&cu_context_, 0, cu_device) != CUDA_SUCCESS) {
        cu_context_ = nullptr;
        return fail("cuCtxCreate failed");
    }
    // cuCtxCreate makes the context current on this thread (the caller of
    // start). decode() pushes it on the decode thread for each call.
    {
        void* popped = nullptr;
        a.cuCtxPopCurrent(&popped);
    }

    CUVIDPARSERPARAMS params{};
    params.CodecType              = cudaVideoCodec_H264;
    params.ulMaxNumDecodeSurfaces = 8;
    params.ulMaxDisplayDelay      = 0; // low latency: show at once
    params.pUserData              = this;
    params.pfnSequenceCallback    = handle_video_sequence;
    params.pfnDecodePicture       = handle_picture_decode;
    params.pfnDisplayPicture      = handle_picture_display;
    if (a.createParser(&parser_, &params) != CUDA_SUCCESS) {
        parser_ = nullptr;
        return fail("cuvidCreateVideoParser failed");
    }

    MELLO_LOG_INFO(TAG, "Selected decoder: NVDEC (host copy) codec=H264 resolution=%ux%u",
                   config.width, config.height);
    return true;
}

void NvdecDecoder::shutdown() {
    if (api_) {
        Api& a = *api_;
        const bool pushed = cu_context_ && a.cuCtxPushCurrent(cu_context_) == CUDA_SUCCESS;
        if (parser_) a.destroyParser(parser_);
        if (decoder_) a.destroyDecoder(decoder_);
        if (pushed) {
            void* popped = nullptr;
            a.cuCtxPopCurrent(&popped);
        }
        if (cu_context_) a.cuCtxDestroy(cu_context_);
    }
    parser_ = nullptr;
    decoder_ = nullptr;
    cu_context_ = nullptr;
    delete api_;
    api_ = nullptr;
    if (cuvid_lib_) { dlclose(cuvid_lib_); cuvid_lib_ = nullptr; }
    if (cuda_lib_)  { dlclose(cuda_lib_);  cuda_lib_ = nullptr; }
    pending_display_.clear();
    display_width_ = display_height_ = 0;
    decoder_coded_width_ = decoder_coded_height_ = 0;
    decoder_codec_ = decoder_chroma_format_ = -1;
    sequence_failed_ = false;
    for (Slot& slot : slots_) slot = Slot{};
}

DecodeFeedResult NvdecDecoder::decode(const uint8_t* data, size_t size, bool is_keyframe, int slot) {
    (void)is_keyframe;
    if (!parser_ || slot < 0 || slot >= kOutputSlots) return DecodeFeedResult::Error;
    Api& a = *api_;
    if (a.cuCtxPushCurrent(cu_context_) != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuCtxPushCurrent failed");
        return DecodeFeedResult::Error;
    }

    pending_display_.clear();
    sequence_failed_ = false;
    CUVIDSOURCEDATAPACKET packet{};
    packet.payload      = data;
    packet.payload_size = static_cast<unsigned long>(size);
    packet.flags        = CUVID_PKT_TIMESTAMP | CUVID_PKT_ENDOFPICTURE;
    packet.timestamp    = 0;
    const CUresult res = a.parse(parser_, &packet);

    bool frame_ready = false;
    bool copy_failed = false;
    if (res == CUDA_SUCCESS && !pending_display_.empty()) {
        // The newest surface wins the slot.
        if (copy_to_slot(pending_display_.back().picture_index, slot)) {
            frame_ready = true;
        } else {
            copy_failed = true;
        }
    }
    pending_display_.clear();

    void* popped = nullptr;
    const int pop_result = a.cuCtxPopCurrent(&popped);

    if (res != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuvidParseVideoData failed: %d", res);
        return DecodeFeedResult::Error;
    }
    if (pop_result != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuCtxPopCurrent failed: %d", pop_result);
        return DecodeFeedResult::Error;
    }
    // A sequence the GPU cannot decode (too large, an unknown profile) shows
    // nothing and never will: an error, so the viewer can move on.
    if (copy_failed || sequence_failed_) return DecodeFeedResult::Error;
    return frame_ready ? DecodeFeedResult::FrameReady : DecodeFeedResult::Accepted;
}

// CUDA surface to the slot's host buffer. The CUDA context is current.
bool NvdecDecoder::copy_to_slot(int picture_index, int slot_index) {
    if (!decoder_ || display_width_ == 0 || display_height_ == 0) return false;
    Api& a = *api_;
    const uint32_t w = display_width_ & ~1u;
    const uint32_t h = display_height_ & ~1u;
    Slot& slot = slots_[slot_index];
    const NvdecHostCopy plan = plan_nvdec_host_copy(w, h, decoder_coded_height_);
    slot.nv12.resize(plan.buffer_bytes);

    CUVIDPROCPARAMS proc{};
    proc.progressive_frame = 1;
    unsigned int pitch = 0;
    unsigned long long dev_ptr = 0;
    if (a.mapFrame(decoder_, picture_index, &dev_ptr, &pitch, &proc) != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuvidMapVideoFrame failed");
        return false;
    }
    const size_t copy_w = (w <= pitch) ? w : pitch;
    auto copy_rows = [&](uint32_t src_row, uint32_t dst_row, uint32_t rows) {
        CudaMemcpy2D cp{};
        cp.srcMemoryType = kMemoryTypeDevice;
        cp.srcDevice     = dev_ptr;
        cp.srcY          = src_row;
        cp.srcPitch      = pitch;
        cp.dstMemoryType = kMemoryTypeHost;
        cp.dstHost       = slot.nv12.data();
        cp.dstY          = dst_row;
        cp.dstPitch      = w;
        cp.WidthInBytes  = copy_w;
        cp.Height        = rows;
        return a.cuMemcpy2D(&cp);
    };
    int rc = copy_rows(0, 0, plan.luma_rows);
    if (rc == CUDA_SUCCESS) rc = copy_rows(plan.chroma_src_row, plan.chroma_dst_row, plan.chroma_rows);
    const bool unmapped = a.unmapFrame(decoder_, dev_ptr) == CUDA_SUCCESS;
    if (rc != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: CUDA copy to the host failed: %d", rc);
        return false;
    }
    if (!unmapped) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuvidUnmapVideoFrame failed");
        return false;
    }
    slot.width = w;
    slot.height = h;
    return true;
}

int CUDAAPI NvdecDecoder::handle_video_sequence(void* user, CUVIDEOFORMAT* fmt) {
    auto* self = static_cast<NvdecDecoder*>(user);
    Api& a = *self->api_;
    const uint32_t display_w = static_cast<uint32_t>(fmt->display_area.right - fmt->display_area.left);
    const uint32_t display_h = static_cast<uint32_t>(fmt->display_area.bottom - fmt->display_area.top);
    self->display_width_ = display_w > 0 ? display_w : fmt->coded_width;
    self->display_height_ = display_h > 0 ? display_h : fmt->coded_height;

    if (self->decoder_ && self->decoder_coded_width_ == fmt->coded_width &&
        self->decoder_coded_height_ == fmt->coded_height &&
        self->decoder_codec_ == static_cast<int>(fmt->codec) &&
        self->decoder_chroma_format_ == static_cast<int>(fmt->chroma_format)) {
        return 1;
    }
    MELLO_LOG_INFO(TAG, "NVDEC: (re)create decoder coded=%ux%u display=%ux%u",
                   fmt->coded_width, fmt->coded_height, self->display_width_, self->display_height_);

    // A picture of the old sequence indexes a surface of the decoder that
    // goes now (see decoder_nvdec.cpp): not shown.
    self->pending_display_.clear();
    if (self->decoder_) {
        a.destroyDecoder(self->decoder_);
        self->decoder_ = nullptr;
    }

    CUVIDDECODECREATEINFO info{};
    info.CodecType           = fmt->codec;
    info.ChromaFormat        = fmt->chroma_format;
    info.OutputFormat        = cudaVideoSurfaceFormat_NV12;
    info.ulWidth             = fmt->coded_width;
    info.ulHeight            = fmt->coded_height;
    info.ulTargetWidth       = fmt->coded_width;
    info.ulTargetHeight      = fmt->coded_height;
    info.ulNumDecodeSurfaces = 8;
    info.ulNumOutputSurfaces = 2;
    info.DeinterlaceMode     = cudaVideoDeinterlaceMode_Weave;
    const CUresult rc = a.createDecoder(&self->decoder_, &info);
    if (rc != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuvidCreateDecoder failed: %d (coded=%ux%u)", rc,
                        fmt->coded_width, fmt->coded_height);
        self->decoder_ = nullptr;
        self->sequence_failed_ = true;
        return 0;
    }
    self->decoder_coded_width_ = fmt->coded_width;
    self->decoder_coded_height_ = fmt->coded_height;
    self->decoder_codec_ = static_cast<int>(fmt->codec);
    self->decoder_chroma_format_ = static_cast<int>(fmt->chroma_format);
    return 8;
}

int CUDAAPI NvdecDecoder::handle_picture_decode(void* user, CUVIDPICPARAMS* pic) {
    auto* self = static_cast<NvdecDecoder*>(user);
    if (!self->decoder_) return 0;
    const CUresult res = self->api_->decodePicture(self->decoder_, pic);
    if (res != CUDA_SUCCESS) {
        MELLO_LOG_ERROR(TAG, "NVDEC: cuvidDecodePicture failed: %d", res);
        return 0;
    }
    return 1;
}

int CUDAAPI NvdecDecoder::handle_picture_display(void* user, CUVIDPARSERDISPINFO* disp) {
    auto* self = static_cast<NvdecDecoder*>(user);
    if (!self->decoder_ || !disp) return 0;
    self->pending_display_.push_back(*disp);
    return 1;
}

bool NvdecDecoder::picture(int slot_index, CpuPicture& out) const {
    if (slot_index < 0 || slot_index >= kOutputSlots) return false;
    const Slot& slot = slots_[slot_index];
    if (slot.nv12.empty() || slot.width == 0 || slot.height == 0) return false;
    out.format = CpuPictureFormat::NV12;
    out.y = slot.nv12.data();
    out.u = slot.nv12.data() + static_cast<size_t>(slot.width) * slot.height;
    out.v = nullptr;
    out.y_stride = slot.width;
    out.uv_stride = slot.width;
    out.width = slot.width;
    out.height = slot.height;
    return true;
}

} // namespace mello::video
#endif
