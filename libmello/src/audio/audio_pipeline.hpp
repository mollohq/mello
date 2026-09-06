#pragma once
#include "audio_capture.hpp"
#include "audio_playback.hpp"
#include "opus_codec.hpp"
#include "noise_suppressor.hpp"
#include "echo_canceller.hpp"
#include "echo_suppressor.hpp"
#include "jitter_buffer.hpp"
#include "device_enumerator.hpp"
#include "clip_buffer.hpp"
#include "clip_encoder.hpp"
#include "vad.hpp"
#include "../util/ring_buffer.hpp"
#ifdef _WIN32
#include "audio_session_win.hpp"
#endif
#include <mutex>
#include <vector>
#include <queue>
#include <deque>
#include <array>
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

enum class NsMode {
    Off = 0,
    Rnnoise = 1,
    WebRtcLow = 2,
    WebRtcModerate = 3,
    WebRtcHigh = 4,
    WebRtcVeryHigh = 5,
};

class AudioPipeline {
public:
    AudioPipeline();
    ~AudioPipeline();

    bool initialize();
    void shutdown();

    bool start_capture();
    void stop_capture();
    bool start_capture_inject();
    void stop_capture_inject();
    void inject_capture(const int16_t* samples, int count);

    void set_mute(bool muted);
    void set_deafen(bool deafened);
    void set_push_to_talk(bool enabled);
    void set_input_volume(float vol) { input_gain_.store(vol, std::memory_order_relaxed); }
    void set_output_volume(float vol) { output_gain_.store(vol, std::memory_order_relaxed); }
    float input_volume() const { return input_gain_.load(std::memory_order_relaxed); }
    float output_volume() const { return output_gain_.load(std::memory_order_relaxed); }
    void set_echo_cancellation(bool enabled);
    void set_agc(bool enabled) { echo_canceller_.set_agc_enabled(enabled); }
    /// Neural residual-echo suppressor (flag-off rollout). Soft dependency:
    /// missing model degrades to passthrough, never blocks audio.
    void set_echo_suppression(bool enabled) { echo_suppressor_.set_enabled(enabled); }
    bool echo_suppression_enabled() const { return echo_suppressor_.enabled(); }
    void set_noise_suppression(bool enabled) { set_ns_mode(enabled ? NsMode::Rnnoise : NsMode::Off); }
    void set_ns_mode(NsMode mode);
    NsMode ns_mode() const { return static_cast<NsMode>(ns_mode_.load(std::memory_order_relaxed)); }
    void set_transient_suppression(bool enabled);
    void set_high_pass_filter(bool enabled);
    bool echo_cancellation_enabled() const { return echo_canceller_.aec_enabled(); }
    bool agc_enabled() const { return echo_canceller_.agc_enabled(); }
    bool noise_suppression_enabled() const { return ns_mode() != NsMode::Off; }
    bool transient_suppression_enabled() const {
        return transient_suppression_enabled_.load(std::memory_order_relaxed);
    }
    bool high_pass_filter_enabled() const {
        return high_pass_filter_enabled_.load(std::memory_order_relaxed);
    }
    uint32_t aec_capture_frames() const { return echo_canceller_.capture_frames(); }
    uint32_t aec_render_frames() const { return echo_canceller_.render_frames(); }
    bool is_muted() const { return muted_; }
    bool is_deafened() const { return deafened_; }

    int get_packet(uint8_t* buffer, int buffer_size);
    void feed_packet(const char* peer_id, const uint8_t* data, int size);

    bool is_capturing() const { return capturing_; }
    bool is_speaking() const {
        return push_to_talk_mode_.load(std::memory_order_relaxed)
                   ? false
                   : vad_.is_speaking();
    }
    float speech_probability() const {
        return push_to_talk_mode_.load(std::memory_order_relaxed) ? 0.0f
                                                                  : vad_.probability();
    }
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
    // Returns: 0 = failed, 1 = ok, 2 = fell back to default device
    int set_capture_device(const char* device_id);
    int set_playback_device(const char* device_id);

