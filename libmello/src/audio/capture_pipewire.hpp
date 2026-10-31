#pragma once

#if defined(__linux__)
#include "audio_capture.hpp"
#include "pipewire_stream.hpp"
#include <atomic>
#include <string>

namespace mello::audio {

/// Microphone capture through PipeWire: 48 kHz mono S16, converted from the
/// device format inside the stream (spec 10 §2 and §6.3).
///
/// initialize() only opens the stream object. The capture node exists from
/// start() to stop(), so no microphone node runs outside a voice session.
/// The callback runs on the stream's loop thread (the capture thread).
class PipeWireCapture : public AudioCapture {
public:
    PipeWireCapture() = default;
    ~PipeWireCapture() override;

    bool initialize(const char* device_id = nullptr) override;
    bool start(Callback callback) override;
    void stop() override;

    uint32_t sample_rate() const override { return pw::kRate; }
    uint32_t channels() const override { return 1; }
    /// The requested cycle (10 ms). The device latency is not known here.
    int input_latency_ms() const override { return 10; }

private:
    void on_process(pw_stream* stream);

    pw::Stream stream_;
    std::string target_;
    Callback callback_;
    std::atomic<bool> running_{false};
};

} // namespace mello::audio
#endif
