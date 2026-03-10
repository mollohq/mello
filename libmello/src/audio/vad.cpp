#include "vad.hpp"
#include "../util/log.hpp"
#include <cstring>
#include <algorithm>
#include <cmath>

namespace mello::audio {

VoiceActivityDetector::VoiceActivityDetector()
    : env_(ORT_LOGGING_LEVEL_WARNING, "mello_vad")
{
}

VoiceActivityDetector::~VoiceActivityDetector() {
    shutdown();
}

bool VoiceActivityDetector::initialize(const std::string& model_path) {
    try {
        session_options_.SetIntraOpNumThreads(1);
        session_options_.SetGraphOptimizationLevel(GraphOptimizationLevel::ORT_ENABLE_ALL);

#ifdef _WIN32
        std::wstring wpath(model_path.begin(), model_path.end());
        session_ = new Ort::Session(env_, wpath.c_str(), session_options_);
#else
        session_ = new Ort::Session(env_, model_path.c_str(), session_options_);
#endif

        // Log model metadata
        Ort::AllocatorWithDefaultOptions allocator;
        size_t num_in = session_->GetInputCount();
        size_t num_out = session_->GetOutputCount();
        MELLO_LOG_DEBUG("vad", "model inputs=%zu outputs=%zu", num_in, num_out);
        for (size_t i = 0; i < num_in; ++i) {
            auto name = session_->GetInputNameAllocated(i, allocator);
            auto type_info = session_->GetInputTypeInfo(i);
            auto tensor_info = type_info.GetTensorTypeAndShapeInfo();
            auto shape = tensor_info.GetShape();
            auto type = tensor_info.GetElementType();
            std::string shape_str;
            for (auto d : shape) shape_str += std::to_string(d) + ",";
            MELLO_LOG_DEBUG("vad", "  input[%zu] name='%s' shape=[%s] type=%d",
                           i, name.get(), shape_str.c_str(), (int)type);
        }

        h_state_.resize(VAD_STATE_SIZE, 0.0f);
        context_.resize(VAD_CONTEXT_SIZE, 0.0f);
        model_input_buf_.resize(VAD_CONTEXT_SIZE + VAD_CHUNK_SIZE);

        initialized_ = true;
        MELLO_LOG_INFO("vad", "Silero VAD v5 initialized (model=%s)", model_path.c_str());
        return true;
    } catch (const Ort::Exception& e) {
        MELLO_LOG_ERROR("vad", "Silero VAD init failed: %s", e.what());
        return false;
    }
}

void VoiceActivityDetector::shutdown() {
    if (session_) {
        delete session_;
        session_ = nullptr;
    }
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

void VoiceActivityDetector::run_inference() {
    if (!session_) return;

    try {
        // Build model input: [context(64) + chunk(512)] = 576 samples
        std::copy(context_.begin(), context_.end(), model_input_buf_.begin());
        std::copy(accum_buf_.begin(), accum_buf_.begin() + VAD_CHUNK_SIZE,
                  model_input_buf_.begin() + VAD_CONTEXT_SIZE);

        // Save last 64 samples as context for next chunk
        std::copy(accum_buf_.begin() + VAD_CHUNK_SIZE - VAD_CONTEXT_SIZE,
                  accum_buf_.begin() + VAD_CHUNK_SIZE,
                  context_.begin());

        auto memory_info = Ort::MemoryInfo::CreateCpu(OrtArenaAllocator, OrtMemTypeDefault);

        // Input 0: audio [1, 576]
        std::vector<int64_t> audio_shape = {1, VAD_CONTEXT_SIZE + VAD_CHUNK_SIZE};
        Ort::Value audio_tensor = Ort::Value::CreateTensor<float>(
            memory_info, model_input_buf_.data(), model_input_buf_.size(),
            audio_shape.data(), audio_shape.size());

        // Input 1: state [2, 1, 128]
        std::vector<int64_t> state_shape = {2, 1, 128};
        Ort::Value state_tensor = Ort::Value::CreateTensor<float>(
            memory_info, h_state_.data(), h_state_.size(),
            state_shape.data(), state_shape.size());

        // Input 2: sr - scalar (empty shape)
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

        // Copy output state back for next iteration
        float* state_data = results[1].GetTensorMutableData<float>();
        std::copy(state_data, state_data + VAD_STATE_SIZE, h_state_.begin());

        static int dbg_counter = 0;
        if ((dbg_counter++ % 5) == 0) {
            float abs_max = 0, rms = 0;
            for (int i = 0; i < VAD_CHUNK_SIZE; ++i) {
                float a = std::fabs(accum_buf_[i]);
                if (a > abs_max) abs_max = a;
                rms += accum_buf_[i] * accum_buf_[i];
            }
            rms = std::sqrt(rms / VAD_CHUNK_SIZE);
            MELLO_LOG_DEBUG("vad", "prob=%.4f absmax=%.4f rms=%.6f",
                           prob, abs_max, rms);
        }

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
}

} // namespace mello::audio
