#pragma once
#include <cmath>
#include <cstdint>

namespace mello::audio {

// Downward expander for the capture path. Passes speech frames at unity gain
// and attenuates non-speech frames toward a floor gain, with smoothed attack
// and release so word onsets and tails are not chopped.
//
// Why: AGC2 ramps large gain to normalize a quiet mic (e.g. -39 dBFS), then
// holds that gain across speech gaps and blasts the noise floor (measured
// +14..23 dB in the field / EchoCancellerTest). AGC applies one gain to speech
// and floor alike, so no AGC config can separate them. This expander runs after
// AGC, keyed off the same VAD the pipeline already computes, and pushes the
// pumped floor back down in the gaps while leaving speech untouched.
//
// Thread-safety: not thread-safe; owned and driven by the capture thread only.
class SpeechExpander {
public:
    // floor_gain: linear gain applied when fully closed (0.1 = -20 dB).
    // attack: per-frame smoothing toward open (fast; larger = faster).
    // release: per-frame smoothing toward closed (slow; smaller = gentler).
    // Coefficients are per 20 ms frame, in (0, 1].
    void configure(float floor_gain, float attack, float release) {
        floor_gain_ = floor_gain;
        attack_ = attack;
        release_ = release;
    }

    void reset() { gain_ = 1.0f; }

    // Applies the smoothed gain to `frame` in place. `speech_active` is the
    // pipeline's speech decision for this frame (Silero VAD / gate).
    void process(int16_t* frame, int n, bool speech_active) {
        const float target = speech_active ? 1.0f : floor_gain_;
        // Open fast, close slow: attack when raising gain, release when lowering.
        const float coeff = (target > gain_) ? attack_ : release_;
        gain_ += coeff * (target - gain_);
        if (gain_ >= 0.999f) return;  // unity: leave samples untouched
        for (int i = 0; i < n; ++i) {
            int32_t s = static_cast<int32_t>(std::lround(frame[i] * gain_));
            if (s > 32767) s = 32767;
            if (s < -32768) s = -32768;
            frame[i] = static_cast<int16_t>(s);
        }
    }

    float gain() const { return gain_; }

private:
    float gain_ = 1.0f;
    float floor_gain_ = 0.1f;   // -20 dB
    float attack_ = 0.6f;       // ~1 frame to open
    float release_ = 0.12f;     // ~150 ms to close at 20 ms/frame
};

}  // namespace mello::audio
