#pragma once
#include "audio/audio_pipeline.hpp"
#include "video/video_pipeline.hpp"
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
    video::VideoPipeline& video() { return video_; }

    void set_error(const std::string& error);
    const char* get_error() const;

    bool initialize_inner();

private:
    audio::AudioPipeline audio_;
    video::VideoPipeline video_;
    std::string last_error_;
    mutable std::mutex error_mutex_;
};

} // namespace mello
