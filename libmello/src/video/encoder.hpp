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

    /// Trade encoded quality for encode speed, one step at a time.
    ///
    /// Quality features that are free on a current GPU are not free on an older
    /// one. When the encoder cannot hold the frame budget the result is not a
    /// slightly worse picture but a collapsing one: the encode queue is two
    /// deep and newest-wins, so overrun becomes silently dropped frames. It is
    /// always better to drop a quality feature than to drop frames.
    ///
    /// Returns true when the configuration actually changed. Default is a no-op
    /// so backends without tunable cost need no changes.
    virtual bool reduce_cost_tier() { return false; }

    /// 0 = full quality. Higher means features have been given up.
    virtual int  cost_tier() const { return 0; }

    /// Retarget rate control at a new output framerate.
    ///
    /// Required when the pipeline decimates frames: rate control derives its
    /// per-frame bit budget from the framerate, so an encoder still told 60 while
    /// being fed 30 hands out half the bits each frame actually deserves.
    ///
    /// Default no-op for backends without runtime framerate control.
    virtual void set_framerate(uint32_t fps) { (void)fps; }
};

} // namespace mello::video
