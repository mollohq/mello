#include "ort_loader.hpp"
#include "../util/log.hpp"

#ifndef MELLO_IOS_NO_VAD

#ifdef _WIN32
#include <Windows.h>
#include <filesystem>
#endif

namespace mello::audio {

std::unique_ptr<OrtHandles> init_ort(const std::string& model_path_for_dll_search) {
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
        auto model_dir = std::filesystem::path(model_path_for_dll_search).parent_path();
        HMODULE h = try_load(model_dir / "onnxruntime.dll");
        if (!h) {
            wchar_t exe_buf[MAX_PATH];
            GetModuleFileNameW(nullptr, exe_buf, MAX_PATH);
            auto exe_dir = std::filesystem::path(exe_buf).parent_path();
            h = try_load(exe_dir / "onnxruntime.dll");
        }
        if (!h) {
            MELLO_LOG_ERROR("ort", "cannot load onnxruntime.dll (err=%lu)", GetLastError());
            return nullptr;
        }
        wchar_t loaded[MAX_PATH];
        GetModuleFileNameW(h, loaded, MAX_PATH);
        MELLO_LOG_INFO("ort", "ORT DLL loaded: %ls", loaded);

        auto get_api_base = reinterpret_cast<decltype(&OrtGetApiBase)>(
            GetProcAddress(h, "OrtGetApiBase"));
        if (!get_api_base) {
            MELLO_LOG_ERROR("ort", "OrtGetApiBase not found in DLL");
            return nullptr;
        }

        const OrtApiBase* api_base = get_api_base();
        MELLO_LOG_INFO("ort", "ORT DLL version=%s (need API %d)",
                       api_base->GetVersionString(), ORT_API_VERSION);

        const OrtApi* api = api_base->GetApi(ORT_API_VERSION);
        if (!api) {
            MELLO_LOG_ERROR("ort", "GetApi(%d) returned null — DLL too old (%s)",
                            ORT_API_VERSION, api_base->GetVersionString());
            return nullptr;
        }
        Ort::InitApi(api);
    }
#else
    const OrtApiBase* api_base = OrtGetApiBase();
    if (!api_base) {
        MELLO_LOG_ERROR("ort", "OrtGetApiBase() returned null");
        return nullptr;
    }
    const OrtApi* api = api_base->GetApi(ORT_API_VERSION);
    if (!api) {
        MELLO_LOG_ERROR("ort", "ORT API version mismatch (need %d, DLL=%s)",
                        ORT_API_VERSION, api_base->GetVersionString());
        return nullptr;
    }
    Ort::InitApi(api);
#endif

    auto handles = std::make_unique<OrtHandles>();
    try {
        handles->env = std::make_unique<Ort::Env>(ORT_LOGGING_LEVEL_WARNING, "mello_audio");
        handles->options = std::make_unique<Ort::SessionOptions>();
        handles->options->SetIntraOpNumThreads(1);
        handles->options->SetGraphOptimizationLevel(GraphOptimizationLevel::ORT_ENABLE_ALL);
    } catch (const Ort::Exception& e) {
        MELLO_LOG_ERROR("ort", "session setup failed: %s", e.what());
        return nullptr;
    }
    return handles;
}

Ort::Session* open_ort_session(OrtHandles& handles,
                               const std::string& model_path,
                               const char* tag) {
    try {
        MELLO_LOG_INFO(tag, "loading model: %s", model_path.c_str());
#ifdef _WIN32
        std::wstring wpath(model_path.begin(), model_path.end());
        return new Ort::Session(*handles.env, wpath.c_str(), *handles.options);
#else
        return new Ort::Session(*handles.env, model_path.c_str(), *handles.options);
#endif
    } catch (const Ort::Exception& e) {
        MELLO_LOG_ERROR(tag, "model load failed: %s", e.what());
        return nullptr;
    }
}

}  // namespace mello::audio
#endif  // MELLO_IOS_NO_VAD
