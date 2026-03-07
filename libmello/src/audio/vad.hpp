#pragma once
#include <cstdint>
#include <vector>
#include <string>
#include <atomic>

#ifdef MELLO_HAS_ONNX
#include <onnxruntime_cxx_api.h>
#endif

namespace mello::audio {

// Silero VAD operating on 512-sample chunks at 16kHz.
// We feed 48kHz mono and downsample internally.
static constexpr int VAD_SAMPLE_RATE = 16000;
static constexpr int VAD_WINDOW_SIZE = 512;
static constexpr float VAD_THRESHOLD = 0.5f;

class VoiceActivityDetector {
public:
    VoiceActivityDetector();
    ~VoiceActivityDetector();

    bool initialize(const std::string& model_path);
    void shutdown();

    // Feed 48kHz mono int16 samples. Updates speech probability internally.
    void feed(const int16_t* samples, int count);

    bool is_speaking() const { return speaking_; }
    float probability() const { return probability_; }

    using Callback = void(*)(void* user_data, bool speaking);
    void set_callback(Callback cb, void* user_data);

private:
    void run_inference();
    void downsample_48_to_16(const int16_t* in, int count);

#ifdef MELLO_HAS_ONNX
    Ort::Env env_;
    Ort::Session* session_ = nullptr;
    Ort::MemoryInfo mem_info_ = Ort::MemoryInfo::CreateCpu(OrtArenaAllocator, OrtMemTypeDefault);
    std::vector<float> state_h_;
    std::vector<float> state_c_;
#endif

    std::vector<float> input_buf_;
    std::atomic<bool> speaking_{false};
    float probability_ = 0.0f;
    bool was_speaking_ = false;
    bool initialized_ = false;

    Callback callback_ = nullptr;
    void* callback_ud_ = nullptr;

    // Holdover counter to prevent rapid on/off
    int holdover_ = 0;
    static constexpr int HOLDOVER_FRAMES = 8;
};

} // namespace mello::audio
