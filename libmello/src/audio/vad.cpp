#include "vad.hpp"
#include "../util/log.hpp"
#include <cstring>
#include <algorithm>
#include <cmath>

namespace mello::audio {

VoiceActivityDetector::VoiceActivityDetector() = default;

VoiceActivityDetector::~VoiceActivityDetector() {
    shutdown();
}

bool VoiceActivityDetector::initialize(const std::string& model_path) {
#ifdef MELLO_IOS_NO_VAD
    (void)model_path;
    MELLO_LOG_WARN("vad", "Silero VAD stubbed (MELLO_IOS_NO_VAD kill-switch) — ORT not linked");
    return false;
#else
    ort_ = init_ort(model_path);
    if (!ort_) return false;
    session_ = open_ort_session(*ort_, model_path, "vad");
    if (!session_) {
        ort_.reset();
        return false;
    }
    h_state_.resize(VAD_STATE_SIZE, 0.0f);
    context_.resize(VAD_CONTEXT_SIZE, 0.0f);
    model_input_buf_.resize(VAD_CONTEXT_SIZE + VAD_CHUNK_SIZE);

    initialized_ = true;
    MELLO_LOG_INFO("vad", "Silero VAD v5 initialized (model=%s)", model_path.c_str());
    return true;
#endif  // MELLO_IOS_NO_VAD
}

void VoiceActivityDetector::shutdown() {
#ifndef MELLO_IOS_NO_VAD
    if (session_) {
        delete session_;
        session_ = nullptr;
    }
    ort_.reset();
#endif
    h_state_.clear();
    context_.clear();
    initialized_ = false;
}

void VoiceActivityDetector::downsample_48_to_16(const int16_t* in, int count) {
    for (int i = 0; i < count; i += 3) {
        float sample = static_cast<float>(in[i]) / 32768.0f;
        accum_buf_.push_back(sample);
    }
}

void VoiceActivityDetector::feed(const int16_t* samples, int count) {
    if (!initialized_) return;

    downsample_48_to_16(samples, count);

    while (accum_buf_.size() >= static_cast<size_t>(VAD_CHUNK_SIZE)) {
        run_inference();
        accum_buf_.erase(accum_buf_.begin(), accum_buf_.begin() + VAD_CHUNK_SIZE);
    }
}

void VoiceActivityDetector::force_silence() {
    holdover_ = 0;
    probability_ = 0.0f;
    if (was_speaking_) {
        speaking_ = false;
        was_speaking_ = false;
        if (callback_) {
            callback_(false);
        }
    }
}

void VoiceActivityDetector::run_inference() {
#ifndef MELLO_IOS_NO_VAD
    if (!session_) return;

    try {
        std::copy(context_.begin(), context_.end(), model_input_buf_.begin());
        std::copy(accum_buf_.begin(), accum_buf_.begin() + VAD_CHUNK_SIZE,
                  model_input_buf_.begin() + VAD_CONTEXT_SIZE);

        std::copy(accum_buf_.begin() + VAD_CHUNK_SIZE - VAD_CONTEXT_SIZE,
                  accum_buf_.begin() + VAD_CHUNK_SIZE,
                  context_.begin());

        auto memory_info = Ort::MemoryInfo::CreateCpu(OrtArenaAllocator, OrtMemTypeDefault);

        std::vector<int64_t> audio_shape = {1, VAD_CONTEXT_SIZE + VAD_CHUNK_SIZE};
        Ort::Value audio_tensor = Ort::Value::CreateTensor<float>(
            memory_info, model_input_buf_.data(), model_input_buf_.size(),
            audio_shape.data(), audio_shape.size());

        std::vector<int64_t> state_shape = {2, 1, 128};
        Ort::Value state_tensor = Ort::Value::CreateTensor<float>(
            memory_info, h_state_.data(), h_state_.size(),
            state_shape.data(), state_shape.size());

        int64_t sr_val = sample_rate_;
        std::vector<int64_t> sr_shape = {};
        Ort::Value sr_tensor = Ort::Value::CreateTensor<int64_t>(
            memory_info, &sr_val, 1,
            sr_shape.data(), sr_shape.size());

        const char* input_names[] = {"input", "state", "sr"};
        const char* output_names[] = {"output", "stateN"};

        std::vector<Ort::Value> input_tensors;
        input_tensors.push_back(std::move(audio_tensor));
        input_tensors.push_back(std::move(state_tensor));
        input_tensors.push_back(std::move(sr_tensor));

        auto results = session_->Run(
            Ort::RunOptions{nullptr},
            input_names, input_tensors.data(), input_tensors.size(),
            output_names, 2);

        float prob = results[0].GetTensorData<float>()[0];

        float* state_data = results[1].GetTensorMutableData<float>();
        std::copy(state_data, state_data + VAD_STATE_SIZE, h_state_.begin());

        probability_ = prob;

        bool now_speaking = (prob >= VAD_THRESHOLD);
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
                callback_(now_speaking);
            }
        }
    } catch (const Ort::Exception& e) {
        MELLO_LOG_WARN("vad", "inference error: %s", e.what());
    }
#endif // MELLO_IOS_NO_VAD
}

} // namespace mello::audio
