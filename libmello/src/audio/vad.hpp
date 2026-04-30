#pragma once
#define ORT_API_MANUAL_INIT
#include <onnxruntime_cxx_api.h>
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

    void feed(const int16_t* samples, int count);
    void force_silence();

    bool is_speaking() const { return speaking_; }
    float probability() const { return probability_; }

    using Callback = std::function<void(bool speaking)>;
    void set_callback(Callback cb) { callback_ = std::move(cb); }

private:
    void run_inference();
    void downsample_48_to_16(const int16_t* in, int count);

    std::unique_ptr<Ort::Env> env_;
    std::unique_ptr<Ort::SessionOptions> session_options_;
    Ort::Session* session_ = nullptr;

    std::vector<float> h_state_;           // [2, 1, 128] flattened
    std::vector<float> context_;           // last 64 samples from previous chunk
    std::vector<float> model_input_buf_;   // context + chunk = 576 samples
    int64_t sample_rate_ = VAD_SAMPLE_RATE;

    std::vector<float> accum_buf_;         // accumulates downsampled 16kHz floats
    std::atomic<bool> speaking_{false};
    float probability_ = 0.0f;
    bool was_speaking_ = false;
    bool initialized_ = false;

    Callback callback_;

    int holdover_ = 0;
    static constexpr int HOLDOVER_FRAMES = 8;
};

} // namespace mello::audio
