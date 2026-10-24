#pragma once

#include "opus_codec.hpp"
#include "voice_playout.hpp"
#include "../transport/av_sync.hpp"
#include <atomic>
#include <cstdint>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <vector>

namespace mello::audio {

class StreamAudioLink;

static constexpr int STREAM_AUDIO_SAMPLE_RATE = 48000;
static constexpr int STREAM_AUDIO_CHANNELS = 2;
static constexpr int STREAM_AUDIO_FRAME_SAMPLES = 960; // 20 ms per channel at 48 kHz
static constexpr int STREAM_AUDIO_BITRATE = 96000;

class StreamAudioHostPipeline {
public:
    using PacketCallback = std::function<void(const uint8_t* data, int size, uint64_t ts_us)>;

    StreamAudioHostPipeline();
    ~StreamAudioHostPipeline();

    /// Streaming-quality stage 7: the context's link to the host's SFU peer
    /// (stream_audio_link.hpp). Each encoded frame records its capture time
    /// in it, and the encoder takes its expected loss from it. Before
    /// start(); null: none.
    void set_link(std::shared_ptr<StreamAudioLink> link);

    /// `capture_pid` is the streamed process when capturing a game, 0 otherwise.
    /// Non-zero scopes capture to that process tree; zero scopes it to everything
    /// *except* our own process, so a desktop stream still excludes voice chat.
    /// `external_source`: open no capture device; the audio arrives through
    /// feed_float_pcm or feed_float_pcm_at (macOS, the reference source).
    bool start(PacketCallback callback, uint32_t capture_pid = 0, bool external_source = false);
    void stop();

    /// Push interleaved float PCM captured elsewhere.
    ///
    /// macOS has no loopback device to open: ScreenCaptureKit hands game audio
    /// to the same stream that carries video, so `mello.cpp` routes it here.
    /// Windows opens its own WASAPI loopback capture and never calls this.
    /// The capture time is taken as the arrival time of the last sample.
    ///
    /// Called on the capture backend's audio queue.
    void feed_float_pcm(const float* samples, uint32_t frame_count,
                        uint32_t channels, uint32_t sample_rate);

    /// The same with the capture time of the first sample (steady clock,
    /// microseconds): a source that knows it (the reference source).
    void feed_float_pcm_at(const float* samples, uint32_t frame_count, uint32_t channels,
                           uint32_t sample_rate, int64_t capture_us);

    /// OPUS_SET_PACKET_LOSS_PERC that the encoder uses now (stage 7: the
    /// link's value, from the receiver reports). For tests and stats.
    int loss_perc() const { return loss_perc_.load(std::memory_order_relaxed); }

private:
    /// `capture_us`: capture time of the first sample, or empty to take the
    /// arrival time.
    void on_pcm(const int16_t* samples, size_t count, std::optional<int64_t> capture_us);

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
    std::shared_ptr<StreamAudioLink> link_;
    // Capture time of pcm_accum_[0]; valid while the accumulator has audio.
    int64_t accum_capture_us_ = 0;
    bool have_accum_capture_ = false;
    std::atomic<int> loss_perc_{0};

    // feed_float_pcm runs on the capture queue while stop() runs on the caller's
    // thread; both touch callback_ and pcm_accum_.
    std::mutex pcm_mutex_;
    std::vector<int16_t> float_conv_;
    bool warned_sample_rate_ = false;
};

/// Viewer playout of the game audio of one stream (streaming-quality stage
/// 7, spec 12 section 9.3): one stereo NetEQ in the voice mix
/// (GameAudioSource), fed with each RTP packet as it arrives. It plays on
/// the voice output device, through the voice output gain and deafen, with
/// its own volume.
///
/// Threads: feed_rtp on the network thread (the peer's audio track), feed_packet
/// on the caller's thread, sync_sample and set_sync_delay_ms on the receiver
/// worker (A/V sync), the rest on the caller's thread. One mutex serialises
/// them; the voice mixer pulls the source on the device thread.
class StreamAudioPlayout {
public:
    /// Makes the game-audio source in the voice mix
    /// (VoicePlayout::add_game_audio).
    using SourceFactory = std::function<std::unique_ptr<GameAudioSource>()>;
    /// The latency after the mix: the output ring and the device, in ms.
    using OutputLatencyFn = std::function<float()>;

    explicit StreamAudioPlayout(SourceFactory factory, OutputLatencyFn output_latency = {});
    ~StreamAudioPlayout();

    /// One RTP packet of the game audio (the whole packet) and its arrival
    /// time on the steady clock in microseconds. While muted the packet is
    /// dropped and true is returned.
    bool feed_rtp(const uint8_t* packet, size_t size, int64_t arrival_us);

    /// One Opus payload with no RTP header (a view without a peer): the
    /// sequence and the timestamp count the frames. While muted the packet
    /// is dropped and true is returned.
    bool feed_packet(const uint8_t* data, int size);

    /// Mute flushes: the audio NetEQ holds is dropped, so a paused stream
    /// goes silent at once, and packets that arrive while muted are dropped.
    /// Unmute plays again from the first new packet.
    void set_muted(bool muted);
    bool muted() const;

    /// Per-stream volume, 0..2 (1 = unity). Kept across mute.
    void set_volume(float gain);
    float volume() const;

    /// Leave the mix. Packets after it are refused.
    void stop();

    /// A/V sync (stage 7): the video RTP timestamp of the audio heard now,
    /// from the extension of the newest packet that had one. Empty while
    /// nothing plays or no packet carried the extension.
    std::optional<transport::AudioSyncSample> sync_sample() const;
    /// NetEQ's minimum delay for a late video.
    void set_sync_delay_ms(int delay_ms);

    struct Stats {
        uint64_t packets_fed = 0;
        uint64_t packets_dropped_muted = 0;
        /// Packets that carried the A/V sync extension.
        uint64_t sync_extensions = 0;
        /// NetEQ's delay plus the output latency, ms (0 while nothing plays).
        float delay_ms = 0.0f;
        /// Concealment (expand) since the view started, ms.
        double expand_ms = 0.0;
        /// Lost packets repaired from in-band FEC.
        uint64_t fec_used = 0;
        GameAudioStats source;
    };
    Stats stats() const;

    uint64_t packets_fed() const;
    uint64_t packets_dropped_muted() const;

private:
    bool insert_locked(const RtpInfo& rtp, const uint8_t* payload, size_t size,
                       int64_t arrival_ms);

    const OutputLatencyFn output_latency_;

    mutable std::mutex mutex_;
    std::unique_ptr<GameAudioSource> source_;
    bool muted_ = false;
    uint64_t packets_fed_ = 0;
    uint64_t packets_dropped_muted_ = 0;
    uint64_t sync_extensions_ = 0;
    // The A/V sync mapping of the newest packet with the extension.
    bool have_sync_ = false;
    uint32_t sync_audio_ts_ = 0;
    uint32_t sync_video_ts_ = 0;
    // feed_packet's own header.
    uint16_t plain_sequence_ = 0;
    uint32_t plain_timestamp_ = 0;
};

} // namespace mello::audio
