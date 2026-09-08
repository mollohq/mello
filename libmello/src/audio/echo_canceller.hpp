#pragma once
#include <cstdint>
#include <atomic>
#include <memory>
#include <vector>
#include "api/scoped_refptr.h"

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

    /// Hint to APM about capture-minus-render latency (ms). Clamped to
    /// 0..500. Call after device switches and when the jitter depth
    /// changes. Thread-safe: may be called from any thread.
    void set_stream_delay_ms(int delay_ms);
    int stream_delay_ms() const { return stream_delay_ms_.load(std::memory_order_relaxed); }
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

    // Ref-counted handle; v2.x Create() returns scoped_refptr instead of a
    // raw pointer. Assigning nullptr releases our reference.
    rtc::scoped_refptr<webrtc::AudioProcessing> apm_;
    int sample_rate_ = 0;
    int channels_ = 0;
    std::atomic<bool> aec_enabled_{true};
    std::atomic<bool> agc_enabled_{true};
    std::atomic<int> ns_level_{static_cast<int>(WebRtcNsLevel::Off)};
    std::atomic<bool> transient_suppression_enabled_{false};
    std::atomic<bool> high_pass_filter_enabled_{false};
    std::atomic<uint32_t> capture_frames_{0};
    std::atomic<uint32_t> render_frames_{0};
    std::atomic<int> stream_delay_ms_{0};
    std::vector<int16_t> render_scratch_;
    // Pending render tail (< APM_FRAME_SIZE) carried to the next
    // process_render call. CoreAudio/WASAPI callbacks are ~512 frames,
    // not multiples of 480 — dropping the tail corrupts AEC alignment.
    // Guarded by APM's internal lock via ProcessReverseStream calls;
    // all pushes/pops happen on the playback thread (mix_output).
    std::vector<int16_t> render_pending_;
    std::atomic<uint32_t> render_dropped_tail_frames_{0};
};

} // namespace mello::audio
