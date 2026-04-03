#pragma once
#include "audio_capture.hpp"
#include "audio_playback.hpp"
#include "opus_codec.hpp"
#include "noise_suppressor.hpp"
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

    std::unique_ptr<AudioCapture> capture_;
    std::unique_ptr<AudioPlayback> playback_;
#ifdef _WIN32
    std::unique_ptr<AudioSessionWin> session_win_;
#endif
    OpusEnc encoder_;
    NoiseSuppressor noise_suppressor_;
    VoiceActivityDetector vad_;
    std::unordered_map<std::string, OpusDec> decoders_;
    std::unordered_map<std::string, JitterBuffer> jitter_buffers_;
    std::unique_ptr<AudioDeviceEnumerator> device_enum_;

    std::vector<int16_t> capture_accum_;
    std::mutex accum_mutex_;

    std::queue<EncodedPacket> outgoing_;
    std::mutex outgoing_mutex_;
    uint32_t sequence_ = 0;

    std::atomic<bool> muted_{false};
    std::atomic<bool> deafened_{false};
    std::atomic<bool> capturing_{false};
    std::atomic<float> input_level_{0.0f};
    bool initialized_ = false;
};

} // namespace mello::audio
