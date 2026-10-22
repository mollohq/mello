#pragma once
#include "decoder.hpp"

#ifdef _WIN32
#include <wrl/client.h>
#include <nvcuvid.h>
#include <cuviddec.h>
#include <mutex>

namespace mello::video {

class NvdecDecoder : public Decoder {
public:
    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    DecodeFeedResult decode(const uint8_t* data, size_t size, bool is_keyframe) override;
    ID3D11Texture2D* get_frame() override;
    DXGI_FORMAT      frame_format() const override;
    uint32_t         coded_height() const { return coded_height_; }
    void             publish_d3d11_frame() override;
    bool             supports_codec(VideoCodec codec) const override;
    const char*      name() const override { return "NVDEC"; }

    static bool is_available();

private:
    typedef int (*CuInit_t)(unsigned int);
    typedef int (*CuDeviceGet_t)(int*, int);
    typedef int (*CuCtxCreate_t)(void**, unsigned int, int);
    typedef int (*CuCtxDestroy_t)(void*);
    typedef int (*CuCtxPushCurrent_t)(void*);
    typedef int (*CuCtxPopCurrent_t)(void**);

    HMODULE cuda_dll_   = nullptr;
    HMODULE cuvid_dll_  = nullptr;
    void*   cu_context_ = nullptr;
    CuCtxPushCurrent_t cuCtxPush_ = nullptr;
    CuCtxPopCurrent_t  cuCtxPop_  = nullptr;

    CUvideodecoder decoder_ = nullptr;
    CUvideoparser  parser_  = nullptr;

    static int CUDAAPI handle_video_sequence(void* user, CUVIDEOFORMAT* fmt);
    static int CUDAAPI handle_picture_decode(void* user, CUVIDPICPARAMS* pic);
    static int CUDAAPI handle_picture_display(void* user, CUVIDPARSERDISPINFO* disp);

    bool frame_ready_ = false;
    bool use_interop_ = false;
    // Row of the chroma plane in the host layout and in frame_tex_. Set once
    // in initialize; other threads read it (coded_height).
    uint32_t coded_height_ = 0;
    // Luma rows of the host layout: coded_height_ for the R8 path, the
    // display height for the NV12 path. Set once in initialize.
    uint32_t layout_rows_ = 0;
    // Coded height of the stream, from the sequence callback. Decode thread.
    uint32_t stream_coded_height_ = 0;
    uint32_t decoder_coded_width_ = 0;
    uint32_t decoder_coded_height_ = 0;
    int decoder_codec_ = -1;
    int decoder_chroma_format_ = -1;
    uint64_t sequence_callback_count_ = 0;

    DecoderConfig config_{};
    Microsoft::WRL::ComPtr<ID3D11Device>    device_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> frame_tex_;

    // Host copies of the decoded frame, one owner each, so no buffer is read
    // and written by two threads. The decode thread fills decode_buf_ and
    // swaps it with ready_buf_ under publish_mutex_. publish_d3d11_frame swaps
    // ready_buf_ with upload_buf_ under the same lock and uploads from
    // upload_buf_ without it. The layout is plan_nvdec_host_copy's.
    std::vector<uint8_t> decode_buf_;  // decode thread
    std::vector<uint8_t> ready_buf_;   // publish_mutex_
    std::vector<uint8_t> upload_buf_;  // present thread

    // Deferred D3D11 work: CUDA callbacks run on the decode thread and must
    // not touch D3D11 (including cuGraphicsMapResources on registered textures).
    std::mutex publish_mutex_;
    bool pending_r8_upload_ = false;
    bool pending_nv12_upload_ = false;
};

} // namespace mello::video
#endif
