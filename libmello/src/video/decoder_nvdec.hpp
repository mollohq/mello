#pragma once
#include "decoder.hpp"

#ifdef _WIN32
#include <wrl/client.h>
#include <nvcuvid.h>
#include <cuviddec.h>

namespace mello::video {

class NvdecDecoder : public Decoder {
public:
    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    bool             decode(const uint8_t* data, size_t size, bool is_keyframe) override;
    ID3D11Texture2D* get_frame() override;
    DXGI_FORMAT      frame_format() const override;
    bool             supports_codec(VideoCodec codec) const override;
    const char*      name() const override { return "NVDEC"; }

    static bool is_available();

private:
    typedef int (*CuInit_t)(unsigned int);
    typedef int (*CuDeviceGet_t)(int*, int);
    typedef int (*CuCtxCreate_t)(void**, unsigned int, int);
    typedef int (*CuCtxDestroy_t)(void*);

    HMODULE cuda_dll_   = nullptr;
    HMODULE cuvid_dll_  = nullptr;
    void*   cu_context_ = nullptr;

    CUvideodecoder decoder_ = nullptr;
    CUvideoparser  parser_  = nullptr;

    static int CUDAAPI handle_video_sequence(void* user, CUVIDEOFORMAT* fmt);
    static int CUDAAPI handle_picture_decode(void* user, CUVIDPICPARAMS* pic);
    static int CUDAAPI handle_picture_display(void* user, CUVIDPARSERDISPINFO* disp);

    bool frame_ready_ = false;
    bool use_interop_ = false;

    DecoderConfig config_{};
    Microsoft::WRL::ComPtr<ID3D11Device>    device_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> frame_tex_;

    void* cuda_gfx_resource_ = nullptr; // CUgraphicsResource for frame_tex_ (interop only)
    std::vector<uint8_t> nv12_buf_;    // fallback only (when interop unavailable)
};

} // namespace mello::video
#endif
