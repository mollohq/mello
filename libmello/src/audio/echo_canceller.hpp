#pragma once
#include <cstdint>
#include <atomic>
#include <memory>
#include <vector>

namespace webrtc {
class AudioProcessing;
}

namespace mello::audio {

// 10ms at 48kHz — the chunk size WebRTC APM processes internally
static constexpr int APM_FRAME_SIZE = 480;

enum class WebRtcNsLevel {
    Off = 0,
    Low = 1,
    Moderate = 2,
    High = 3,
    VeryHigh = 4,
};

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
    void set_noise_suppression_level(WebRtcNsLevel level);
    void set_transient_suppression_enabled(bool enabled);
    void set_high_pass_filter_enabled(bool enabled);
    bool aec_enabled() const { return aec_enabled_.load(std::memory_order_relaxed); }
    bool agc_enabled() const { return agc_enabled_.load(std::memory_order_relaxed); }
    WebRtcNsLevel noise_suppression_level() const {
        return static_cast<WebRtcNsLevel>(ns_level_.load(std::memory_order_relaxed));
    }
    bool transient_suppression_enabled() const {
        return transient_suppression_enabled_.load(std::memory_order_relaxed);
    }
    bool high_pass_filter_enabled() const {
        return high_pass_filter_enabled_.load(std::memory_order_relaxed);
    }
    uint32_t capture_frames() const { return capture_frames_.load(std::memory_order_relaxed); }
    uint32_t render_frames() const { return render_frames_.load(std::memory_order_relaxed); }

private:
    void apply_config();

    webrtc::AudioProcessing* apm_ = nullptr;
    int sample_rate_ = 0;
    int channels_ = 0;
    std::atomic<bool> aec_enabled_{true};
    std::atomic<bool> agc_enabled_{true};
    std::atomic<int> ns_level_{static_cast<int>(WebRtcNsLevel::Off)};
    std::atomic<bool> transient_suppression_enabled_{false};
    std::atomic<bool> high_pass_filter_enabled_{false};
    std::atomic<uint32_t> capture_frames_{0};
    std::atomic<uint32_t> render_frames_{0};
    std::vector<int16_t> render_scratch_;
};

} // namespace mello::audio
