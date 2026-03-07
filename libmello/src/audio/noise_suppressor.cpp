#include "noise_suppressor.hpp"
#include <algorithm>
#include <cmath>

namespace mello::audio {

NoiseSuppressor::NoiseSuppressor() = default;

NoiseSuppressor::~NoiseSuppressor() {
    shutdown();
}

bool NoiseSuppressor::initialize() {
    state_ = rnnoise_create(nullptr);
    return state_ != nullptr;
}

void NoiseSuppressor::shutdown() {
    if (state_) {
        rnnoise_destroy(state_);
        state_ = nullptr;
    }
}

void NoiseSuppressor::process(int16_t* samples, int count) {
    if (!state_ || !enabled_) return;

    // Convert int16 to float and accumulate
    for (int i = 0; i < count; ++i) {
        accum_.push_back(static_cast<float>(samples[i]));
    }

    // Process complete 480-sample frames
    int out_idx = 0;
    while (accum_.size() >= RNNOISE_FRAME_SIZE) {
        float frame[RNNOISE_FRAME_SIZE];
        std::copy(accum_.begin(), accum_.begin() + RNNOISE_FRAME_SIZE, frame);
        accum_.erase(accum_.begin(), accum_.begin() + RNNOISE_FRAME_SIZE);

        speech_prob_ = rnnoise_process_frame(state_, frame, frame);

        // Convert back to int16 and write to output
        for (int j = 0; j < RNNOISE_FRAME_SIZE && out_idx < count; ++j, ++out_idx) {
            float val = frame[j];
            val = (val < -32768.0f) ? -32768.0f : (val > 32767.0f) ? 32767.0f : val;
            samples[out_idx] = static_cast<int16_t>(val);
        }
    }
}

} // namespace mello::audio
