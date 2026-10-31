#pragma once

#if defined(__linux__)
#include "audio_playback.hpp"
#include "pipewire_stream.hpp"
#include "../util/ring_buffer.hpp"
#include <atomic>
#include <string>

namespace mello::audio {

/// Voice output through PipeWire: 48 kHz S16 with `render_channels()`
/// channels (stereo for the voice mix), converted to the device format inside
/// the stream. The render source runs on the stream's loop thread (the device
/// thread of spec 10 §5.3).
class PipeWirePlayback : public AudioPlayback {
public:
    PipeWirePlayback() = default;
    ~PipeWirePlayback() override;

    bool initialize(const char* device_id = nullptr) override;
    bool start() override;
    void stop() override;

    size_t feed(const int16_t* samples, size_t count) override;

    /// Must be called before start(): the channel count is part of the
    /// format that connect() negotiates.
    void set_input_channels(uint32_t channels) override {
        input_channels_ = channels < 1 ? 1 : (channels > 2 ? 2 : channels);
    }
    uint32_t render_channels() const override { return input_channels_; }

    uint32_t sample_rate() const override { return pw::kRate; }
    /// The requested cycle (10 ms). The device latency is not known here.
    int output_latency_ms() const override { return 10; }

private:
    void on_process(pw_stream* stream);

    pw::Stream stream_;
    std::string target_;
    uint32_t input_channels_ = 1;
    std::atomic<bool> running_{false};
    // ~1 second at stereo 48 kHz; mono uses half of it.
    util::RingBuffer<int16_t> ring_{48000 * 2};
};

} // namespace mello::audio
#endif
