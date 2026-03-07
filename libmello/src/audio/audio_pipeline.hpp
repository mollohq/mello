#pragma once
#include "capture_wasapi.hpp"
#include "playback_wasapi.hpp"
#include "opus_codec.hpp"
#include "noise_suppressor.hpp"
#include "jitter_buffer.hpp"
#include "../util/ring_buffer.hpp"
#include <mutex>
#include <vector>
#include <queue>
#include <atomic>
#include <cstdint>
#include <functional>
#include <unordered_map>
#include <string>

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

    // Get next encoded packet to send to peers. Returns size or 0 if none.
    int get_packet(uint8_t* buffer, int buffer_size);

    // Feed a received encoded packet from a peer for playback.
    void feed_packet(const char* peer_id, const uint8_t* data, int size);

    bool is_capturing() const { return capturing_; }
    bool is_speaking() const { return speaking_; }
    float speech_probability() const { return speech_prob_; }

    using VadCallback = std::function<void(bool speaking)>;
    void set_vad_callback(VadCallback cb) { vad_callback_ = std::move(cb); }

private:
    void on_captured_audio(const int16_t* samples, size_t count);
    void update_vad(float prob);

    WasapiCapture capture_;
    WasapiPlayback playback_;
    OpusEnc encoder_;
    NoiseSuppressor noise_suppressor_;
    std::unordered_map<std::string, OpusDec> decoders_;
    std::unordered_map<std::string, JitterBuffer> jitter_buffers_;

    // Accumulation buffer for capture: we need exactly FRAME_SIZE samples before encoding
    std::vector<int16_t> capture_accum_;
    std::mutex accum_mutex_;

    // Queue of encoded packets ready to send
    std::queue<EncodedPacket> outgoing_;
    std::mutex outgoing_mutex_;
    uint32_t sequence_ = 0;

    std::atomic<bool> muted_{false};
    std::atomic<bool> deafened_{false};
    std::atomic<bool> capturing_{false};
    std::atomic<bool> speaking_{false};
    float speech_prob_ = 0.0f;
    bool was_speaking_ = false;
    int vad_holdover_ = 0;
    VadCallback vad_callback_;
    bool initialized_ = false;
};

} // namespace mello::audio
