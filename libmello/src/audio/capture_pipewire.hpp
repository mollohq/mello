#pragma once

#if defined(__linux__)
#include "audio_capture.hpp"
#include "pipewire_stream.hpp"
#include "../util/ring_buffer.hpp"
#include <atomic>
#include <string>
#include <thread>

namespace mello::audio {

/// Microphone capture through PipeWire: 48 kHz mono S16, converted from the
/// device format inside the stream (spec 10 §2).
///
/// Two threads. PipeWire's real-time data thread only copies each buffer into
/// a ring and wakes the capture thread. The capture thread drains the ring and
/// calls the callback, which runs the whole send chain (APM, RNNoise, VAD,
/// Opus, the packet sink). That work on PipeWire's own loop made the node miss
/// its cycles (xruns, robotic audio at the receiver, 2026-10-31).
///
/// The capture node exists from start() to stop() only, so no microphone node
/// runs outside a voice session.
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
    void on_process(pw_stream* stream); // PipeWire data thread
    void capture_thread_main();         // the capture thread
    void stop_capture_thread();

    pw::Stream stream_;
    std::string target_;
    Callback callback_;
    std::atomic<bool> running_{false};

    // One second of mono audio between the two threads.
    util::RingBuffer<int16_t> ring_{48000};
    std::atomic<uint64_t> dropped_samples_{0};
    int wake_fd_ = -1; // eventfd: the data thread writes, the capture thread reads
    std::thread capture_thread_;
};

} // namespace mello::audio
#endif
