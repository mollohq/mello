#include "stream_audio_pipeline.hpp"
#include "../util/log.hpp"
#include <chrono>
#include <cmath>
#include <cstring>

#ifdef _WIN32
#include "capture_wasapi_loopback.hpp"
#include "playback_wasapi.hpp"
#endif

namespace mello::audio {

StreamAudioHostPipeline::StreamAudioHostPipeline() = default;

StreamAudioHostPipeline::~StreamAudioHostPipeline() {
    stop();
}

bool StreamAudioHostPipeline::start(PacketCallback callback) {
    if (!callback) return false;
    stop();

#if !defined(_WIN32) && !defined(__APPLE__)
    (void)callback;
    MELLO_LOG_WARN("stream_audio", "host pipeline not implemented on this platform");
    return false;
#else

#ifdef _WIN32
    // Windows opens its own loopback device. macOS has none: ScreenCaptureKit
    // delivers game audio on the video stream, and mello.cpp routes it into
    // feed_float_pcm once this pipeline exists.
    capture_ = std::make_unique<WasapiLoopbackCapture>();
    if (!capture_->initialize()) {
        MELLO_LOG_ERROR("stream_audio", "loopback capture init failed");
        capture_.reset();
        return false;
    }
#endif

    if (!encoder_.initialize(
            STREAM_AUDIO_SAMPLE_RATE,
            STREAM_AUDIO_CHANNELS,
            STREAM_AUDIO_BITRATE,
            OpusApplication::Audio)) {
        MELLO_LOG_ERROR("stream_audio", "Opus encoder init failed");
#ifdef _WIN32
        capture_.reset();
#endif
        return false;
    }

    {
        std::lock_guard<std::mutex> lock(pcm_mutex_);
        callback_ = std::move(callback);
        pcm_accum_.clear();
    }
    frame_index_ = 0;
    warned_sample_rate_ = false;

#ifdef _WIN32
    if (!capture_->start([this](const int16_t* samples, size_t count) {
            on_pcm(samples, count);
        })) {
        MELLO_LOG_ERROR("stream_audio", "loopback capture start failed");
        std::lock_guard<std::mutex> lock(pcm_mutex_);
        callback_ = nullptr;
        capture_.reset();
        return false;
    }
#endif

    MELLO_LOG_INFO("stream_audio", "host pipeline started");
    return true;
#endif
}

void StreamAudioHostPipeline::stop() {
#ifdef _WIN32
    if (capture_) {
        capture_->stop();
        capture_.reset();
    }
#endif
    // Under the lock: on macOS the ScreenCaptureKit audio queue can still be
    // inside feed_float_pcm while the caller stops the stream.
    std::lock_guard<std::mutex> lock(pcm_mutex_);
    callback_ = nullptr;
    pcm_accum_.clear();
}

void StreamAudioHostPipeline::feed_float_pcm(const float* samples, uint32_t frame_count,
                                             uint32_t channels, uint32_t sample_rate) {
    if (!samples || frame_count == 0 || channels == 0) return;

    // The encoder is fixed at 48 kHz and there is no resampler in libmello.
    // ScreenCaptureKit is configured for 48 kHz, so a mismatch means the OS
    // ignored the request — drop rather than emit pitch-shifted audio.
    if (sample_rate != static_cast<uint32_t>(STREAM_AUDIO_SAMPLE_RATE)) {
        if (!warned_sample_rate_) {
            warned_sample_rate_ = true;
            MELLO_LOG_WARN("stream_audio",
                "game audio at %u Hz, expected %d — dropping",
                sample_rate, STREAM_AUDIO_SAMPLE_RATE);
        }
        return;
    }

    // Map onto the encoder's stereo layout: mono duplicates across both sides,
    // more than two channels keeps the front pair rather than downmixing.
    float_conv_.resize(static_cast<size_t>(frame_count) * STREAM_AUDIO_CHANNELS);
    for (uint32_t i = 0; i < frame_count; ++i) {
        for (uint32_t ch = 0; ch < STREAM_AUDIO_CHANNELS; ++ch) {
            const uint32_t src_ch = (channels == 1) ? 0 : ch;
            float v = samples[static_cast<size_t>(i) * channels + src_ch];
            if (v > 1.0f) v = 1.0f;
            if (v < -1.0f) v = -1.0f;
            float_conv_[static_cast<size_t>(i) * STREAM_AUDIO_CHANNELS + ch] =
                static_cast<int16_t>(std::lrintf(v * 32767.0f));
        }
    }

    on_pcm(float_conv_.data(), float_conv_.size());
}

void StreamAudioHostPipeline::on_pcm(const int16_t* samples, size_t count) {
    std::lock_guard<std::mutex> lock(pcm_mutex_);
    if (!callback_ || count == 0) return;

    pcm_accum_.insert(pcm_accum_.end(), samples, samples + count);

    const size_t frame_samples =
        static_cast<size_t>(STREAM_AUDIO_FRAME_SAMPLES) * STREAM_AUDIO_CHANNELS;
    while (pcm_accum_.size() >= frame_samples) {
        const int encoded = encoder_.encode(
            pcm_accum_.data(),
            STREAM_AUDIO_FRAME_SAMPLES,
            encode_buf_,
            static_cast<int>(sizeof(encode_buf_)));
        if (encoded > 0) {
            const uint64_t ts_us = frame_index_ * 20000;
            callback_(encode_buf_, encoded, ts_us);
            ++frame_index_;
        } else if (encoded < 0) {
            // encoded == 0 is Opus DTX ("nothing to transmit") and is normal.
            // A negative value is a real encoder error; voice logs it the same
            // way in audio_pipeline.cpp, and without this it is invisible.
            MELLO_LOG_WARN("stream_audio", "opus encode error: %d", encoded);
        }
        pcm_accum_.erase(pcm_accum_.begin(),
                         pcm_accum_.begin() + static_cast<std::ptrdiff_t>(frame_samples));
    }
}

StreamAudioPlayout::StreamAudioPlayout() = default;

StreamAudioPlayout::~StreamAudioPlayout() {
    stop();
}

bool StreamAudioPlayout::ensure_started() {
    if (started_) return true;

    if (!decoder_.initialize(STREAM_AUDIO_SAMPLE_RATE, STREAM_AUDIO_CHANNELS)) {
        MELLO_LOG_ERROR("stream_audio", "Opus decoder init failed");
        return false;
    }

    playback_ = create_audio_playback();
    if (!playback_) {
        MELLO_LOG_WARN("stream_audio", "no audio playback backend on this platform");
        return false;
    }
    // Before initialize(): backends bake the channel count into their output
    // format. Game audio is stereo; downmixing would lose the spatial mix.
    playback_->set_input_channels(STREAM_AUDIO_CHANNELS);
    if (!playback_->initialize() || !playback_->start()) {
        MELLO_LOG_ERROR("stream_audio", "playback init/start failed");
        playback_.reset();
        return false;
    }
    started_ = true;
    MELLO_LOG_INFO("stream_audio", "viewer playout started");
    return true;
}

bool StreamAudioPlayout::feed_packet(const uint8_t* data, int size) {
    if (!data || size <= 0) return false;
    if (!ensure_started()) return false;

    decode_pcm_.resize(static_cast<size_t>(STREAM_AUDIO_FRAME_SAMPLES) * STREAM_AUDIO_CHANNELS);
    const int decoded = decoder_.decode(
        data,
        size,
        decode_pcm_.data(),
        STREAM_AUDIO_FRAME_SAMPLES);
    if (decoded <= 0) {
        return false;
    }

    const size_t sample_count =
        static_cast<size_t>(decoded) * static_cast<size_t>(STREAM_AUDIO_CHANNELS);
    const size_t written = playback_->feed(decode_pcm_.data(), sample_count);
    ++packets_fed_;
    if (written < sample_count) {
        ++underruns_;
    }
    return true;
}

void StreamAudioPlayout::stop() {
    if (playback_) {
        playback_->stop();
        playback_.reset();
    }
    started_ = false;
}

} // namespace mello::audio
