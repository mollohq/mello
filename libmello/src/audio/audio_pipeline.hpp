#pragma once
#include "audio_capture.hpp"
#include "audio_playback.hpp"
#include "opus_codec.hpp"
#include "noise_suppressor.hpp"
#include "echo_canceller.hpp"
#include "jitter_buffer.hpp"
#include "device_enumerator.hpp"
#include "vad.hpp"
#include "../util/ring_buffer.hpp"
#ifdef _WIN32
#include "audio_session_win.hpp"
#endif
#include <mutex>
#include <vector>
#include <queue>
#include <atomic>
#include <cstdint>
#include <functional>
#include <unordered_map>
#include <string>
#include <memory>

namespace mello::audio {

struct EncodedPacket {
    std::vector<uint8_t> data;
    uint32_t sequence;
};

class AudioPipeline {
public:
    AudioPipeline();
    ~AudioPipeline();

    bool initialize();
    void shutdown();

    bool start_capture();
    void stop_capture();

    void set_mute(bool muted);
    void set_deafen(bool deafened);
    void set_input_volume(float vol) { input_gain_.store(vol, std::memory_order_relaxed); }
    void set_output_volume(float vol) { output_gain_.store(vol, std::memory_order_relaxed); }
    float input_volume() const { return input_gain_.load(std::memory_order_relaxed); }
    float output_volume() const { return output_gain_.load(std::memory_order_relaxed); }
    void set_echo_cancellation(bool enabled) { echo_canceller_.set_aec_enabled(enabled); }
    void set_agc(bool enabled) { echo_canceller_.set_agc_enabled(enabled); }
    bool echo_cancellation_enabled() const { return echo_canceller_.aec_enabled(); }
    bool agc_enabled() const { return echo_canceller_.agc_enabled(); }
    bool noise_suppression_enabled() const { return noise_suppressor_.is_enabled(); }
    uint32_t aec_capture_frames() const { return echo_canceller_.capture_frames(); }
    uint32_t aec_render_frames() const { return echo_canceller_.render_frames(); }
    bool is_muted() const { return muted_; }
    bool is_deafened() const { return deafened_; }

    int get_packet(uint8_t* buffer, int buffer_size);
    void feed_packet(const char* peer_id, const uint8_t* data, int size);

    bool is_capturing() const { return capturing_; }
    bool is_speaking() const { return vad_.is_speaking(); }
    float speech_probability() const { return vad_.probability(); }
    float rnnoise_probability() const { return noise_suppressor_.speech_probability(); }
    float input_level() const { return input_level_.load(std::memory_order_relaxed); }
    uint32_t packets_encoded() const { return sequence_; }
    int active_streams() const { return active_streams_.load(std::memory_order_relaxed); }
    int underrun_count() const { return underrun_count_.load(std::memory_order_relaxed); }
    int rtp_recv_total() const { return rtp_recv_total_.load(std::memory_order_relaxed); }
    float pipeline_delay_ms() const;

    using VadCallback = std::function<void(bool speaking)>;
    void set_vad_callback(VadCallback cb) { vad_.set_callback(std::move(cb)); }

    AudioDeviceEnumerator& device_enumerator();
    bool set_capture_device(const char* device_id);
    bool set_playback_device(const char* device_id);

private:
    void on_captured_audio(const int16_t* samples, size_t count);
#ifdef _WIN32
    void apply_session(AudioPlayback* pb);
#endif

    size_t mix_output(int16_t* out, size_t count);

    std::unique_ptr<AudioCapture> capture_;
    std::unique_ptr<AudioPlayback> playback_;
#ifdef _WIN32
    std::unique_ptr<AudioSessionWin> session_win_;
#endif
    OpusEnc encoder_;
    NoiseSuppressor noise_suppressor_;
    EchoCanceller echo_canceller_;
    VoiceActivityDetector vad_;
    std::unordered_map<std::string, OpusDec> decoders_;
    std::unordered_map<std::string, JitterBuffer> jitter_buffers_;
    std::unique_ptr<AudioDeviceEnumerator> device_enum_;

    // Per-peer playback ring buffers for mixing
    std::unordered_map<std::string, std::unique_ptr<util::RingBuffer<int16_t>>> peer_buffers_;
    mutable std::mutex peer_buffers_mutex_;
    std::atomic<int> active_streams_{0};
    std::atomic<int> underrun_count_{0};
    std::atomic<int> rtp_recv_total_{0};

    std::vector<int16_t> capture_accum_;
    std::mutex accum_mutex_;

    std::queue<EncodedPacket> outgoing_;
    std::mutex outgoing_mutex_;
    uint32_t sequence_ = 0;

    std::atomic<bool> muted_{false};
    std::atomic<bool> deafened_{false};
    std::atomic<bool> capturing_{false};
    std::atomic<float> input_level_{0.0f};
    std::atomic<float> input_gain_{1.0f};
    std::atomic<float> output_gain_{1.0f};
    bool initialized_ = false;
};

} // namespace mello::audio
