#include "echo_canceller.hpp"
#include "../util/log.hpp"
#include "modules/audio_processing/include/audio_processing.h"
#include <cmath>

namespace mello::audio {

static webrtc::AudioProcessing::Config::NoiseSuppression::Level to_webrtc_ns_level(
    WebRtcNsLevel level) {
    switch (level) {
        case WebRtcNsLevel::Low:
            return webrtc::AudioProcessing::Config::NoiseSuppression::Level::kLow;
        case WebRtcNsLevel::Moderate:
            return webrtc::AudioProcessing::Config::NoiseSuppression::Level::kModerate;
        case WebRtcNsLevel::High:
            return webrtc::AudioProcessing::Config::NoiseSuppression::Level::kHigh;
        case WebRtcNsLevel::VeryHigh:
            return webrtc::AudioProcessing::Config::NoiseSuppression::Level::kVeryHigh;
        case WebRtcNsLevel::Off:
        default:
            return webrtc::AudioProcessing::Config::NoiseSuppression::Level::kModerate;
    }
}

EchoCanceller::EchoCanceller() = default;

EchoCanceller::~EchoCanceller() {
    shutdown();
}

bool EchoCanceller::initialize(int sample_rate, int channels) {
    if (apm_) shutdown();

    apm_ = webrtc::AudioProcessingBuilder().Create();
    if (!apm_) {
        MELLO_LOG_ERROR("aec", "failed to create AudioProcessing");
        return false;
    }

    sample_rate_ = sample_rate;
    channels_ = channels;
    apply_config();

    webrtc::StreamConfig stream_cfg(sample_rate, channels);
    webrtc::ProcessingConfig proc_cfg;
    proc_cfg.input_stream() = stream_cfg;
    proc_cfg.output_stream() = stream_cfg;
    proc_cfg.reverse_input_stream() = stream_cfg;
    proc_cfg.reverse_output_stream() = stream_cfg;

    int err = apm_->Initialize(proc_cfg);
    if (err != 0) {
        MELLO_LOG_ERROR("aec", "APM Initialize failed (error %d)", err);
        delete apm_;
        apm_ = nullptr;
        return false;
    }

    render_scratch_.resize(APM_FRAME_SIZE);

    MELLO_LOG_INFO(
        "aec",
        "initialized (rate=%d, ch=%d, aec=%d, agc=%d, ns_level=%d, transient=%d, hpf=%d)",
        sample_rate,
        channels,
        aec_enabled_.load(),
        agc_enabled_.load(),
        ns_level_.load(),
        transient_suppression_enabled_.load(),
        high_pass_filter_enabled_.load());
    return true;
}

void EchoCanceller::shutdown() {
    if (apm_) {
        delete apm_;
        apm_ = nullptr;
        MELLO_LOG_INFO("aec", "shut down");
    }
}

void EchoCanceller::apply_config() {
    if (!apm_) return;

    webrtc::AudioProcessing::Config cfg;
    cfg.echo_canceller.enabled = aec_enabled_.load(std::memory_order_relaxed);
    cfg.echo_canceller.mobile_mode = false;
    cfg.echo_canceller.enforce_high_pass_filtering =
        high_pass_filter_enabled_.load(std::memory_order_relaxed);
    cfg.gain_controller2.enabled = agc_enabled_.load(std::memory_order_relaxed);
    cfg.gain_controller2.adaptive_digital.enabled = true;
    WebRtcNsLevel ns_level =
        static_cast<WebRtcNsLevel>(ns_level_.load(std::memory_order_relaxed));
    cfg.noise_suppression.enabled = ns_level != WebRtcNsLevel::Off;
    cfg.noise_suppression.level = to_webrtc_ns_level(ns_level);
    cfg.transient_suppression.enabled =
        transient_suppression_enabled_.load(std::memory_order_relaxed);
    cfg.high_pass_filter.enabled = high_pass_filter_enabled_.load(std::memory_order_relaxed);
    cfg.pre_amplifier.enabled = false;
    cfg.voice_detection.enabled = false;
    cfg.residual_echo_detector.enabled = true;

    apm_->ApplyConfig(cfg);
}

static float rms_i16(const int16_t* buf, int n) {
    double sum = 0.0;
    for (int i = 0; i < n; ++i) {
        double s = buf[i] / 32768.0;
        sum += s * s;
    }
    return static_cast<float>(std::sqrt(sum / n));
}

void EchoCanceller::process_capture(int16_t* samples, int count) {
    if (!apm_ || (!aec_enabled_.load(std::memory_order_relaxed) &&
                  !agc_enabled_.load(std::memory_order_relaxed))) {
        return;
    }

    uint32_t frame_num = capture_frames_.load(std::memory_order_relaxed);
    bool should_log = (frame_num % 500) == 0;

    float pre_rms = 0.0f;
    if (should_log) {
        pre_rms = rms_i16(samples, count);
    }

    webrtc::StreamConfig stream_cfg(sample_rate_, channels_);

    for (int offset = 0; offset + APM_FRAME_SIZE <= count; offset += APM_FRAME_SIZE) {
        int err = apm_->ProcessStream(
            samples + offset, stream_cfg, stream_cfg, samples + offset);
        if (err != 0) {
            MELLO_LOG_WARN("aec", "ProcessStream error %d", err);
        }
        capture_frames_.fetch_add(1, std::memory_order_relaxed);
    }

    if (should_log) {
        float post_rms = rms_i16(samples, count);
        MELLO_LOG_DEBUG("aec", "capture: pre_rms=%.4f post_rms=%.4f ratio=%.2f frames=%u",
                        pre_rms, post_rms,
                        pre_rms > 0.0001f ? post_rms / pre_rms : 0.0f,
                        capture_frames_.load(std::memory_order_relaxed));
    }
}

void EchoCanceller::process_render(const int16_t* samples, int count) {
    if (!apm_ || !aec_enabled_.load(std::memory_order_relaxed)) {
        return;
    }

    uint32_t frame_num = render_frames_.load(std::memory_order_relaxed);
    bool should_log = (frame_num % 500) == 0;

    if (should_log) {
        float rms = rms_i16(samples, count);
        MELLO_LOG_DEBUG("aec", "render: rms=%.4f count=%d frames=%u",
                        rms, count, frame_num);
    }

    webrtc::StreamConfig stream_cfg(sample_rate_, channels_);

    for (int offset = 0; offset + APM_FRAME_SIZE <= count; offset += APM_FRAME_SIZE) {
        int err = apm_->ProcessReverseStream(
            samples + offset, stream_cfg, stream_cfg,
            render_scratch_.data());
        if (err != 0) {
            MELLO_LOG_WARN("aec", "ProcessReverseStream error %d", err);
        }
        render_frames_.fetch_add(1, std::memory_order_relaxed);
    }
}

void EchoCanceller::set_aec_enabled(bool enabled) {
    aec_enabled_.store(enabled, std::memory_order_relaxed);
    apply_config();
    MELLO_LOG_INFO("aec", "AEC %s", enabled ? "enabled" : "disabled");
}

void EchoCanceller::set_agc_enabled(bool enabled) {
    agc_enabled_.store(enabled, std::memory_order_relaxed);
    apply_config();
    MELLO_LOG_INFO("aec", "AGC %s", enabled ? "enabled" : "disabled");
}

void EchoCanceller::set_noise_suppression_level(WebRtcNsLevel level) {
    ns_level_.store(static_cast<int>(level), std::memory_order_relaxed);
    apply_config();
    MELLO_LOG_INFO("aec", "WebRTC noise suppression level set to %d", static_cast<int>(level));
}

void EchoCanceller::set_transient_suppression_enabled(bool enabled) {
    transient_suppression_enabled_.store(enabled, std::memory_order_relaxed);
    apply_config();
    MELLO_LOG_INFO("aec", "Transient suppression %s", enabled ? "enabled" : "disabled");
}

void EchoCanceller::set_high_pass_filter_enabled(bool enabled) {
    high_pass_filter_enabled_.store(enabled, std::memory_order_relaxed);
    apply_config();
    MELLO_LOG_INFO("aec", "High-pass filter %s", enabled ? "enabled" : "disabled");
}

} // namespace mello::audio
