#include "vad.hpp"
#include "../util/log.hpp"
#include <cstring>
#include <algorithm>
#include <cmath>

#ifdef _WIN32
#include <Windows.h>
#include <filesystem>
#endif

namespace mello::audio {

VoiceActivityDetector::VoiceActivityDetector() = default;

VoiceActivityDetector::~VoiceActivityDetector() {
    shutdown();
}

bool VoiceActivityDetector::initialize(const std::string& model_path) {
    try {
#ifdef _WIN32
        // Windows ships onnxruntime.dll in System32/WinSxS (Copilot, Studio Effects)
        // which shadows ours via the PE loader. Bypass the import table entirely:
        // LoadLibrary our copy by full path and GetProcAddress for OrtGetApiBase.
        {
            auto try_load = [](const std::filesystem::path& p) -> HMODULE {
                HMODULE h = LoadLibraryExW(p.c_str(), nullptr,
                                           LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR |
                                           LOAD_LIBRARY_SEARCH_DEFAULT_DIRS);
                if (!h) h = LoadLibraryW(p.c_str());
                return h;
            };

            // Try next to the model first (production layout), then next to
            // the exe (dev layout — build.rs copies DLLs to target/<profile>/).
            auto model_dir = std::filesystem::path(model_path).parent_path();
            HMODULE h = try_load(model_dir / "onnxruntime.dll");
            if (!h) {
                wchar_t exe_buf[MAX_PATH];
                GetModuleFileNameW(nullptr, exe_buf, MAX_PATH);
                auto exe_dir = std::filesystem::path(exe_buf).parent_path();
                h = try_load(exe_dir / "onnxruntime.dll");
            }
            if (!h) {
                MELLO_LOG_ERROR("vad", "cannot load onnxruntime.dll (err=%lu)", GetLastError());
                return false;
            }
            wchar_t loaded[MAX_PATH];
            GetModuleFileNameW(h, loaded, MAX_PATH);
            MELLO_LOG_INFO("vad", "ORT DLL loaded: %ls", loaded);

            auto get_api_base = reinterpret_cast<decltype(&OrtGetApiBase)>(
                GetProcAddress(h, "OrtGetApiBase"));
            if (!get_api_base) {
                MELLO_LOG_ERROR("vad", "OrtGetApiBase not found in DLL");
                return false;
            }

            const OrtApiBase* api_base = get_api_base();
            MELLO_LOG_INFO("vad", "ORT DLL version=%s (need API %d)",
                           api_base->GetVersionString(), ORT_API_VERSION);

            const OrtApi* api = api_base->GetApi(ORT_API_VERSION);
            if (!api) {
                MELLO_LOG_ERROR("vad", "GetApi(%d) returned null — DLL too old (%s)",
                                ORT_API_VERSION, api_base->GetVersionString());
                return false;
            }
            Ort::InitApi(api);
        }
#else
        const OrtApiBase* api_base = OrtGetApiBase();
        if (!api_base) {
            MELLO_LOG_ERROR("vad", "OrtGetApiBase() returned null");
            return false;
        }
        const OrtApi* api = api_base->GetApi(ORT_API_VERSION);
        if (!api) {
            MELLO_LOG_ERROR("vad", "ORT API version mismatch (need %d, DLL=%s)",
                            ORT_API_VERSION, api_base->GetVersionString());
            return false;
        }
        Ort::InitApi(api);
#endif

        env_ = std::make_unique<Ort::Env>(ORT_LOGGING_LEVEL_WARNING, "mello_vad");
        session_options_ = std::make_unique<Ort::SessionOptions>();
        session_options_->SetIntraOpNumThreads(1);
        session_options_->SetGraphOptimizationLevel(GraphOptimizationLevel::ORT_ENABLE_ALL);

        MELLO_LOG_INFO("vad", "loading model: %s", model_path.c_str());
#ifdef _WIN32
        std::wstring wpath(model_path.begin(), model_path.end());
        session_ = new Ort::Session(*env_, wpath.c_str(), *session_options_);
#else
        session_ = new Ort::Session(*env_, model_path.c_str(), *session_options_);
#endif
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
    session_options_.reset();
    env_.reset();
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
}

} // namespace mello::audio
