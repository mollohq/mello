#pragma once
#ifdef __APPLE__

#include "encoder.hpp"
#include <deque>
#include <mutex>
#include <vector>

namespace mello::video {

class VTEncoder : public Encoder {
public:
    VTEncoder();
    ~VTEncoder() override;

    bool        initialize(const GraphicsDevice& device, const EncoderConfig& config) override;
    void        shutdown() override;
    bool        encode(void* cv_pixel_buffer, uint64_t capture_us, EncodedPacket& out) override;
    void        set_framerate(uint32_t fps) override;
    void        request_keyframe() override;
    void        set_bitrate(uint32_t kbps) override;
    void        get_stats(EncoderStats& out) const override;
    bool        supports_codec(VideoCodec codec) const override;
    const char* name() const override { return "VideoToolbox"; }

    static bool is_available();

    // Accessed by the C compress callback, so it must be public. One encoded
    // frame: its Annex B bytes, the keyframe flag, its presentation time.
    struct Output {
        std::vector<uint8_t> data;
        bool keyframe = false;
        uint64_t pts_us = 0;
    };
    std::mutex              output_mutex_;
    std::deque<Output>      outputs_;  // output_mutex_

    /// The recording intent: frames in flight. encode() submits and returns
    /// the oldest frame that is done (or none yet), with its own time, so the
    /// media engine works on frames in a row. A stream waits for each frame
    /// (the lowest latency).
    bool pipelined() const { return pipelined_; }

private:
    void* session_ = nullptr; // VTCompressionSessionRef

    uint32_t width_    = 0;
    uint32_t height_   = 0;
    uint32_t fps_      = 60;
    uint32_t bitrate_  = 0; // kbps

    bool     force_keyframe_ = false;
    bool     pipelined_      = false;
    uint64_t frame_count_    = 0;
    uint32_t keyframe_interval_ = 120;

    // Stats
    mutable EncoderStats stats_{};
};

} // namespace mello::video

#endif