    void start_clip_buffer();
    void stop_clip_buffer();
    bool clip_buffer_active() const;
    bool clip_capture(float seconds, const std::string& output_path);
    bool play_clip(const std::string& wav_path);
    bool play_mp4(const std::string& mp4_path);
    void stop_clip_playback();
    bool clip_is_playing() const;
    void clip_playback_progress(uint64_t& position_samples, uint64_t& total_samples, uint32_t& sample_rate) const;
    void clip_pause();
    void clip_resume();
    void clip_seek(uint64_t position_samples);

private:
    void on_captured_audio(const int16_t* samples, size_t count);
    void process_and_encode_frame(int16_t* frame);
    void reset_speech_gate_state();
    void clear_remote_streams();
    /// (Re)build the capture+playback backend pair for the desired
    /// voice-processing state (macOS: VPIO duplex vs plain HAL pair) and
    /// restart what was running. Falls back to the plain pair when the
    /// duplex unit fails to initialize.
    void switch_audio_backend(bool voice_processing);
    const char* current_capture_device_id() const {
        return capture_device_id_.empty() ? nullptr : capture_device_id_.c_str();
    }
    const char* current_playback_device_id() const {
        return playback_device_id_.empty() ? nullptr : playback_device_id_.c_str();
    }
#ifdef __APPLE__
    /// Install a live VPIO duplex pair for the stored device ids. Returns
    /// false when the unit fails (caller falls back to the plain pair).
    bool activate_vpio_pair();
    void activate_plain_pair();
#endif
    /// Recompute APM stream-delay hint from device latencies plus jitter
    /// depth. Called on init and device switches (not per-frame: the
    /// estimator converges from a close start; per-frame jitter tracking
    /// is future work — see Windows handoff TODO).
    void refresh_stream_delay_hint();
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
    EchoSuppressor echo_suppressor_;
    VoiceActivityDetector vad_;
    std::unordered_map<std::string, OpusDec> decoders_;
    std::unordered_map<std::string, bool> decoder_primed_;
    std::unordered_map<std::string, uint32_t> last_decoded_seq_;
    std::unordered_map<std::string, JitterBuffer> jitter_buffers_;
    std::unique_ptr<AudioDeviceEnumerator> device_enum_;

    // Per-peer playback ring buffers for mixing
    std::unordered_map<std::string, std::unique_ptr<util::RingBuffer<int16_t>>> peer_buffers_;
    mutable std::mutex peer_buffers_mutex_;
    std::atomic<int> active_streams_{0};
    std::atomic<int> underrun_count_{0};
    std::atomic<int> rtp_recv_total_{0};

    // Windowed underrun health (audio-thread only; no atomics needed).
    int underrun_window_count_ = 0;
    int64_t underrun_window_start_ms_ = 0;
    int64_t last_underrun_warn_ms_ = 0;

    std::vector<int16_t> capture_accum_;
    std::deque<std::array<int16_t, FRAME_SIZE>> speech_pre_roll_;
    std::mutex accum_mutex_;

    std::queue<EncodedPacket> outgoing_;
    std::mutex outgoing_mutex_;
    uint32_t sequence_ = 0;

    std::atomic<bool> muted_{false};
    std::atomic<bool> deafened_{false};
    std::atomic<bool> push_to_talk_mode_{false};
    std::atomic<bool> capturing_{false};
    std::atomic<float> input_level_{0.0f};
    std::atomic<float> input_gain_{1.0f};
    std::atomic<float> output_gain_{1.0f};
    std::atomic<int> ns_mode_{static_cast<int>(NsMode::Rnnoise)};
    std::atomic<bool> transient_suppression_enabled_{false};
    std::atomic<bool> high_pass_filter_enabled_{false};
    std::atomic<bool> capture_inject_mode_{false};
    // Desired capture backend on macOS (echo toggle position). The actual
    // backend is reported by AudioCapture::provides_echo_cancellation().
    std::atomic<bool> voice_processing_capture_{false};
    // Actual backend echo state, cached on the main thread at capture start
    // (the audio thread must not touch the capture_ pointer: device
    // switches can replace it mid-callback).
    std::atomic<bool> backend_cancels_echo_{false};
    // Last requested devices ("empty" = default). Backend and device
    // switches re-open the stored pair.
    std::string capture_device_id_;
    std::string playback_device_id_;
    float noise_floor_rms_ = 0.001f;
    int candidate_hangover_frames_ = 0;
    int speech_hangover_frames_ = 0;
    bool speech_gate_active_ = false;

    uint32_t get_pkt_ctr_ = 0;

    bool initialized_ = false;

    std::unique_ptr<ClipBuffer> clip_buffer_;
    std::unique_ptr<util::RingBuffer<int16_t>> local_clip_ring_;

    // Clip playback: retained PCM with atomic position for lock-free read from audio thread
    std::vector<int16_t> clip_playback_pcm_;
    std::atomic<size_t> clip_playback_pos_{0};
    size_t clip_playback_total_{0};
    std::atomic<bool> clip_playing_{false};
    std::atomic<bool> clip_paused_{false};
};

} // namespace mello::audio
