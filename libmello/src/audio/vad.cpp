#include "vad.hpp"
#include <cstring>
#include <algorithm>
#include <cmath>

namespace mello::audio {

VoiceActivityDetector::VoiceActivityDetector()
#ifdef MELLO_HAS_ONNX
    : env_(ORT_LOGGING_LEVEL_WARNING, "mello_vad")
#endif
{
}

VoiceActivityDetector::~VoiceActivityDetector() {
    shutdown();
}

bool VoiceActivityDetector::initialize(const std::string& model_path) {
#ifdef MELLO_HAS_ONNX
    try {
        Ort::SessionOptions opts;
        opts.SetIntraOpNumThreads(1);
        opts.SetGraphOptimizationLevel(GraphOptimizationLevel::ORT_ENABLE_ALL);

        std::wstring wpath(model_path.begin(), model_path.end());
        session_ = new Ort::Session(env_, wpath.c_str(), opts);

        // Silero VAD v5 state: h and c are 2x1x64
        state_h_.assign(2 * 1 * 64, 0.0f);
        state_c_.assign(2 * 1 * 64, 0.0f);

        initialized_ = true;
        return true;
    } catch (const Ort::Exception& e) {
        (void)e;
        return false;
    }
#else
    (void)model_path;
    return false;
#endif
}

void VoiceActivityDetector::shutdown() {
#ifdef MELLO_HAS_ONNX
    if (session_) {
        delete session_;
        session_ = nullptr;
    }
#endif
    initialized_ = false;
}

void VoiceActivityDetector::downsample_48_to_16(const int16_t* in, int count) {
    // Simple 3:1 decimation (48kHz -> 16kHz)
    for (int i = 0; i < count; i += 3) {
        float sample = static_cast<float>(in[i]) / 32768.0f;
        input_buf_.push_back(sample);
    }
}

void VoiceActivityDetector::feed(const int16_t* samples, int count) {
    if (!initialized_) return;

    downsample_48_to_16(samples, count);

    while (input_buf_.size() >= VAD_WINDOW_SIZE) {
        run_inference();
        input_buf_.erase(input_buf_.begin(), input_buf_.begin() + VAD_WINDOW_SIZE);
    }
}

void VoiceActivityDetector::run_inference() {
#ifdef MELLO_HAS_ONNX
    if (!session_) return;

    try {
        // Input: audio chunk [1, window_size]
        int64_t input_shape[] = {1, VAD_WINDOW_SIZE};
        auto input_tensor = Ort::Value::CreateTensor<float>(
            mem_info_, input_buf_.data(), VAD_WINDOW_SIZE, input_shape, 2);

        // State inputs: h [2, 1, 64], c [2, 1, 64]
        int64_t state_shape[] = {2, 1, 64};
        auto h_tensor = Ort::Value::CreateTensor<float>(
            mem_info_, state_h_.data(), state_h_.size(), state_shape, 3);
        auto c_tensor = Ort::Value::CreateTensor<float>(
            mem_info_, state_c_.data(), state_c_.size(), state_shape, 3);

        // Sample rate input
        int64_t sr = VAD_SAMPLE_RATE;
        int64_t sr_shape[] = {1};
        auto sr_tensor = Ort::Value::CreateTensor<int64_t>(
            mem_info_, &sr, 1, sr_shape, 1);

        const char* input_names[] = {"input", "state", "sr"};
        // Silero VAD v5 uses combined state
        // But older versions use h/c. We'll handle v5's combined state format.
        // Actually Silero VAD v5 uses: input, state (combined h+c), sr
        // Let's use the simpler approach with combined state.
        std::vector<float> combined_state(state_h_.size() + state_c_.size());
        std::copy(state_h_.begin(), state_h_.end(), combined_state.begin());
        std::copy(state_c_.begin(), state_c_.end(), combined_state.begin() + state_h_.size());

        int64_t combined_state_shape[] = {2, 1, 64};
        auto state_tensor = Ort::Value::CreateTensor<float>(
            mem_info_, combined_state.data(), 2 * 1 * 64, combined_state_shape, 3);

        std::vector<Ort::Value> inputs;
        inputs.push_back(std::move(input_tensor));
        inputs.push_back(std::move(state_tensor));
        inputs.push_back(std::move(sr_tensor));

        const char* output_names[] = {"output", "stateN"};
        auto results = session_->Run(
            Ort::RunOptions{nullptr},
            input_names, inputs.data(), inputs.size(),
            output_names, 2);

        // Output probability
        float* out_data = results[0].GetTensorMutableData<float>();
        probability_ = out_data[0];

        // Update state for next frame
        float* new_state = results[1].GetTensorMutableData<float>();
        std::copy(new_state, new_state + state_h_.size(), state_h_.begin());
        std::copy(new_state + state_h_.size(), new_state + state_h_.size() + state_c_.size(), state_c_.begin());

        // Apply threshold with holdover
        bool now_speaking = (probability_ >= VAD_THRESHOLD);
        if (now_speaking) {
            holdover_ = HOLDOVER_FRAMES;
        } else if (holdover_ > 0) {
            holdover_--;
            now_speaking = true;
        }

        if (now_speaking != was_speaking_) {
            speaking_ = now_speaking;
            was_speaking_ = now_speaking;
            if (callback_) {
                callback_(callback_ud_, now_speaking);
            }
        }
    } catch (const Ort::Exception& e) {
        (void)e;
    }
#endif
}

void VoiceActivityDetector::set_callback(Callback cb, void* user_data) {
    callback_ = cb;
    callback_ud_ = user_data;
}

} // namespace mello::audio
