#pragma once
#include "audio/audio_pipeline.hpp"
#include <string>
#include <mutex>

namespace mello {

class Context {
public:
    Context() = default;
    ~Context() = default;

    bool initialize();
    void shutdown();

    audio::AudioPipeline& audio() { return audio_; }

    void set_error(const std::string& error);
    const char* get_error() const;

private:
    audio::AudioPipeline audio_;
    std::string last_error_;
    mutable std::mutex error_mutex_;
};

} // namespace mello
