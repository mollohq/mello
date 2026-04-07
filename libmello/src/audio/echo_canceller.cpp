#include "echo_canceller.hpp"
#include "../util/log.hpp"
#include "modules/audio_processing/include/audio_processing.h"

namespace mello::audio {

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

    MELLO_LOG_INFO("aec", "initialized (rate=%d, ch=%d, aec=%d, agc=%d)",
                   sample_rate, channels, aec_enabled_.load(), agc_enabled_.load());
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
    cfg.gain_controller2.enabled = agc_enabled_.load(std::memory_order_relaxed);
    cfg.gain_controller2.adaptive_digital.enabled = true;
    cfg.noise_suppression.enabled = false;
    cfg.high_pass_filter.enabled = false;
    cfg.pre_amplifier.enabled = false;
    cfg.voice_detection.enabled = false;
    cfg.residual_echo_detector.enabled = true;

    apm_->ApplyConfig(cfg);
}

void EchoCanceller::process_capture(int16_t* samples, int count) {
    if (!apm_ || (!aec_enabled_.load(std::memory_order_relaxed) &&
                  !agc_enabled_.load(std::memory_order_relaxed))) {
        return;
    }

    webrtc::StreamConfig stream_cfg(sample_rate_, channels_);

    for (int offset = 0; offset + APM_FRAME_SIZE <= count; offset += APM_FRAME_SIZE) {
        int err = apm_->ProcessStream(
            samples + offset, stream_cfg, stream_cfg, samples + offset);
        if (err != 0) {
            MELLO_LOG_WARN("aec", "ProcessStream error %d", err);
        }
    }
}

void EchoCanceller::process_render(const int16_t* samples, int count) {
    if (!apm_ || !aec_enabled_.load(std::memory_order_relaxed)) {
        return;
    }

    webrtc::StreamConfig stream_cfg(sample_rate_, channels_);

    for (int offset = 0; offset + APM_FRAME_SIZE <= count; offset += APM_FRAME_SIZE) {
        int err = apm_->ProcessReverseStream(
            samples + offset, stream_cfg, stream_cfg,
            const_cast<int16_t*>(samples + offset));
        if (err != 0) {
            MELLO_LOG_WARN("aec", "ProcessReverseStream error %d", err);
        }
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

} // namespace mello::audio
