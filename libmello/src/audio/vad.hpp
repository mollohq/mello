#pragma once
// MELLO_IOS_NO_VAD is the documented VAD kill-switch (IOS-LIBMELLO-PORT §4.3
// fallback): define it to stub Silero out (no ORT) if ORT-iOS regresses. It is
// NOT defined by default as of Step 2 — iOS links the prebuilt static ORT. When
// set, gating the ORT include + members here keeps every TU that includes
// vad.hpp (e.g. audio_pipeline) free of ONNX Runtime. Desktop never sets it.
#ifndef MELLO_IOS_NO_VAD
#include "ort_loader.hpp"
#endif
#include <cstdint>
#include <vector>
#include <string>
#include <atomic>
#include <functional>
#include <memory>

namespace mello::audio {

static constexpr int VAD_SAMPLE_RATE = 16000;
static constexpr int VAD_CHUNK_SIZE = 512;
static constexpr int VAD_CONTEXT_SIZE = 64;
static constexpr int VAD_STATE_SIZE = 2 * 1 * 128;  // [2, 1, 128]
static constexpr float VAD_THRESHOLD = 0.35f;

class VoiceActivityDetector {
public:
    VoiceActivityDetector();
    ~VoiceActivityDetector();

    bool initialize(const std::string& model_path);
    void shutdown();

    /// Returns the number of 32 ms chunks that ran.
    int feed(const int16_t* samples, int count);
    void force_silence();
    /// Start a new run of chunks after the caller stopped feeding for a
    /// while (spec 10 section 4.1). Drops the 16 kHz samples left from the
    /// last run: they are old audio. `history` is the 48 kHz audio right
    /// before the next feed(). Its last `chunk_lead` 16 kHz samples start the
    /// next chunk and the VAD_CONTEXT_SIZE samples before them become the
    /// model context, so the first chunk ends `VAD_CHUNK_SIZE - chunk_lead`
    /// samples into the next feed. The model state stays. `count` must be a
    /// multiple of 3 and hold at least 3 * (chunk_lead + VAD_CONTEXT_SIZE)
    /// samples; otherwise the call only drops the old samples.
    void restart(const int16_t* history, int count, int chunk_lead);
    /// Decide on the latest 32 ms now, between two chunks: run the model on
    /// the last VAD_CHUNK_SIZE samples fed, from the state of the last
    /// chunk, and keep neither the new state nor the context. The result
    /// sets probability() and is_speaking() like a chunk. The pipeline calls
    /// it at the end of a frame in which no chunk completed, while the gate
    /// waits for speech, so the decision is not one frame late.
    void peek();

    bool is_speaking() const { return speaking_; }
    float probability() const { return probability_; }

    using Callback = std::function<void(bool speaking)>;
    void set_callback(Callback cb) { callback_ = std::move(cb); }

private:
    void run_inference();
    // Run the model on VAD_CONTEXT_SIZE + VAD_CHUNK_SIZE samples with
    // `state` (VAD_STATE_SIZE, updated in place). False on an error.
    bool infer(float* input, float* state, float* prob);
    void apply(float prob);
    void downsample_48_to_16(const int16_t* in, int count);

#ifndef MELLO_IOS_NO_VAD
    std::unique_ptr<OrtHandles> ort_;
    Ort::Session* session_ = nullptr;
#endif

    std::vector<float> h_state_;           // [2, 1, 128] flattened
    std::vector<float> context_;           // last 64 samples from previous chunk
    std::vector<float> model_input_buf_;   // context + chunk = 576 samples
    int64_t sample_rate_ = VAD_SAMPLE_RATE;

    std::vector<float> accum_buf_;         // accumulates downsampled 16kHz floats
    std::vector<float> recent_;            // last context + chunk samples fed, for peek()
    std::vector<float> peek_state_;
    std::atomic<bool> speaking_{false};
    float probability_ = 0.0f;
    bool was_speaking_ = false;
    bool initialized_ = false;

    Callback callback_;

    int holdover_ = 0;
    static constexpr int HOLDOVER_FRAMES = 8;
};

} // namespace mello::audio
