#pragma once
#ifdef __APPLE__

#include "decoder.hpp"
#include <mutex>

namespace mello::video {

class VTDecoder : public Decoder {
public:
    VTDecoder();
    ~VTDecoder() override;

    bool initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void shutdown() override;

    bool  decode(const uint8_t* data, size_t size, bool is_keyframe) override;
    void* get_frame_buffer() override;

    bool        supports_codec(VideoCodec codec) const override;
    const char* name() const override { return "VideoToolbox"; }

    static bool is_available();

    // Accessed by the C decompress callback — must be public
    std::mutex  frame_mutex_;
    void*       latest_frame_ = nullptr; // CVPixelBufferRef, retained

private:
    void* session_ = nullptr; // VTDecompressionSessionRef
    void* format_  = nullptr; // CMVideoFormatDescriptionRef (const-qualified internally)

    uint32_t width_  = 0;
    uint32_t height_ = 0;

    bool create_format_description(const uint8_t* sps, size_t sps_len,
                                   const uint8_t* pps, size_t pps_len);
    bool create_session();
};

} // namespace mello::video

#endif
