#pragma once
// Shared ONNX Runtime bootstrap for the audio DSP stages (Silero VAD,
// neural echo suppressor). One copy of the tricky bits: Windows loads our
// ORT copy by absolute path because a System32/WinSxS copy shadows the
// import table, and every session runs single-threaded with full graph
// optimization. See vad.cpp history for the shadowing incident.
#include <memory>
#include <string>

#ifndef MELLO_IOS_NO_VAD
#define ORT_API_MANUAL_INIT
#include <onnxruntime_cxx_api.h>
#endif

namespace mello::audio {

#ifndef MELLO_IOS_NO_VAD

struct OrtHandles {
    std::unique_ptr<Ort::Env> env;
    std::unique_ptr<Ort::SessionOptions> options;
};

/// Process-wide ORT API init plus default session handles. Returns nullptr
/// (with a log line) when the runtime cannot load.
std::unique_ptr<OrtHandles> init_ort(const std::string& model_path_for_dll_search);

/// Open one model session. Returns nullptr on failure (logs it).
/// `tag` names the stage in logs (e.g. "vad", "echo").
Ort::Session* open_ort_session(OrtHandles& handles,
                               const std::string& model_path,
                               const char* tag);

#endif  // MELLO_IOS_NO_VAD

}  // namespace mello::audio
