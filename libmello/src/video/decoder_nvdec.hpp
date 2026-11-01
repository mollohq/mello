#pragma once
#include "decoder.hpp"

#ifdef _WIN32
#include <wrl/client.h>
#include <nvcuvid.h>
#include <cuviddec.h>
#include <vector>

namespace mello::video {

// NVDEC through CUDA-D3D11 interop (streaming-quality stage 3). Each output
// slot is an R8 texture with the NV12 layout (luma rows, then the chroma
// rows), registered with CUDA once. A decoded surface goes from CUDA memory
// into the slot texture on the GPU: no host copy and no upload.
//
// The parser callbacks only record the surface to show. The copy into the
// slot runs after cuvidParseVideoData returns, on the same decode thread
// with the CUDA context current, so no D3D11 interop call runs inside a
// parser callback.
class NvdecDecoder : public Decoder {
public:
    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    DecodeFeedResult decode(const uint8_t* data, size_t size, bool is_keyframe, int slot) override;
    bool             picture(int slot, DecodedPicture& out) const override;
    bool             supports_codec(VideoCodec codec) const override;
    const char*      name() const override { return "NVDEC"; }

    static bool is_available();

private:
    typedef int (CUDAAPI *CuInit_t)(unsigned int);
    typedef int (CUDAAPI *CuDeviceGet_t)(int*, int);
    typedef int (CUDAAPI *CuCtxCreate_t)(void**, unsigned int, int);
    typedef int (CUDAAPI *CuCtxDestroy_t)(void*);
    typedef int (CUDAAPI *CuCtxPushCurrent_t)(void*);
    typedef int (CUDAAPI *CuCtxPopCurrent_t)(void**);
    typedef int (CUDAAPI *CuD3D11GetDevice_t)(int*, IDXGIAdapter*);
    typedef int (CUDAAPI *CuGraphicsD3D11RegisterResource_t)(void**, ID3D11Resource*, unsigned int);
    typedef int (CUDAAPI *CuGraphicsUnregisterResource_t)(void*);
    typedef int (CUDAAPI *CuGraphicsMapResources_t)(unsigned int, void**, void*);
    typedef int (CUDAAPI *CuGraphicsUnmapResources_t)(unsigned int, void**, void*);
    typedef int (CUDAAPI *CuGraphicsSubResourceGetMappedArray_t)(void**, void*, unsigned int, unsigned int);
    typedef int (CUDAAPI *CuMemcpy2D_t)(const void*);

    struct Slot {
        Microsoft::WRL::ComPtr<ID3D11Texture2D> texture;
        void* resource = nullptr;  // CUgraphicsResource
        uint32_t width = 0;
        uint32_t height = 0;       // luma rows; chroma from this row
    };

    bool load_cuda_entry_points();
    bool ensure_slot(int slot, uint32_t width, uint32_t height);
    void release_slot(Slot& slot);
    bool copy_to_slot(int picture_index, int slot);

    static int CUDAAPI handle_video_sequence(void* user, CUVIDEOFORMAT* fmt);
    static int CUDAAPI handle_picture_decode(void* user, CUVIDPICPARAMS* pic);
    static int CUDAAPI handle_picture_display(void* user, CUVIDPARSERDISPINFO* disp);

    HMODULE cuda_dll_   = nullptr;
    HMODULE cuvid_dll_  = nullptr;
    void*   cu_context_ = nullptr;
    CuCtxPushCurrent_t cuCtxPush_ = nullptr;
    CuCtxPopCurrent_t  cuCtxPop_  = nullptr;
    CuGraphicsD3D11RegisterResource_t cuRegister_ = nullptr;
    CuGraphicsUnregisterResource_t    cuUnregister_ = nullptr;
    CuGraphicsMapResources_t          cuMap_ = nullptr;
    CuGraphicsUnmapResources_t        cuUnmap_ = nullptr;
    CuGraphicsSubResourceGetMappedArray_t cuMappedArray_ = nullptr;
    CuMemcpy2D_t                      cuMemcpy2D_ = nullptr;

    CUvideodecoder decoder_ = nullptr;
    CUvideoparser  parser_  = nullptr;

    // Surfaces the parser asked to show during the current parse call.
    std::vector<CUVIDPARSERDISPINFO> pending_display_;

    // From the sequence callback (decode thread).
    uint32_t display_width_  = 0;
    uint32_t display_height_ = 0;
    uint32_t decoder_coded_width_ = 0;
    uint32_t decoder_coded_height_ = 0;
    int decoder_codec_ = -1;
    int decoder_chroma_format_ = -1;
    uint64_t sequence_callback_count_ = 0;

    DecoderConfig config_{};
    Microsoft::WRL::ComPtr<ID3D11Device> device_;
    Slot slots_[kOutputSlots];
};

} // namespace mello::video
#endif

#if defined(__linux__)
#include <nvcuvid.h>
#include <cuviddec.h>
#include <vector>

namespace mello::video {

/// NVDEC on Linux (decoder_nvdec_linux.cpp): libnvcuvid and libcuda through
/// dlopen, NVIDIA's own H.264 parser. Each decoded surface is copied from
/// CUDA memory into a host NV12 buffer of its output slot; the viewer
/// converts it on the CPU. Needs the NVIDIA driver (not nouveau).
///
/// As on Windows, the parser callbacks only record the surface to show, and
/// the copy runs after cuvidParseVideoData returns. All calls run on the
/// decode thread.
class NvdecDecoder : public Decoder {
public:
    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    DecodeFeedResult decode(const uint8_t* data, size_t size, bool is_keyframe, int slot) override;
    bool             picture(int slot, CpuPicture& out) const override;
    bool             supports_codec(VideoCodec codec) const override { return codec == VideoCodec::H264; }
    const char*      name() const override { return "NVDEC"; }

    /// True when libcuda.so.1 and libnvcuvid.so.1 load. Says nothing about a
    /// usable GPU: initialize() finds that out.
    static bool is_available();

    ~NvdecDecoder() override { shutdown(); }

private:
    struct Api;  // the resolved entry points

    struct Slot {
        std::vector<uint8_t> nv12;  // luma rows, then the interleaved chroma
        uint32_t width = 0;
        uint32_t height = 0;
    };

    bool copy_to_slot(int picture_index, int slot);

    static int CUDAAPI handle_video_sequence(void* user, CUVIDEOFORMAT* fmt);
    static int CUDAAPI handle_picture_decode(void* user, CUVIDPICPARAMS* pic);
    static int CUDAAPI handle_picture_display(void* user, CUVIDPARSERDISPINFO* disp);

    void* cuda_lib_  = nullptr;
    void* cuvid_lib_ = nullptr;
    Api*  api_       = nullptr;
    void* cu_context_ = nullptr;

    CUvideodecoder decoder_ = nullptr;
    CUvideoparser  parser_  = nullptr;

    std::vector<CUVIDPARSERDISPINFO> pending_display_;

    uint32_t display_width_  = 0;
    uint32_t display_height_ = 0;
    uint32_t decoder_coded_width_ = 0;
    uint32_t decoder_coded_height_ = 0;
    int decoder_codec_ = -1;
    int decoder_chroma_format_ = -1;
    bool sequence_failed_ = false;

    DecoderConfig config_{};
    Slot slots_[kOutputSlots];
};

} // namespace mello::video
#endif
