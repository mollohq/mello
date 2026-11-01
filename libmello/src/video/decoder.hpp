#pragma once
#include "graphics_device.hpp"
#include "encoder.hpp" // for VideoCodec
#include <cstddef>
#include <cstdint>

#ifdef _WIN32
#include <d3d11.h>
#include <wrl/client.h>
#endif

namespace mello::video {

struct DecoderConfig {
    uint32_t   width;
    uint32_t   height;
    VideoCodec codec = VideoCodec::H264;
};

enum class DecodeFeedResult {
    Error,
    Accepted,
    FrameReady,
};

#ifdef _WIN32
/// One decoded picture in GPU memory. The texture reference keeps it alive
/// while the present thread converts it, also after the decoder made a new
/// texture for its slot (a size change).
struct DecodedPicture {
    Microsoft::WRL::ComPtr<ID3D11Texture2D> texture;
    /// DXGI_FORMAT_NV12, or DXGI_FORMAT_R8_UNORM with the NV12 layout (luma
    /// rows, then the interleaved chroma rows from `uv_row`).
    DXGI_FORMAT format = DXGI_FORMAT_NV12;
    /// Visible size of the picture.
    uint32_t width = 0;
    uint32_t height = 0;
    /// R8 layout: the row where the chroma starts. 0 for NV12.
    uint32_t uv_row = 0;
};
#elif defined(__linux__)
/// The layout of a CpuPicture.
enum class CpuPictureFormat {
    /// Three planes: luma, then U and V at half size (OpenH264).
    I420,
    /// Two planes: luma, then interleaved U and V at half size (NVDEC, VA).
    /// `u` points at the interleaved plane, `v` is null.
    NV12,
};

/// One decoded picture in CPU memory, in the decoder's output slot. The
/// planes stay valid until the decoder writes that slot again; the viewer's
/// ring never decodes into a slot the present thread reads.
struct CpuPicture {
    CpuPictureFormat format = CpuPictureFormat::I420;
    const uint8_t* y = nullptr;
    const uint8_t* u = nullptr;
    const uint8_t* v = nullptr;
    uint32_t y_stride = 0;
    /// Bytes per row of the chroma plane (NV12: of the interleaved plane).
    uint32_t uv_stride = 0;
    /// Visible size of the picture (even).
    uint32_t width = 0;
    uint32_t height = 0;
};
#endif

/// A hardware or software H.264 decoder that writes each picture into one of
/// `kOutputSlots` output slots, chosen by the caller for each decode. The
/// viewer pipeline keeps a ring of the same size: a slot that the present
/// thread reads is never the slot of the next decode. Streaming-quality
/// stage 3 (spec 14 section 5).
///
/// Threads: initialize, decode, picture and shutdown run on the decode
/// thread only (after initialize).
class Decoder {
public:
    static constexpr int kOutputSlots = 3;

    virtual ~Decoder() = default;

    virtual bool initialize(const GraphicsDevice& device, const DecoderConfig& config) = 0;
    virtual void shutdown() = 0;

    /// Decode one access unit. FrameReady: a picture is in output slot `slot`.
    virtual DecodeFeedResult decode(const uint8_t* data, size_t size, bool is_keyframe, int slot) = 0;

#ifdef _WIN32
    /// The picture of output slot `slot` after a FrameReady.
    virtual bool picture(int slot, DecodedPicture& out) const = 0;
#elif defined(__APPLE__)
    /// The latest decoded frame as a CVPixelBufferRef (BGRA). The caller
    /// retains it if it keeps it. Valid until the next call to decode().
    virtual void* get_frame_buffer() = 0;
#elif defined(__linux__)
    /// The picture of output slot `slot` after a FrameReady.
    virtual bool picture(int slot, CpuPicture& out) const = 0;
#endif

    virtual bool        supports_codec(VideoCodec codec) const = 0;
    virtual const char* name() const = 0;
};

} // namespace mello::video
