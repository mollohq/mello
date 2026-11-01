#pragma once
#include "decoder.hpp"

#if defined(__linux__)
#include <memory>
#include <string>
#include <vector>

namespace mello::video {

/// VA-API decode on AMD and Intel through GStreamer (decoder_gst_va_linux.cpp):
/// appsrc, h264parse, the VA H.264 decoder of the primary GPU, appsink in
/// system memory. GStreamer parses the stream and keeps the reference
/// pictures; it does not use FFmpeg. GStreamer loads with dlopen, so a
/// system without it, or without the `va` plugin, has no VA-API decoder and
/// the viewer takes the next one.
///
/// Each picture is copied as NV12 into its output slot. All calls run on the
/// decode thread; GStreamer runs its own streaming threads inside.
class GstVaDecoder : public Decoder {
public:
    GstVaDecoder();
    ~GstVaDecoder() override;

    /// True when GStreamer loads and has h264parse and a VA H.264 decoder.
    static bool is_available();

    /// Loads GStreamer and picks the VA element, once per process, so the
    /// first watch does not wait for it. Any thread.
    static void warm_up();

    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    DecodeFeedResult decode(const uint8_t* data, size_t size, bool is_keyframe, int slot) override;
    bool             picture(int slot, CpuPicture& out) const override;
    bool             supports_codec(VideoCodec codec) const override { return codec == VideoCodec::H264; }
    const char*      name() const override { return "VA-API"; }

private:
    struct Pipeline;  // the GStreamer objects
    std::unique_ptr<Pipeline> p_;

    struct Slot {
        std::vector<uint8_t> nv12;  // luma rows, then the interleaved chroma
        uint32_t width = 0;
        uint32_t height = 0;
    };
    Slot slots_[kOutputSlots];
    uint64_t frames_in_ = 0;
    bool failed_ = false;
};

} // namespace mello::video
#endif
