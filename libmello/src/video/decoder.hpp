#pragma once
#include "graphics_device.hpp"
#include "encoder.hpp" // for VideoCodec
#include <cstdint>

#ifdef _WIN32
#include <d3d11.h>
#endif

namespace mello::video {

struct DecoderConfig {
    uint32_t   width;
    uint32_t   height;
    VideoCodec codec = VideoCodec::H264;
};

class Decoder {
public:
    virtual ~Decoder() = default;

    virtual bool initialize(const GraphicsDevice& device, const DecoderConfig& config) = 0;
    virtual void shutdown() = 0;

    virtual bool decode(const uint8_t* data, size_t size, bool is_keyframe) = 0;

#ifdef _WIN32
    virtual ID3D11Texture2D* get_frame() = 0;

    /// Returns the DXGI format of the frame texture (NV12 by default, R8 for zero-copy NVDEC).
    virtual DXGI_FORMAT frame_format() const { return DXGI_FORMAT_NV12; }
#endif

    virtual bool        supports_codec(VideoCodec codec) const = 0;
    virtual const char* name() const = 0;
};

} // namespace mello::video
