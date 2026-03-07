#pragma once
#include <rnnoise.h>
#include <cstdint>
#include <vector>

namespace mello::audio {

// RNNoise operates on 480-sample (10ms at 48kHz) float frames.
static constexpr int RNNOISE_FRAME_SIZE = 480;

class NoiseSuppressor {
public:
    NoiseSuppressor();
    ~NoiseSuppressor();

    bool initialize();
    void shutdown();

    // Process PCM samples in-place. Handles buffering to 480-sample chunks.
    void process(int16_t* samples, int count);

    void set_enabled(bool enabled) { enabled_ = enabled; }
    bool is_enabled() const { return enabled_; }

    // Returns the speech probability from the last processed frame [0.0, 1.0]
    float speech_probability() const { return speech_prob_; }

private:
    DenoiseState* state_ = nullptr;
    bool enabled_ = true;
    float speech_prob_ = 0.0f;
    std::vector<float> accum_;
};

} // namespace mello::audio
