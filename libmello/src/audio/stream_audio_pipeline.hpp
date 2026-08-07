#pragma once

#include "opus_codec.hpp"
#include <cstdint>
#include <functional>
#include <memory>
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

    bool start(PacketCallback callback);
    void stop();

private:
    void on_pcm(const int16_t* samples, size_t count);

    std::unique_ptr<class WasapiLoopbackCapture> capture_;
    OpusEnc encoder_;
    PacketCallback callback_;
    std::vector<int16_t> pcm_accum_;
    uint8_t encode_buf_[MAX_PACKET_SIZE]{};
    uint64_t frame_index_ = 0;
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
    std::unique_ptr<class WasapiPlayback> playback_;
    std::vector<int16_t> decode_pcm_;
    bool started_ = false;
    uint64_t packets_fed_ = 0;
    uint64_t underruns_ = 0;
};

} // namespace mello::audio
