#pragma once

#include "audio_playback.hpp"
#include "opus_codec.hpp"
#include <cstdint>
#include <functional>
#include <memory>
#include <mutex>
#include <vector>

namespace mello::audio {

static constexpr int STREAM_AUDIO_SAMPLE_RATE = 48000;
static constexpr int STREAM_AUDIO_CHANNELS = 2;
static constexpr int STREAM_AUDIO_FRAME_SAMPLES = 960; // 20 ms per channel at 48 kHz
static constexpr int STREAM_AUDIO_BITRATE = 96000;

class StreamAudioHostPipeline {
public:
    using PacketCallback = std::function<void(const uint8_t* data, int size, uint64_t ts_us)>;

    StreamAudioHostPipeline();
    ~StreamAudioHostPipeline();

    /// `capture_pid` is the streamed process when capturing a game, 0 otherwise.
    /// Non-zero scopes capture to that process tree; zero scopes it to everything
    /// *except* our own process, so a desktop stream still excludes voice chat.
    bool start(PacketCallback callback, uint32_t capture_pid = 0);
    void stop();

    /// Push interleaved float PCM captured elsewhere.
    ///
    /// macOS has no loopback device to open: ScreenCaptureKit hands game audio
    /// to the same stream that carries video, so `mello.cpp` routes it here.
    /// Windows opens its own WASAPI loopback capture and never calls this.
    ///
    /// Called on the capture backend's audio queue.
    void feed_float_pcm(const float* samples, uint32_t frame_count,
                        uint32_t channels, uint32_t sample_rate);

private:
    void on_pcm(const int16_t* samples, size_t count);

    // Windows-only: WasapiLoopbackCapture exists only under _WIN32, and a
    // unique_ptr to an incomplete type cannot be destroyed, so the member
    // itself must be gated rather than just its uses.
#ifdef _WIN32
    std::unique_ptr<class WasapiLoopbackCapture> capture_;
#endif
    OpusEnc encoder_;
    PacketCallback callback_;
    std::vector<int16_t> pcm_accum_;
    uint8_t encode_buf_[MAX_PACKET_SIZE]{};
    uint64_t frame_index_ = 0;

    // feed_float_pcm runs on the capture queue while stop() runs on the caller's
    // thread; both touch callback_ and pcm_accum_.
    std::mutex pcm_mutex_;
    std::vector<int16_t> float_conv_;
    bool warned_sample_rate_ = false;
};

class StreamAudioPlayout {
public:
    StreamAudioPlayout();
    ~StreamAudioPlayout();

    bool feed_packet(const uint8_t* data, int size);
    void stop();

    uint64_t packets_fed() const { return packets_fed_; }
    uint64_t underruns() const { return underruns_; }

private:
    bool ensure_started();

    OpusDec decoder_;
    // Platform backend from create_audio_playback(): WASAPI on Windows,
    // CoreAudio on macOS. Held by interface so the playout path is
    // platform-neutral.
    std::unique_ptr<AudioPlayback> playback_;
    std::vector<int16_t> decode_pcm_;
    bool started_ = false;
    uint64_t packets_fed_ = 0;
    uint64_t underruns_ = 0;
};

} // namespace mello::audio
