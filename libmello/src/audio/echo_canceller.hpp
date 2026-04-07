#pragma once
#include <cstdint>
#include <atomic>
#include <memory>

namespace webrtc {
class AudioProcessing;
}

namespace mello::audio {

// 10ms at 48kHz — the chunk size WebRTC APM processes internally
static constexpr int APM_FRAME_SIZE = 480;

/// Wraps WebRTC AudioProcessing for AEC3 (echo cancellation) + AGC2 (gain control).
/// Thread-safety: process_capture() is called from the capture thread,
/// process_render() from the playback thread. APM handles this internally.
class EchoCanceller {
public:
    EchoCanceller();
    ~EchoCanceller();

    bool initialize(int sample_rate, int channels);
    void shutdown();

    /// Process near-end (mic) signal in-place. Called from capture thread.
    /// Splits 960-sample frames into two 480-sample APM calls.
    void process_capture(int16_t* samples, int count);

    /// Feed far-end (speaker) reference. Called from playback thread.
    /// Splits 960-sample frames into two 480-sample APM calls.
    void process_render(const int16_t* samples, int count);

    void set_aec_enabled(bool enabled);
    void set_agc_enabled(bool enabled);
    bool aec_enabled() const { return aec_enabled_.load(std::memory_order_relaxed); }
    bool agc_enabled() const { return agc_enabled_.load(std::memory_order_relaxed); }

private:
    void apply_config();

    webrtc::AudioProcessing* apm_ = nullptr;
    int sample_rate_ = 0;
    int channels_ = 0;
    std::atomic<bool> aec_enabled_{true};
    std::atomic<bool> agc_enabled_{true};
};

} // namespace mello::audio
