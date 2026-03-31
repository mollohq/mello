#pragma once
#include "graphics_device.hpp"
#include <cstdint>
#include <vector>

#ifdef _WIN32
#include <d3d11.h>
#endif

namespace mello::video {

enum class VideoCodec { H264, AV1 };

struct EncoderConfig {
    uint32_t   width;
    uint32_t   height;
    uint32_t   fps;
    uint32_t   bitrate_kbps;
    uint32_t   keyframe_interval = 120;
    VideoCodec codec = VideoCodec::H264;
};

struct EncodedPacket {
    std::vector<uint8_t> data;
    uint64_t             timestamp_us;
    bool                 is_keyframe;
};

struct EncoderStats {
    uint32_t bitrate_kbps;
    uint32_t fps_actual;
    uint32_t keyframes_sent;
    uint64_t bytes_sent;
};

class Encoder {
public:
    virtual ~Encoder() = default;

    virtual bool        initialize(const GraphicsDevice& device, const EncoderConfig& config) = 0;
    virtual void        shutdown() = 0;

#ifdef _WIN32
    virtual bool        encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) = 0;
#elif defined(__APPLE__)
    /// Encode one frame from a CVPixelBufferRef (passed as void*). BGRA input accepted.
    virtual bool        encode(void* cv_pixel_buffer, EncodedPacket& out) = 0;
#endif

    virtual void        request_keyframe() = 0;
    virtual void        set_bitrate(uint32_t kbps) = 0;
    virtual void        get_stats(EncoderStats& out) const = 0;
    virtual bool        supports_codec(VideoCodec codec) const = 0;
    virtual const char* name() const = 0;
};

} // namespace mello::video
