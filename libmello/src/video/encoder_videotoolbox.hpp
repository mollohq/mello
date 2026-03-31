#pragma once
#ifdef __APPLE__

#include "encoder.hpp"
#include <mutex>
#include <vector>

namespace mello::video {

class VTEncoder : public Encoder {
public:
    VTEncoder();
    ~VTEncoder() override;

    bool        initialize(const GraphicsDevice& device, const EncoderConfig& config) override;
    void        shutdown() override;
    bool        encode(void* cv_pixel_buffer, EncodedPacket& out) override;
    void        request_keyframe() override;
    void        set_bitrate(uint32_t kbps) override;
    void        get_stats(EncoderStats& out) const override;
    bool        supports_codec(VideoCodec codec) const override;
    const char* name() const override { return "VideoToolbox"; }

    static bool is_available();

    // Accessed by the C compress callback — must be public
    std::mutex              output_mutex_;
    std::vector<uint8_t>    output_data_;
    bool                    output_is_keyframe_ = false;
    bool                    output_ready_       = false;

private:
    void* session_ = nullptr; // VTCompressionSessionRef

    uint32_t width_    = 0;
    uint32_t height_   = 0;
    uint32_t fps_      = 60;
    uint32_t bitrate_  = 0; // kbps

    bool     force_keyframe_ = false;
    uint64_t frame_count_    = 0;
    uint32_t keyframe_interval_ = 120;

    // Stats
    mutable EncoderStats stats_{};
};

} // namespace mello::video

#endif
